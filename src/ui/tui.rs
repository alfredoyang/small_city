//! Panel-based ratatui terminal frontend built only from Game API view models.

use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::core::game::Game;
use crate::interface::input::{BuildingKind, MapOverlayInput};
use crate::interface::view::{
    BuildPreviewView, DemandLevel, GameView, InspectDetailsView, InspectView,
};
use crate::ui::ascii;
use crate::ui::tui_input::{TuiAction, map_key_event};

const DEFAULT_SAVE_FILE: &str = "city1";

/// Local frontend state that is intentionally not stored in the simulation.
///
/// The core owns the city. The TUI owns transient interaction state such as cursor position,
/// selected build tool, modal visibility, and the latest message.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TuiState {
    cursor_x: usize,
    cursor_y: usize,
    selected_build: BuildingKind,
    current_overlay: MapOverlayInput,
    message: String,
    show_help: bool,
    show_overlay_menu: bool,
    prompt: Option<PromptState>,
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            cursor_x: 0,
            cursor_y: 0,
            selected_build: BuildingKind::Residential,
            current_overlay: MapOverlayInput::Normal,
            message: "Tiny City Builder".to_string(),
            show_help: false,
            show_overlay_menu: false,
            prompt: None,
        }
    }
}

impl TuiState {
    /// Moves the cursor while clamping it to the current map dimensions from `GameView`.
    fn move_cursor(&mut self, dx: isize, dy: isize, view: &GameView) {
        let max_x = view.map.width.saturating_sub(1);
        let max_y = view.map.height.saturating_sub(1);
        self.cursor_x = self.cursor_x.saturating_add_signed(dx).min(max_x);
        self.cursor_y = self.cursor_y.saturating_add_signed(dy).min(max_y);
    }

    /// Keeps the cursor valid after operations that may change the loaded map size.
    fn clamp_cursor(&mut self, view: &GameView) {
        self.cursor_x = self.cursor_x.min(view.map.width.saturating_sub(1));
        self.cursor_y = self.cursor_y.min(view.map.height.saturating_sub(1));
    }

    fn reset_cursor(&mut self) {
        self.cursor_x = 0;
        self.cursor_y = 0;
    }
}

/// Temporary text-entry state for save/load filename prompts.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PromptState {
    kind: PromptKind,
    input: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptKind {
    Save,
    Load,
}

/// Runs the ratatui frontend while preserving the public Game API boundary.
pub fn run() -> io::Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    let mut game = Game::default();
    let mut state = TuiState::default();

    loop {
        // Pull all render data through the public API on each frame. This keeps the TUI from
        // caching or reaching into ECS internals.
        let view = game.view_with_overlay(state.current_overlay);
        state.clamp_cursor(&view);
        let inspect = game.inspect(state.cursor_x, state.cursor_y);
        let preview = game.preview_build(state.cursor_x, state.cursor_y, state.selected_build);

        // ratatui redraws the whole frame into an off-screen buffer and then flushes the diff to
        // the terminal. The closure receives a `Frame` that all render functions write into.
        terminal.draw(|frame| render(frame, &view, &inspect, &preview, &state))?;

        // Polling with a timeout lets the UI refresh even when no key is pressed, while avoiding a
        // busy loop that would burn CPU.
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }

        // Crossterm can report mouse, resize, and paste events too. This UI only handles keys.
        let Event::Key(key) = event::read()? else {
            continue;
        };

        // Prompts are modal: while entering a filename, normal gameplay hotkeys are ignored.
        if handle_prompt_key(key, &mut game, &mut state)? {
            continue;
        }

        // The overlay selector is also modal, but only for keys it understands.
        if state.show_overlay_menu && handle_overlay_menu_key(key, &mut state) {
            continue;
        }

        // Non-modal keys are normalized into actions before mutating UI state or calling Game APIs.
        let action = map_key_event(key);
        match action {
            TuiAction::MoveUp => state.move_cursor(0, -1, &view),
            TuiAction::MoveDown => state.move_cursor(0, 1, &view),
            TuiAction::MoveLeft => state.move_cursor(-1, 0, &view),
            TuiAction::MoveRight => state.move_cursor(1, 0, &view),
            TuiAction::SelectBuild(kind) => {
                state.selected_build = kind;
                state.message = format!("Selected {}", kind.label());
            }
            TuiAction::Build => {
                state.message = game
                    .build(state.cursor_x, state.cursor_y, state.selected_build)
                    .message();
            }
            TuiAction::Replace => {
                state.message = game
                    .replace(state.cursor_x, state.cursor_y, state.selected_build)
                    .message();
            }
            TuiAction::Upgrade => {
                state.message = game.upgrade(state.cursor_x, state.cursor_y).message();
            }
            TuiAction::Bulldoze => {
                state.message = game.bulldoze(state.cursor_x, state.cursor_y).message();
            }
            TuiAction::Tick => {
                state.message = game.tick().message();
            }
            TuiAction::Save => {
                state.prompt = Some(PromptState {
                    kind: PromptKind::Save,
                    input: String::new(),
                });
            }
            TuiAction::Load => {
                state.prompt = Some(PromptState {
                    kind: PromptKind::Load,
                    input: String::new(),
                });
            }
            TuiAction::ToggleHelp => state.show_help = !state.show_help,
            TuiAction::ToggleOverlayMenu => state.show_overlay_menu = !state.show_overlay_menu,
            TuiAction::Quit => return Ok(()),
            TuiAction::None => {}
        }
    }
}

fn handle_prompt_key(key: KeyEvent, game: &mut Game, state: &mut TuiState) -> io::Result<bool> {
    let Some(prompt) = state.prompt.as_mut() else {
        return Ok(false);
    };

    // Returning `true` means the key was consumed by the prompt and should not also trigger a
    // normal gameplay action.
    match key.code {
        KeyCode::Esc => {
            state.prompt = None;
            state.message = "Cancelled prompt".to_string();
        }
        KeyCode::Backspace => {
            prompt.input.pop();
        }
        KeyCode::Enter => {
            let prompt = state.prompt.take().expect("prompt exists");
            let filename = if prompt.input.trim().is_empty() {
                DEFAULT_SAVE_FILE.to_string()
            } else {
                prompt.input.trim().to_string()
            };

            match prompt.kind {
                PromptKind::Save => {
                    state.message = match game.save_to_file(&filename) {
                        Ok(()) => format!("Saved {filename}"),
                        Err(error) => error.to_string(),
                    };
                }
                PromptKind::Load => {
                    state.message = match Game::load_from_file(&filename) {
                        Ok(loaded_game) => {
                            *game = loaded_game;
                            // Loading swaps in a new game state, so the cursor is reset and then
                            // clamped against the loaded map dimensions.
                            state.reset_cursor();
                            let view = game.view_with_overlay(state.current_overlay);
                            state.clamp_cursor(&view);
                            format!("Loaded {filename}")
                        }
                        Err(error) => error.to_string(),
                    };
                }
            }
        }
        KeyCode::Char(value) => prompt.input.push(value),
        _ => {}
    }

    Ok(true)
}

fn handle_overlay_menu_key(key: KeyEvent, state: &mut TuiState) -> bool {
    // The overlay menu uses number keys for overlay choices. It consumes only those keys plus
    // close keys; other keys fall through to normal action handling.
    match key.code {
        KeyCode::Esc | KeyCode::Char('o') | KeyCode::Char('O') => {
            state.show_overlay_menu = false;
            true
        }
        KeyCode::Char('1') => select_overlay(state, MapOverlayInput::Normal),
        KeyCode::Char('2') => select_overlay(state, MapOverlayInput::Power),
        KeyCode::Char('3') => select_overlay(state, MapOverlayInput::Pollution),
        KeyCode::Char('4') => select_overlay(state, MapOverlayInput::Population),
        KeyCode::Char('5') => select_overlay(state, MapOverlayInput::LandValue),
        KeyCode::Char('6') => select_overlay(state, MapOverlayInput::Desirability),
        _ => false,
    }
}

fn select_overlay(state: &mut TuiState, overlay: MapOverlayInput) -> bool {
    state.current_overlay = overlay;
    state.show_overlay_menu = false;
    state.message = format!("Overlay: {}", overlay_label(overlay));
    true
}

fn render(
    frame: &mut Frame<'_>,
    view: &GameView,
    inspect: &InspectView,
    preview: &BuildPreviewView,
    state: &TuiState,
) {
    let root = frame.area();
    // The screen is split into three horizontal bands. Each band is then split into panels.
    // `Constraint::Min` gives the map the flexible space; fixed-height lower panels stay readable.
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(12),
            Constraint::Length(8),
            Constraint::Length(5),
        ])
        .split(root);
    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(vertical[0]);
    let middle = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(vertical[1]);

    render_map(frame, top[0], view, state);
    render_selected_cell(frame, top[1], inspect);
    render_status(frame, middle[0], view);
    render_build_preview(frame, middle[1], view, preview, state);
    render_messages(frame, vertical[2], state);

    // Modal panels are rendered after the base layout so they appear on top. `Clear` inside the
    // modal renderers blanks the covered area before drawing the popup border and text.
    if state.show_help {
        render_help(frame, root);
    }
    if state.show_overlay_menu {
        render_overlay_menu(frame, root, state.current_overlay);
    }
    if let Some(prompt) = &state.prompt {
        render_prompt(frame, root, prompt);
    }
}

fn render_map(frame: &mut Frame<'_>, area: Rect, view: &GameView, state: &TuiState) {
    let mut lines = Vec::new();
    lines.push(Line::from(format!(
        "Overlay: {} | {}",
        overlay_label(state.current_overlay),
        overlay_legend(state.current_overlay)
    )));

    let mut header = vec![Span::raw("   ")];
    for x in 0..view.map.width {
        header.push(Span::raw(format!("{x:^3}")));
    }
    lines.push(Line::from(header));

    for y in 0..view.map.height {
        let mut row = vec![Span::raw(format!("{y:>2} "))];
        for x in 0..view.map.width {
            let index = y * view.map.width + x;
            let symbol = view.map.cells[index].symbol;
            // The cursor is styling, not extra text. Every cell stays exactly three characters
            // wide, so moving the cursor cannot shift map columns.
            let style = if x == state.cursor_x && y == state.cursor_y {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                cell_style(symbol, state.current_overlay)
            };
            row.push(Span::styled(format!(" {symbol} "), style));
        }
        lines.push(Line::from(row));
    }

    frame.render_widget(
        // A Paragraph is enough here because the map is already formatted into line spans. Ratatui
        // handles clipping if the terminal is smaller than the full map.
        Paragraph::new(lines)
            .block(Block::default().title("City Map").borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_selected_cell(frame: &mut Frame<'_>, area: Rect, inspect: &InspectView) {
    // Reuse the ASCII inspect formatter so both terminal frontends describe cells consistently.
    let mut lines = vec![Line::from(ascii::format_inspect(inspect))];

    if let Some(effects) = inspect.local_effects {
        lines.push(Line::from(format!(
            "Land {} | Pollution {} | Access {} | Desirability {}",
            effects.land_value,
            effects.pollution_pressure,
            effects.accessibility,
            effects.desirability
        )));
    }

    if !inspect.explanations.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from("Inspect Notes:"));
        lines.extend(
            inspect
                .explanations
                .iter()
                .map(|note| Line::from(note.as_str())),
        );
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title("Selected Cell")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_status(frame: &mut Frame<'_>, area: Rect, view: &GameView) {
    let status = &view.status;
    let lines = vec![
        Line::from(format!(
            "Turn: {} | Money: ${} | Pop: {} | Citizens: {}",
            status.turn, status.money, status.population, status.citizens
        )),
        Line::from(format!(
            "Jobs: {} | Unemployed: {} | Happiness: {} | Pollution: {}",
            status.jobs, status.unemployment, status.happiness, status.pollution
        )),
        Line::from(format!(
            "Power: {}/{} supplied | Demand: {} | Shortage: {}",
            status.power.total_supplied,
            status.power.total_capacity,
            status.power.total_demand,
            status.power.total_shortage
        )),
        Line::from(format!(
            "Demand: R {} | C {} | I {}",
            demand_label(status.demand.residential),
            demand_label(status.demand.commercial),
            demand_label(status.demand.industrial)
        )),
        Line::from(format!(
            "Demand Notes: R {} | C {} | I {}",
            demand_note(BuildingKind::Residential, status.demand.residential),
            demand_note(BuildingKind::Commercial, status.demand.commercial),
            demand_note(BuildingKind::Industrial, status.demand.industrial)
        )),
    ];

    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title("Status").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_build_preview(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &GameView,
    preview: &BuildPreviewView,
    state: &TuiState,
) {
    let can_build = if preview.can_build { "Yes" } else { "No" };
    let mut lines = vec![
        Line::from(vec![
            Span::raw("Tool: "),
            Span::styled(
                state.selected_build.label(),
                building_style(state.selected_build).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(format!(
            "Cost: ${} | Upkeep: ${}",
            selected_build_cost(view, state.selected_build),
            selected_build_maintenance_cost(view, state.selected_build)
        )),
        Line::from(format!("Can Build: {can_build}")),
    ];

    if let Some(reason) = &preview.reason {
        lines.push(Line::from(format!("Reason: {reason}")));
    }
    if !preview.effects.is_empty() {
        lines.push(Line::from("Effects:"));
        lines.extend(
            preview
                .effects
                .iter()
                .map(|effect| Line::from(effect.as_str())),
        );
    }
    lines.push(Line::from(
        "Actions: B Build | R Replace | U Upgrade | X Bulldoze",
    ));

    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title("Build / Actions")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_messages(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let lines = vec![
        Line::from(state.message.as_str()),
        Line::from("WASD/Arrows move | 1-6 tools | N next | O overlays | H help | Q quit"),
    ];

    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title("Messages / Tick Summary")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_help(frame: &mut Frame<'_>, area: Rect) {
    let popup = centered_rect(70, 70, area);
    let lines = vec![
        Line::from("Movement: WASD or arrow keys"),
        Line::from(
            "Build tools: 1 Road | 2 Residential | 3 Commercial | 4 Industrial | 5 Power | 6 Park",
        ),
        Line::from("Actions: B/Enter Build | R Replace | U Upgrade | X Bulldoze | N Next Turn"),
        Line::from("Files: S Save | L Load | Enter at prompt uses city1"),
        Line::from("Views: O Overlay Menu | H Close Help | Q Quit"),
        Line::from("The TUI renders only GameView, InspectView, and BuildPreviewView data."),
    ];
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title("Help").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        popup,
    );
}

fn render_overlay_menu(frame: &mut Frame<'_>, area: Rect, current: MapOverlayInput) {
    let popup = centered_rect(50, 45, area);
    let lines = [
        MapOverlayInput::Normal,
        MapOverlayInput::Power,
        MapOverlayInput::Pollution,
        MapOverlayInput::Population,
        MapOverlayInput::LandValue,
        MapOverlayInput::Desirability,
    ]
    .iter()
    .enumerate()
    .map(|(index, overlay)| {
        let marker = if *overlay == current { "*" } else { " " };
        Line::from(format!(
            "{} {}. {} - {}",
            marker,
            index + 1,
            overlay_label(*overlay),
            overlay_legend(*overlay)
        ))
    })
    .collect::<Vec<_>>();

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title("Overlay Selector")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: true }),
        popup,
    );
}

fn render_prompt(frame: &mut Frame<'_>, area: Rect, prompt: &PromptState) {
    let popup = centered_rect(45, 20, area);
    let label = match prompt.kind {
        PromptKind::Save => "Save filename",
        PromptKind::Load => "Load filename",
    };
    let input = if prompt.input.is_empty() {
        // Showing the default while the input is empty makes the Enter behavior visible.
        DEFAULT_SAVE_FILE
    } else {
        prompt.input.as_str()
    };
    let lines = vec![
        Line::from(format!("{label}: {input}")),
        Line::from("Enter confirms | Esc cancels"),
    ];

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title(label).borders(Borders::ALL))
            .alignment(Alignment::Left),
        popup,
    );
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    // A centered popup is made by splitting the available area into three columns/rows and taking
    // the middle rectangle.
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn cell_style(symbol: char, overlay: MapOverlayInput) -> Style {
    // Color is purely presentation. The meaning still comes from GameView cell symbols and overlay
    // data generated by the interface adapter.
    match overlay {
        MapOverlayInput::Normal => match symbol {
            '=' => Style::default().fg(Color::Gray),
            'R' => Style::default().fg(Color::Green),
            'C' => Style::default().fg(Color::Blue),
            'I' => Style::default().fg(Color::Yellow),
            'T' => Style::default().fg(Color::Red),
            'P' => Style::default().fg(Color::LightGreen),
            _ => Style::default(),
        },
        MapOverlayInput::Power => match symbol {
            '*' | '+' | 'P' => Style::default().fg(Color::Yellow),
            '-' => Style::default().fg(Color::Red),
            _ => Style::default(),
        },
        MapOverlayInput::Pollution => Style::default().fg(Color::Red),
        MapOverlayInput::Population => Style::default().fg(Color::Green),
        MapOverlayInput::LandValue => Style::default().fg(Color::Cyan),
        MapOverlayInput::Desirability => Style::default().fg(Color::Magenta),
    }
}

fn building_style(kind: BuildingKind) -> Style {
    match kind {
        BuildingKind::Road => Style::default().fg(Color::Gray),
        BuildingKind::Residential => Style::default().fg(Color::Green),
        BuildingKind::Commercial => Style::default().fg(Color::Blue),
        BuildingKind::Industrial => Style::default().fg(Color::Yellow),
        BuildingKind::PowerPlant => Style::default().fg(Color::Red),
        BuildingKind::Park => Style::default().fg(Color::LightGreen),
    }
}

fn selected_build_cost(view: &GameView, selected_build: BuildingKind) -> i32 {
    view.build_options
        .iter()
        .find(|option| option.kind == selected_build)
        .map(|option| option.cost)
        .unwrap_or_else(|| selected_build.cost())
}

fn selected_build_maintenance_cost(view: &GameView, selected_build: BuildingKind) -> i32 {
    view.build_options
        .iter()
        .find(|option| option.kind == selected_build)
        .map(|option| option.maintenance_cost)
        .unwrap_or_else(|| selected_build.maintenance_cost())
}

fn overlay_legend(overlay: MapOverlayInput) -> &'static str {
    match overlay {
        MapOverlayInput::Normal => {
            ". empty | = road | R residential | C commercial | I industrial | T power | P park"
        }
        MapOverlayInput::Power => {
            "P plant | * powered road | + powered building | - unpowered building | . none"
        }
        MapOverlayInput::Pollution => "0-9 pollution level | . none",
        MapOverlayInput::Population => "0-9 population | . none",
        MapOverlayInput::LandValue => "0-9 land value | higher is better",
        MapOverlayInput::Desirability => "0-9 desirability | high grows faster, low blocks growth",
    }
}

fn overlay_label(overlay: MapOverlayInput) -> &'static str {
    match overlay {
        MapOverlayInput::Normal => "Normal",
        MapOverlayInput::Power => "Power",
        MapOverlayInput::Pollution => "Pollution",
        MapOverlayInput::Population => "Population",
        MapOverlayInput::LandValue => "Land Value",
        MapOverlayInput::Desirability => "Desirability",
    }
}

fn demand_label(level: DemandLevel) -> &'static str {
    match level {
        DemandLevel::Low => "Low",
        DemandLevel::Medium => "Medium",
        DemandLevel::High => "High",
    }
}

fn demand_note(kind: BuildingKind, level: DemandLevel) -> &'static str {
    match (kind, level) {
        (BuildingKind::Residential, DemandLevel::High) => "High: jobs and happiness support growth",
        (BuildingKind::Residential, DemandLevel::Medium) => "Medium: some room for growth",
        (BuildingKind::Residential, DemandLevel::Low) => "Low: add jobs or improve happiness",
        (BuildingKind::Commercial, DemandLevel::High) => "High: residents can support more shops",
        (BuildingKind::Commercial, DemandLevel::Medium) => "Medium: shops are near balance",
        (BuildingKind::Commercial, DemandLevel::Low) => "Low: grow population first",
        (BuildingKind::Industrial, DemandLevel::High) => "High: unemployed residents need jobs",
        (BuildingKind::Industrial, DemandLevel::Medium) => "Medium: industry is near balance",
        (BuildingKind::Industrial, DemandLevel::Low) => "Low: jobs or pollution are limiting",
        _ => "",
    }
}

#[allow(dead_code)]
fn inspect_title(inspect: &InspectView) -> &'static str {
    match inspect.details {
        Some(InspectDetailsView::Empty { .. }) => "Empty Land",
        Some(InspectDetailsView::Road) => "Road",
        Some(InspectDetailsView::Residential { .. }) => "Residential",
        Some(InspectDetailsView::Commercial { .. }) => "Commercial",
        Some(InspectDetailsView::Industrial { .. }) => "Industrial",
        Some(InspectDetailsView::PowerPlant { .. }) => "Power Plant",
        Some(InspectDetailsView::Park { .. }) => "Park",
        Some(InspectDetailsView::Unknown) | None => "Selected Cell",
    }
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    /// Switches the terminal into the mode ratatui expects.
    ///
    /// Raw mode lets crossterm receive key presses immediately instead of waiting for Enter.
    /// The alternate screen gives the app a full-screen drawing surface without overwriting the
    /// user's shell scrollback. `CrosstermBackend` is the adapter that lets ratatui draw through
    /// crossterm.
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }

    /// Draws one frame. Ratatui owns the buffering; callers only describe widgets for this frame.
    fn draw(&mut self, render: impl FnOnce(&mut Frame<'_>)) -> io::Result<()> {
        self.terminal.draw(render).map(|_| ())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Restore terminal state even if the event loop exits early because of an error.
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}
