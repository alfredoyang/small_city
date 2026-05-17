//! Panel-based ratatui terminal frontend built only from Game API view models.

use std::io::{self, Stdout};
use std::time::{Duration, Instant};

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
const AUTO_TICK_INTERVAL: Duration = Duration::from_secs(1);
const PAUSED_POLL_TIMEOUT: Duration = Duration::from_secs(3600);

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
    is_running: bool,
    show_help: bool,
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
            is_running: false,
            show_help: false,
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

    fn cycle_overlay(&mut self) {
        self.current_overlay = match self.current_overlay {
            MapOverlayInput::Normal => MapOverlayInput::Power,
            MapOverlayInput::Power => MapOverlayInput::Pollution,
            MapOverlayInput::Pollution => MapOverlayInput::Population,
            MapOverlayInput::Population => MapOverlayInput::LandValue,
            MapOverlayInput::LandValue => MapOverlayInput::Desirability,
            MapOverlayInput::Desirability => MapOverlayInput::Normal,
        };
        self.message = format!("Overlay: {}", overlay_label(self.current_overlay));
    }

    fn toggle_run(&mut self) {
        self.is_running = !self.is_running;
        self.message = if self.is_running {
            "Simulation running: auto tick every 1 second".to_string()
        } else {
            "Simulation paused".to_string()
        };
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
    let mut next_auto_tick = Instant::now() + AUTO_TICK_INTERVAL;
    let mut dirty = true;

    loop {
        if state.is_running && Instant::now() >= next_auto_tick {
            state.message = game.tick().message();
            next_auto_tick = Instant::now() + AUTO_TICK_INTERVAL;
            dirty = true;
        }

        if dirty {
            // Pull all render data through the public API only when the screen needs repainting.
            // This keeps the TUI from caching or reaching into ECS internals while avoiding idle
            // redraw work.
            let view = game.view_with_overlay(state.current_overlay);
            state.clamp_cursor(&view);
            let inspect = game.inspect(state.cursor_x, state.cursor_y);
            let preview = game.preview_build(state.cursor_x, state.cursor_y, state.selected_build);

            // ratatui redraws the whole frame into an off-screen buffer and then flushes the diff
            // to the terminal. The closure receives a `Frame` that all render functions write into.
            terminal.draw(|frame| render(frame, &view, &inspect, &preview, &state))?;
            dirty = false;
        }

        // Sleep until input arrives or, in running mode, until the next scheduled auto tick is due.
        if !event::poll(poll_timeout(
            state.is_running,
            next_auto_tick,
            Instant::now(),
        ))? {
            continue;
        }

        // Crossterm can report mouse, resize, and paste events too. Resize changes should repaint.
        let Event::Key(key) = event::read()? else {
            dirty = true;
            continue;
        };

        // Prompts are modal: while entering a filename, normal gameplay hotkeys are ignored.
        if handle_prompt_key(key, &mut game, &mut state)? {
            dirty = true;
            continue;
        }

        // Non-modal keys are normalized into actions before mutating UI state or calling Game APIs.
        let action = map_key_event(key);
        match action {
            TuiAction::MoveUp => move_cursor_with_current_view(&game, &mut state, 0, -1),
            TuiAction::MoveDown => move_cursor_with_current_view(&game, &mut state, 0, 1),
            TuiAction::MoveLeft => move_cursor_with_current_view(&game, &mut state, -1, 0),
            TuiAction::MoveRight => move_cursor_with_current_view(&game, &mut state, 1, 0),
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
                manual_tick(&mut game, &mut state);
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
            TuiAction::CycleOverlay => state.cycle_overlay(),
            TuiAction::ToggleRun => {
                state.toggle_run();
                next_auto_tick = Instant::now() + AUTO_TICK_INTERVAL;
            }
            TuiAction::Quit => return Ok(()),
            TuiAction::None => continue,
        }
        dirty = true;
    }
}

fn move_cursor_with_current_view(game: &Game, state: &mut TuiState, dx: isize, dy: isize) {
    let view = game.view_with_overlay(state.current_overlay);
    state.move_cursor(dx, dy, &view);
}

fn poll_timeout(is_running: bool, next_auto_tick: Instant, now: Instant) -> Duration {
    if is_running {
        next_auto_tick.saturating_duration_since(now)
    } else {
        PAUSED_POLL_TIMEOUT
    }
}

fn manual_tick(game: &mut Game, state: &mut TuiState) {
    if state.is_running {
        state.message = "Pause before using manual next turn".to_string();
        return;
    }

    state.message = game.tick().message();
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
                            state.is_running = false;
                            // Loading swaps in a new game state, so the cursor is reset and then
                            // clamped against the loaded map dimensions.
                            state.reset_cursor();
                            let view = game.view_with_overlay(state.current_overlay);
                            state.clamp_cursor(&view);
                            format!("Loaded {filename}; simulation paused")
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
    render_status(frame, middle[0], view, state);
    render_build_preview(frame, middle[1], view, preview, state);
    render_messages(frame, vertical[2], state);

    // Modal panels are rendered after the base layout so they appear on top. `Clear` inside the
    // modal renderers blanks the covered area before drawing the popup border and text.
    if state.show_help {
        render_help(frame, root);
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

fn render_status(frame: &mut Frame<'_>, area: Rect, view: &GameView, state: &TuiState) {
    let status = &view.status;
    let lines = vec![
        simulation_status_line(state.is_running),
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
        simulation_status_line(state.is_running),
        Line::from(state.message.as_str()),
        Line::from(
            "Space pause/resume | WASD/Arrows move | 1-6 tools | N next | O overlay | H help | Q quit",
        ),
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
    let popup = centered_rect(82, 100, area);
    let lines = vec![
        help_section("Movement"),
        Line::from("  WASD / Arrow Keys    Move cursor"),
        Line::from(""),
        help_section("Build Tools"),
        Line::from("  1 Road        2 Residential     3 Commercial"),
        Line::from("  4 Industrial  5 Power Plant     6 Park"),
        Line::from(""),
        help_section("Actions"),
        Line::from("  Space         Pause / resume automatic ticks"),
        Line::from("  B / Enter     Build selected tool"),
        Line::from("  R             Replace selected cell with selected tool"),
        Line::from("  U             Upgrade selected cell"),
        Line::from("  X             Bulldoze selected cell"),
        Line::from("  N             Next turn"),
        Line::from(""),
        help_section("Files And UI"),
        Line::from("  S             Save city"),
        Line::from("  L             Load city"),
        Line::from("  H             Close Help"),
        Line::from("  Q             Quit"),
        Line::from("  Enter at save/load prompt uses city1"),
        Line::from(""),
        help_section("Overlays"),
        Line::from("  O Cycle Overlay"),
        Line::from(
            "Overlay order: Normal -> Power -> Pollution -> Population -> Land Value -> Desirability",
        ),
        Line::from(format!(
            "Normal: {}",
            overlay_legend(MapOverlayInput::Normal)
        )),
        Line::from(format!("Power: {}", overlay_legend(MapOverlayInput::Power))),
        Line::from(format!(
            "Pollution: {}",
            overlay_legend(MapOverlayInput::Pollution)
        )),
        Line::from(format!(
            "Population: {}",
            overlay_legend(MapOverlayInput::Population)
        )),
        Line::from(format!(
            "Land Value: {}",
            overlay_legend(MapOverlayInput::LandValue)
        )),
        Line::from(format!(
            "Desirability: {}",
            overlay_legend(MapOverlayInput::Desirability)
        )),
        Line::from(""),
        help_section("Boundary"),
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

fn help_section(label: &'static str) -> Line<'static> {
    Line::from(Span::styled(
        label,
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ))
}

fn simulation_status_line(is_running: bool) -> Line<'static> {
    let (label, color, detail) = if is_running {
        ("RUNNING", Color::Green, "auto tick every 1 second")
    } else {
        ("PAUSED", Color::Yellow, "press Space to resume")
    };

    Line::from(vec![
        Span::raw("Simulation: "),
        Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" | {detail}")),
    ])
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn overlay_cycles_in_display_order() {
        let mut state = TuiState::default();

        state.cycle_overlay();
        assert_eq!(state.current_overlay, MapOverlayInput::Power);
        assert_eq!(state.message, "Overlay: Power");

        state.cycle_overlay();
        assert_eq!(state.current_overlay, MapOverlayInput::Pollution);
        state.cycle_overlay();
        assert_eq!(state.current_overlay, MapOverlayInput::Population);
        state.cycle_overlay();
        assert_eq!(state.current_overlay, MapOverlayInput::LandValue);
        state.cycle_overlay();
        assert_eq!(state.current_overlay, MapOverlayInput::Desirability);
        state.cycle_overlay();
        assert_eq!(state.current_overlay, MapOverlayInput::Normal);
    }

    #[test]
    fn run_state_defaults_to_paused_and_toggles() {
        let mut state = TuiState::default();

        assert!(!state.is_running);
        state.toggle_run();
        assert!(state.is_running);
        assert_eq!(
            state.message,
            "Simulation running: auto tick every 1 second"
        );
        state.toggle_run();
        assert!(!state.is_running);
        assert_eq!(state.message, "Simulation paused");
    }

    #[test]
    fn manual_tick_advances_only_when_paused() {
        let mut game = Game::new(10, 10);
        let mut state = TuiState::default();

        manual_tick(&mut game, &mut state);

        assert_eq!(game.view().status.turn, 1);
        assert!(state.message.contains("Advanced to turn 1"));
    }

    #[test]
    fn manual_tick_is_blocked_while_running() {
        let mut game = Game::new(10, 10);
        let mut state = TuiState {
            is_running: true,
            ..TuiState::default()
        };

        manual_tick(&mut game, &mut state);

        assert_eq!(game.view().status.turn, 0);
        assert_eq!(state.message, "Pause before using manual next turn");
    }

    #[test]
    fn paused_mode_uses_long_poll_timeout() {
        let now = Instant::now();

        assert_eq!(
            poll_timeout(false, now + AUTO_TICK_INTERVAL, now),
            PAUSED_POLL_TIMEOUT
        );
    }

    #[test]
    fn running_mode_polls_until_next_tick_deadline() {
        let now = Instant::now();

        assert_eq!(
            poll_timeout(true, now + Duration::from_millis(250), now),
            Duration::from_millis(250)
        );
        assert_eq!(
            poll_timeout(true, now - Duration::from_millis(1), now),
            Duration::ZERO
        );
    }

    #[test]
    fn render_draws_expected_main_panels() {
        let game = Game::new(10, 10);
        let output = render_test_screen(&game, TuiState::default());

        for expected in [
            "City Map",
            "Selected Cell",
            "Status",
            "Build / Actions",
            "Messages / Tick Summary",
            "Overlay: Normal",
            "Tool: Residential",
            "Simulation: PAUSED",
            "press Space to resume",
        ] {
            assert!(
                output.contains(expected),
                "expected TUI output to contain {expected:?}\n{output}"
            );
        }
    }

    #[test]
    fn help_panel_contains_overlay_order_and_legends() {
        let game = Game::new(10, 10);
        let state = TuiState {
            show_help: true,
            ..TuiState::default()
        };

        let output = render_test_screen(&game, state);

        assert!(output.contains("Help"));
        assert!(output.contains("Space         Pause / resume automatic ticks"));
        assert!(output.contains("O Cycle Overlay"));
        assert!(output.contains("Overlay order: Normal -> Power -> Pollution"));
        assert!(output.contains("Normal: . empty"));
        assert!(output.contains("Desirability: 0-9 desirability"));
    }

    #[test]
    fn render_shows_running_status_when_auto_tick_is_enabled() {
        let game = Game::new(10, 10);
        let state = TuiState {
            is_running: true,
            ..TuiState::default()
        };

        let output = render_test_screen(&game, state);

        assert!(output.contains("Simulation: RUNNING"));
        assert!(output.contains("auto tick every 1 second"));
    }

    #[test]
    fn selected_build_tool_uses_building_color() {
        let game = Game::new(10, 10);
        let state = TuiState {
            selected_build: BuildingKind::Industrial,
            ..TuiState::default()
        };

        let terminal = render_test_terminal(&game, state);
        let buffer = terminal.backend().buffer();
        let cell = find_first_text_cell(buffer, "Industrial").expect("styled Industrial text");

        assert_eq!(cell.fg, Color::Yellow);
        assert!(cell.modifier.contains(Modifier::BOLD));
    }

    fn render_test_screen(game: &Game, state: TuiState) -> String {
        let terminal = render_test_terminal(game, state);
        buffer_text(terminal.backend().buffer())
    }

    fn render_test_terminal(game: &Game, mut state: TuiState) -> Terminal<TestBackend> {
        let view = game.view_with_overlay(state.current_overlay);
        state.clamp_cursor(&view);
        let inspect = game.inspect(state.cursor_x, state.cursor_y);
        let preview = game.preview_build(state.cursor_x, state.cursor_y, state.selected_build);

        let backend = TestBackend::new(120, 36);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &view, &inspect, &preview, &state))
            .expect("render TUI frame");
        terminal
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        let area = buffer.area();
        let width = area.width as usize;
        let mut text = String::new();

        for row in buffer.content().chunks(width) {
            for cell in row {
                text.push_str(cell.symbol());
            }
            text.push('\n');
        }

        text
    }

    fn find_first_text_cell<'a>(
        buffer: &'a ratatui::buffer::Buffer,
        needle: &str,
    ) -> Option<&'a ratatui::buffer::Cell> {
        let area = buffer.area();
        let width = area.width as usize;

        for row in buffer.content().chunks(width) {
            let mut line = String::new();
            for cell in row {
                line.push_str(cell.symbol());
            }

            if let Some(x) = line.find(needle) {
                return row.get(x);
            }
        }

        None
    }
}
