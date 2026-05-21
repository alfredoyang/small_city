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
    BuildPreviewView, CellView, DemandLevel, GameView, InspectDetailsView, InspectView,
};
use crate::ui::ascii;
use crate::ui::tui_input::{TuiAction, map_key_event};

const DEFAULT_SAVE_FILE: &str = "city1";
const AUTO_TICK_INTERVAL: Duration = Duration::from_secs(1);
const PAUSED_POLL_TIMEOUT: Duration = Duration::from_secs(3600);
const MIN_TUI_WIDTH: u16 = 100;
const MIN_TUI_HEIGHT: u16 = 30;

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
    tile_theme: TileTheme,
    message: String,
    is_running: bool,
    run_speed: RunSpeed,
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
            tile_theme: TileTheme::AsciiDetailed,
            message: "Tiny City Builder".to_string(),
            is_running: false,
            run_speed: RunSpeed::One,
            show_help: false,
            prompt: None,
        }
    }
}

/// UI-only fast-forward speed. The core simulation still advances by repeated `Game::tick()` calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunSpeed {
    One,
    Two,
    Four,
}

impl RunSpeed {
    fn label(self) -> &'static str {
        match self {
            Self::One => "1x",
            Self::Two => "2x",
            Self::Four => "4x",
        }
    }

    fn interval(self) -> Duration {
        match self {
            Self::One => AUTO_TICK_INTERVAL,
            Self::Two => Duration::from_millis(500),
            Self::Four => Duration::from_millis(250),
        }
    }

    fn faster(self) -> Self {
        match self {
            Self::One => Self::Two,
            Self::Two | Self::Four => Self::Four,
        }
    }

    fn slower(self) -> Self {
        match self {
            Self::One | Self::Two => Self::One,
            Self::Four => Self::Two,
        }
    }
}

/// UI-only map theme. It converts safe interface view models into fixed-width terminal tiles.
///
/// `AsciiDetailed` is the default because every tile is exactly two ASCII characters, which keeps
/// map rows aligned across terminals. `AsciiCompact` preserves the older one-character language
/// inside a two-character tile. `Unicode` remains optional and is not the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TileTheme {
    AsciiCompact,
    AsciiDetailed,
    Unicode,
}

impl TileTheme {
    fn tile_for_cell(
        self,
        cell: &CellView,
        overlay: MapOverlayInput,
        is_cursor: bool,
        preview_state: PreviewState,
    ) -> TileGlyph {
        let mut glyph = match overlay {
            MapOverlayInput::Normal => self.normal_tile(cell),
            MapOverlayInput::Power => self.power_tile(cell),
            MapOverlayInput::Pollution => self.intensity_tile(
                cell.local_effects.pollution_pressure,
                IntensityKind::Pollution,
            ),
            MapOverlayInput::Population => self.population_tile(cell),
            MapOverlayInput::LandValue => {
                self.intensity_tile(cell.local_effects.land_value, IntensityKind::LandValue)
            }
            MapOverlayInput::Desirability => {
                self.intensity_tile(cell.local_effects.desirability, IntensityKind::Desirability)
            }
        };

        if preview_state != PreviewState::None {
            glyph.style = match preview_state {
                PreviewState::Valid => glyph.style.fg(Color::Green),
                PreviewState::Invalid => glyph.style.fg(Color::Red),
                PreviewState::None => glyph.style,
            }
            .add_modifier(Modifier::BOLD);
        }

        if is_cursor {
            glyph.style = glyph
                .style
                .add_modifier(Modifier::REVERSED | Modifier::BOLD);
        }

        glyph
    }

    fn normal_tile(self, cell: &CellView) -> TileGlyph {
        let tile = match self {
            TileTheme::AsciiCompact => format!("{}.", cell.symbol),
            TileTheme::AsciiDetailed => ascii_detailed_normal_tile(cell),
            TileTheme::Unicode => unicode_normal_tile(cell),
        };
        TileGlyph {
            tile,
            style: cell_base_style(cell),
        }
    }

    fn power_tile(self, cell: &CellView) -> TileGlyph {
        let tile = match cell.symbol {
            'P' => "T*".to_string(),
            '*' => "=*".to_string(),
            '+' => format!("{}+", tile_type(cell)),
            '-' => format!("{}-", tile_type(cell)),
            _ => "..".to_string(),
        };
        let style = match cell.symbol {
            'P' | '*' | '+' => Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            '-' => problem_style(),
            _ => empty_style(),
        };

        match self {
            TileTheme::Unicode => TileGlyph {
                tile: tile.replace('T', "ϟ"),
                style,
            },
            TileTheme::AsciiCompact | TileTheme::AsciiDetailed => TileGlyph { tile, style },
        }
    }

    fn intensity_tile(self, value: i32, kind: IntensityKind) -> TileGlyph {
        let marker = intensity_marker(value, kind);
        let tile = match self {
            TileTheme::AsciiCompact | TileTheme::AsciiDetailed => format!("{marker}{marker}"),
            TileTheme::Unicode => unicode_intensity_tile(value, kind).to_string(),
        };
        TileGlyph {
            tile,
            style: intensity_style(value, kind),
        }
    }

    fn population_tile(self, cell: &CellView) -> TileGlyph {
        if let Some(population) = cell.population {
            let marker = population.clamp(0, 9);
            return TileGlyph {
                tile: format!("{}{}", tile_type(cell), marker),
                style: Style::default().fg(Color::Green),
            };
        }

        self.intensity_tile(0, IntensityKind::Population)
    }

    fn legend(self, overlay: MapOverlayInput) -> &'static str {
        match overlay {
            MapOverlayInput::Normal => {
                ".. Empty | == Road | R1/R2 Residential | C1 Commercial | I1 Industrial | T1 Power | P1 Park"
            }
            MapOverlayInput::Power => {
                "T* Plant | =* Powered road | R+ Powered | R- Unpowered | C+/C- Commercial | I+/I- Industrial"
            }
            MapOverlayInput::Pollution => ". Clean | - Low | + Medium | * High | # Severe",
            MapOverlayInput::Population => ".. None | R0-R9 Residential population",
            MapOverlayInput::LandValue => ". None | - Low | + Medium | * High | # Very High",
            MapOverlayInput::Desirability => "! Bad | - Low | + Medium | * Good | # Excellent",
        }
    }

    fn label(self) -> &'static str {
        match self {
            TileTheme::AsciiCompact => "ASCII Compact",
            TileTheme::AsciiDetailed => "ASCII-2",
            TileTheme::Unicode => "Unicode",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TileGlyph {
    pub tile: String,
    pub style: Style,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewState {
    None,
    Valid,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntensityKind {
    Pollution,
    Population,
    LandValue,
    Desirability,
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
            format!(
                "Simulation running at {}: auto tick every {}",
                self.run_speed.label(),
                speed_interval_label(self.run_speed)
            )
        } else {
            "Simulation paused".to_string()
        };
    }

    fn increase_speed(&mut self) {
        self.run_speed = self.run_speed.faster();
        self.message = format!("Simulation speed: {}", self.run_speed.label());
    }

    fn decrease_speed(&mut self) {
        self.run_speed = self.run_speed.slower();
        self.message = format!("Simulation speed: {}", self.run_speed.label());
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

struct TuiRuntime {
    game: Game,
    state: TuiState,
    next_auto_tick: Instant,
    dirty: bool,
}

impl TuiRuntime {
    fn new(now: Instant) -> Self {
        Self {
            game: Game::default(),
            state: TuiState::default(),
            next_auto_tick: now + RunSpeed::One.interval(),
            dirty: true,
        }
    }

    fn apply_due_auto_tick(&mut self, now: Instant) {
        if self.state.is_running && now >= self.next_auto_tick {
            self.state.message = self.game.tick().message();
            self.next_auto_tick = now + self.state.run_speed.interval();
            self.dirty = true;
        }
    }

    fn poll_timeout(&self, now: Instant) -> Duration {
        poll_timeout(self.state.is_running, self.next_auto_tick, now)
    }

    fn mark_clean(&mut self) {
        self.dirty = false;
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn handle_prompt_key(&mut self, key: KeyEvent) -> io::Result<bool> {
        let Some(prompt) = self.state.prompt.as_mut() else {
            return Ok(false);
        };

        // Returning `true` means the key was consumed by the prompt and should not also trigger a
        // normal gameplay action.
        match key.code {
            KeyCode::Esc => {
                self.state.prompt = None;
                self.state.message = "Cancelled prompt".to_string();
            }
            KeyCode::Backspace => {
                prompt.input.pop();
            }
            KeyCode::Enter => {
                let prompt = self.state.prompt.take().expect("prompt exists");
                let filename = if prompt.input.trim().is_empty() {
                    DEFAULT_SAVE_FILE.to_string()
                } else {
                    prompt.input.trim().to_string()
                };

                match prompt.kind {
                    PromptKind::Save => {
                        self.state.message = match self.game.save_to_file(&filename) {
                            Ok(()) => format!("Saved {filename}"),
                            Err(error) => error.to_string(),
                        };
                    }
                    PromptKind::Load => {
                        self.state.message = match Game::load_from_file(&filename) {
                            Ok(loaded_game) => {
                                self.game = loaded_game;
                                self.state.is_running = false;
                                // Loading swaps in a new game state, so the cursor is reset and then
                                // clamped against the loaded map dimensions.
                                self.state.reset_cursor();
                                let view = self.game.view_with_overlay(self.state.current_overlay);
                                self.state.clamp_cursor(&view);
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

        self.dirty = true;
        Ok(true)
    }

    fn apply_action(&mut self, action: TuiAction, now: Instant) -> TuiFlow {
        match action {
            TuiAction::MoveUp => self.move_cursor(0, -1),
            TuiAction::MoveDown => self.move_cursor(0, 1),
            TuiAction::MoveLeft => self.move_cursor(-1, 0),
            TuiAction::MoveRight => self.move_cursor(1, 0),
            TuiAction::SelectBuild(kind) => {
                self.state.selected_build = kind;
                self.state.message = format!("Selected {}", kind.label());
            }
            TuiAction::Build => {
                self.state.message = self
                    .game
                    .build(
                        self.state.cursor_x,
                        self.state.cursor_y,
                        self.state.selected_build,
                    )
                    .message();
            }
            TuiAction::Replace => {
                self.state.message = self
                    .game
                    .replace(
                        self.state.cursor_x,
                        self.state.cursor_y,
                        self.state.selected_build,
                    )
                    .message();
            }
            TuiAction::Upgrade => {
                self.state.message = self
                    .game
                    .upgrade(self.state.cursor_x, self.state.cursor_y)
                    .message();
            }
            TuiAction::Bulldoze => {
                self.state.message = self
                    .game
                    .bulldoze(self.state.cursor_x, self.state.cursor_y)
                    .message();
            }
            TuiAction::Tick => self.manual_tick(),
            TuiAction::Save => {
                self.state.prompt = Some(PromptState {
                    kind: PromptKind::Save,
                    input: String::new(),
                });
            }
            TuiAction::Load => {
                self.state.prompt = Some(PromptState {
                    kind: PromptKind::Load,
                    input: String::new(),
                });
            }
            TuiAction::ToggleHelp => self.state.show_help = !self.state.show_help,
            TuiAction::CycleOverlay => self.state.cycle_overlay(),
            TuiAction::ToggleRun => {
                self.state.toggle_run();
                self.next_auto_tick = now + self.state.run_speed.interval();
            }
            TuiAction::IncreaseSpeed => {
                self.state.increase_speed();
                self.next_auto_tick = now + self.state.run_speed.interval();
            }
            TuiAction::DecreaseSpeed => {
                self.state.decrease_speed();
                self.next_auto_tick = now + self.state.run_speed.interval();
            }
            TuiAction::Quit => return TuiFlow::Quit,
            TuiAction::None => return TuiFlow::Continue,
        }

        self.dirty = true;
        TuiFlow::Continue
    }

    fn move_cursor(&mut self, dx: isize, dy: isize) {
        let view = self.game.view_with_overlay(self.state.current_overlay);
        self.state.move_cursor(dx, dy, &view);
    }

    fn manual_tick(&mut self) {
        if self.state.is_running {
            self.state.message = "Pause before using manual next turn".to_string();
            return;
        }

        self.state.message = self.game.tick().message();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TuiFlow {
    Continue,
    Quit,
}

/// Runs the ratatui frontend while preserving the public Game API boundary.
pub fn run() -> io::Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    let mut runtime = TuiRuntime::new(Instant::now());

    loop {
        runtime.apply_due_auto_tick(Instant::now());

        if runtime.dirty {
            // Pull all render data through the public API only when the screen needs repainting.
            // This keeps the TUI from caching or reaching into ECS internals while avoiding idle
            // redraw work.
            let view = runtime
                .game
                .view_with_overlay(runtime.state.current_overlay);
            runtime.state.clamp_cursor(&view);
            let inspect = runtime
                .game
                .inspect(runtime.state.cursor_x, runtime.state.cursor_y);
            let preview = runtime.game.preview_build(
                runtime.state.cursor_x,
                runtime.state.cursor_y,
                runtime.state.selected_build,
            );

            // ratatui redraws the whole frame into an off-screen buffer and then flushes the diff
            // to the terminal. The closure receives a `Frame` that all render functions write into.
            terminal.draw(|frame| render(frame, &view, &inspect, &preview, &runtime.state))?;
            runtime.mark_clean();
        }

        // Sleep until input arrives or, in running mode, until the next scheduled auto tick is due.
        if !event::poll(runtime.poll_timeout(Instant::now()))? {
            continue;
        }

        // Crossterm can report mouse, resize, and paste events too. Resize changes should repaint.
        let Event::Key(key) = event::read()? else {
            runtime.mark_dirty();
            continue;
        };

        // Prompts are modal: while entering a filename, normal gameplay hotkeys are ignored.
        if runtime.handle_prompt_key(key)? {
            continue;
        }

        // Non-modal keys are normalized into actions before mutating UI state or calling Game APIs.
        let action = map_key_event(key);
        if runtime.apply_action(action, Instant::now()) == TuiFlow::Quit {
            return Ok(());
        }
    }
}

fn poll_timeout(is_running: bool, next_auto_tick: Instant, now: Instant) -> Duration {
    if is_running {
        next_auto_tick.saturating_duration_since(now)
    } else {
        PAUSED_POLL_TIMEOUT
    }
}

fn render(
    frame: &mut Frame<'_>,
    view: &GameView,
    inspect: &InspectView,
    preview: &BuildPreviewView,
    state: &TuiState,
) {
    let root = frame.area();
    if terminal_is_too_small(root) {
        render_too_small(frame, root);
        return;
    }

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

    render_map(frame, top[0], view, preview, state);
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

fn terminal_is_too_small(area: Rect) -> bool {
    area.width < MIN_TUI_WIDTH || area.height < MIN_TUI_HEIGHT
}

fn render_too_small(frame: &mut Frame<'_>, area: Rect) {
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "Terminal too small",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!(
            "Need at least {}x{}",
            MIN_TUI_WIDTH, MIN_TUI_HEIGHT
        )),
        Line::from(format!("Current: {}x{}", area.width, area.height)),
        Line::from(""),
        Line::from("Resize the terminal or run: cargo run -- ascii"),
        Line::from("Press Q to quit"),
    ];

    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title("Small City").borders(Borders::ALL))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_map(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &GameView,
    preview: &BuildPreviewView,
    state: &TuiState,
) {
    let mut lines = Vec::new();
    lines.push(Line::from(format!(
        "Overlay: {} | Theme: {} | {}",
        overlay_label(state.current_overlay),
        state.tile_theme.label(),
        state.tile_theme.legend(state.current_overlay)
    )));

    let gap = map_cell_gap(area, view);
    let cell_width = 2 + gap.len();
    let mut header = vec![Span::raw("   ")];
    for x in 0..view.map.width {
        header.push(Span::raw(format!("{x:^cell_width$}")));
    }
    lines.push(Line::from(header));

    for y in 0..view.map.height {
        let mut row = vec![Span::raw(format!("{y:>2} "))];
        for x in 0..view.map.width {
            let index = y * view.map.width + x;
            let cell = &view.map.cells[index];
            let is_cursor = x == state.cursor_x && y == state.cursor_y;
            let preview_state = preview_state_for_cell(x, y, preview, state);
            let glyph = state.tile_theme.tile_for_cell(
                cell,
                state.current_overlay,
                is_cursor,
                preview_state,
            );
            // Each tile is exactly two ASCII characters. The optional gap is outside the styled
            // tile so cursor highlighting never changes map width.
            row.push(Span::styled(glyph.tile, glyph.style));
            if !gap.is_empty() {
                row.push(Span::raw(gap));
            }
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
        simulation_status_line(state),
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
            .block(
                Block::default()
                    .title("Status")
                    .title(status_time_title(view))
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn status_time_title(view: &GameView) -> Line<'static> {
    let status = &view.status;
    Line::from(Span::styled(
        format!("Time: {} {}", time_spinner(status.turn), status.time.label),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
    .right_aligned()
}

fn time_spinner(turn: u32) -> char {
    match turn % 4 {
        0 => '|',
        1 => '/',
        2 => '-',
        _ => '\\',
    }
}

fn render_build_preview(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &GameView,
    preview: &BuildPreviewView,
    state: &TuiState,
) {
    let can_build = if preview.can_build { "Yes" } else { "No" };
    let preview_style = if preview.can_build {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    };
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
        Line::from(vec![
            Span::raw("Can Build: "),
            Span::styled(can_build, preview_style),
        ]),
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
        simulation_status_line(state),
        Line::from(format_message(&state.message)),
        Line::from(
            "Space pause/resume | +/- speed | WASD/Arrows move | 1-6 tools | N next | O overlay | H help | Q quit",
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
        Line::from("  Space         Pause / resume automatic ticks | +/- speed"),
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
        Line::from(format!("  Tile themes   {}", tile_theme_labels())),
        Line::from(""),
        help_section("Overlays"),
        Line::from("  O Cycle Overlay"),
        Line::from(
            "Overlay order: Normal -> Power -> Pollution -> Population -> Land Value -> Desirability",
        ),
        Line::from(format!(
            "Normal: {}",
            TileTheme::AsciiDetailed.legend(MapOverlayInput::Normal)
        )),
        Line::from(format!(
            "Power: {}",
            TileTheme::AsciiDetailed.legend(MapOverlayInput::Power)
        )),
        Line::from(format!(
            "Pollution: {}",
            TileTheme::AsciiDetailed.legend(MapOverlayInput::Pollution)
        )),
        Line::from(format!(
            "Population: {}",
            TileTheme::AsciiDetailed.legend(MapOverlayInput::Population)
        )),
        Line::from(format!(
            "Land Value: {}",
            TileTheme::AsciiDetailed.legend(MapOverlayInput::LandValue)
        )),
        Line::from(format!(
            "Desirability: {}",
            TileTheme::AsciiDetailed.legend(MapOverlayInput::Desirability)
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

fn tile_theme_labels() -> String {
    [
        TileTheme::AsciiDetailed,
        TileTheme::AsciiCompact,
        TileTheme::Unicode,
    ]
    .iter()
    .map(|theme| theme.label())
    .collect::<Vec<_>>()
    .join(" / ")
}

fn simulation_status_line(state: &TuiState) -> Line<'static> {
    let (label, color, detail) = if state.is_running {
        (
            "RUNNING",
            Color::Green,
            format!(
                "{} | auto tick every {}",
                state.run_speed.label(),
                speed_interval_label(state.run_speed)
            ),
        )
    } else {
        (
            "PAUSED",
            Color::Yellow,
            format!("{} | press Space to resume", state.run_speed.label()),
        )
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

fn speed_interval_label(speed: RunSpeed) -> &'static str {
    match speed {
        RunSpeed::One => "1 second",
        RunSpeed::Two => "500ms",
        RunSpeed::Four => "250ms",
    }
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

fn map_cell_gap(area: Rect, view: &GameView) -> &'static str {
    let inner_width = usize::from(area.width.saturating_sub(2));
    let width_with_gaps = 3 + view.map.width * 3;
    if width_with_gaps <= inner_width {
        " "
    } else {
        ""
    }
}

fn preview_state_for_cell(
    x: usize,
    y: usize,
    preview: &BuildPreviewView,
    state: &TuiState,
) -> PreviewState {
    if x != state.cursor_x || y != state.cursor_y {
        return PreviewState::None;
    }

    if preview.can_build {
        PreviewState::Valid
    } else {
        PreviewState::Invalid
    }
}

fn ascii_detailed_normal_tile(cell: &CellView) -> String {
    let Some(kind) = cell.building else {
        return "..".to_string();
    };

    if matches!(cell.road_connected, Some(false)) {
        return format!("{}!", tile_type(cell));
    }
    if matches!(cell.powered, Some(false)) && cell.power_demand.unwrap_or_default() > 0 {
        return format!("{}-", tile_type(cell));
    }

    match kind {
        BuildingKind::Road => "==".to_string(),
        BuildingKind::Residential
        | BuildingKind::Commercial
        | BuildingKind::Industrial
        | BuildingKind::PowerPlant
        | BuildingKind::Park => format!("{}{}", tile_type(cell), tile_level(cell)),
    }
}

fn unicode_normal_tile(cell: &CellView) -> String {
    match ascii_detailed_normal_tile(cell).as_str() {
        ".." => "..".to_string(),
        "==" => "==".to_string(),
        tile => tile.to_string(),
    }
}

fn tile_type(cell: &CellView) -> char {
    match cell.building {
        Some(BuildingKind::Road) => '=',
        Some(BuildingKind::Residential) => 'R',
        Some(BuildingKind::Commercial) => 'C',
        Some(BuildingKind::Industrial) => 'I',
        Some(BuildingKind::PowerPlant) => 'T',
        Some(BuildingKind::Park) => 'P',
        None => '.',
    }
}

fn tile_level(cell: &CellView) -> char {
    char::from_digit(u32::from(cell.upgrade_level.unwrap_or(1).min(9)), 10).unwrap_or('1')
}

fn intensity_marker(value: i32, kind: IntensityKind) -> char {
    match kind {
        IntensityKind::Desirability => match value {
            i32::MIN..=0 => '!',
            1..=3 => '-',
            4..=6 => '+',
            7..=8 => '*',
            _ => '#',
        },
        _ => match value {
            i32::MIN..=0 => '.',
            1..=2 => '-',
            3..=5 => '+',
            6..=8 => '*',
            _ => '#',
        },
    }
}

fn unicode_intensity_tile(value: i32, kind: IntensityKind) -> &'static str {
    match intensity_marker(value, kind) {
        '!' => "!!",
        '.' => "..",
        '-' => "--",
        '+' => "++",
        '*' => "**",
        '#' => "##",
        _ => "..",
    }
}

fn intensity_style(value: i32, kind: IntensityKind) -> Style {
    match kind {
        IntensityKind::Pollution => match value {
            i32::MIN..=0 => empty_style(),
            1..=5 => Style::default().fg(Color::Yellow),
            _ => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        },
        IntensityKind::Population => Style::default().fg(Color::Green),
        IntensityKind::LandValue => Style::default().fg(Color::Cyan),
        IntensityKind::Desirability => match value {
            i32::MIN..=3 => Style::default().fg(Color::Red),
            4..=6 => Style::default().fg(Color::Yellow),
            _ => Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        },
    }
}

fn cell_base_style(cell: &CellView) -> Style {
    let mut style = match cell.building {
        Some(kind) => building_style(kind),
        None => empty_style(),
    };

    if matches!(cell.road_connected, Some(false))
        || (matches!(cell.powered, Some(false)) && cell.power_demand.unwrap_or_default() > 0)
    {
        style = problem_style();
    }

    style
}

fn empty_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn problem_style() -> Style {
    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
}

fn building_style(kind: BuildingKind) -> Style {
    match kind {
        BuildingKind::Road => Style::default().fg(Color::Gray),
        BuildingKind::Residential => Style::default().fg(Color::Green),
        BuildingKind::Commercial => Style::default().fg(Color::Yellow),
        BuildingKind::Industrial => Style::default().fg(Color::Magenta),
        BuildingKind::PowerPlant => Style::default().fg(Color::Cyan),
        BuildingKind::Park => Style::default()
            .fg(Color::LightGreen)
            .add_modifier(Modifier::BOLD),
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

fn format_message(message: &str) -> String {
    if message.starts_with("OK:")
        || message.starts_with("WARN:")
        || message.starts_with("ERR:")
        || message.starts_with("INFO:")
    {
        return message.to_string();
    }

    let prefix = if message.contains("Cannot")
        || message.contains("Failed")
        || message.contains("not")
        || message.contains("Invalid")
        || message.contains("error")
    {
        "ERR"
    } else if message.contains("Shortage") || message.contains("unpowered") {
        "WARN"
    } else if message.contains("Advanced to turn") {
        "INFO"
    } else {
        "OK"
    };

    format!("{prefix}: {message}")
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
    use crossterm::event::KeyModifiers;
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
        assert_eq!(state.tile_theme, TileTheme::AsciiDetailed);
        assert_eq!(state.run_speed, RunSpeed::One);
        state.toggle_run();
        assert!(state.is_running);
        assert_eq!(
            state.message,
            "Simulation running at 1x: auto tick every 1 second"
        );
        state.toggle_run();
        assert!(!state.is_running);
        assert_eq!(state.message, "Simulation paused");
    }

    #[test]
    fn run_speed_increase_and_decrease_are_clamped() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);

        assert_eq!(runtime.state.run_speed, RunSpeed::One);
        assert_eq!(
            runtime.apply_action(TuiAction::IncreaseSpeed, now),
            TuiFlow::Continue
        );
        assert_eq!(runtime.state.run_speed, RunSpeed::Two);
        assert_eq!(runtime.next_auto_tick, now + Duration::from_millis(500));
        assert_eq!(
            runtime.apply_action(TuiAction::IncreaseSpeed, now),
            TuiFlow::Continue
        );
        assert_eq!(runtime.state.run_speed, RunSpeed::Four);
        assert_eq!(runtime.next_auto_tick, now + Duration::from_millis(250));
        assert_eq!(
            runtime.apply_action(TuiAction::IncreaseSpeed, now),
            TuiFlow::Continue
        );
        assert_eq!(runtime.state.run_speed, RunSpeed::Four);

        assert_eq!(
            runtime.apply_action(TuiAction::DecreaseSpeed, now),
            TuiFlow::Continue
        );
        assert_eq!(runtime.state.run_speed, RunSpeed::Two);
        assert_eq!(
            runtime.apply_action(TuiAction::DecreaseSpeed, now),
            TuiFlow::Continue
        );
        assert_eq!(runtime.state.run_speed, RunSpeed::One);
        assert_eq!(
            runtime.apply_action(TuiAction::DecreaseSpeed, now),
            TuiFlow::Continue
        );
        assert_eq!(runtime.state.run_speed, RunSpeed::One);
    }

    #[test]
    fn ascii_detailed_theme_maps_normal_tiles() {
        let residential = themed_cell(Some(BuildingKind::Residential), 'R', Some(2), None, None, 0);
        let road = themed_cell(Some(BuildingKind::Road), '=', None, None, None, 0);
        let empty = themed_cell(None, '.', None, None, None, 0);

        assert_eq!(
            TileTheme::AsciiDetailed
                .tile_for_cell(
                    &residential,
                    MapOverlayInput::Normal,
                    false,
                    PreviewState::None
                )
                .tile,
            "R2"
        );
        assert_eq!(
            TileTheme::AsciiDetailed
                .tile_for_cell(&road, MapOverlayInput::Normal, false, PreviewState::None)
                .tile,
            "=="
        );
        assert_eq!(
            TileTheme::AsciiDetailed
                .tile_for_cell(&empty, MapOverlayInput::Normal, false, PreviewState::None)
                .tile,
            ".."
        );
    }

    #[test]
    fn ascii_detailed_power_overlay_uses_fixed_width_tiles() {
        let cases = [
            (
                themed_cell(Some(BuildingKind::PowerPlant), 'P', Some(1), None, None, 0),
                "T*",
            ),
            (
                themed_cell(Some(BuildingKind::Road), '*', None, None, None, 0),
                "=*",
            ),
            (
                themed_cell(
                    Some(BuildingKind::Residential),
                    '+',
                    Some(1),
                    Some(true),
                    None,
                    0,
                ),
                "R+",
            ),
            (
                themed_cell(
                    Some(BuildingKind::Commercial),
                    '-',
                    Some(1),
                    Some(false),
                    None,
                    0,
                ),
                "C-",
            ),
            (themed_cell(None, '.', None, None, None, 0), ".."),
        ];

        for (cell, expected) in cases {
            let glyph = TileTheme::AsciiDetailed.tile_for_cell(
                &cell,
                MapOverlayInput::Power,
                false,
                PreviewState::None,
            );
            assert_eq!(glyph.tile, expected);
            assert_eq!(glyph.tile.len(), 2);
            assert!(glyph.tile.is_ascii());
        }
    }

    #[test]
    fn ascii_detailed_overlays_return_fixed_width_ascii_tiles() {
        let cell = themed_cell(Some(BuildingKind::Industrial), 'I', Some(1), None, None, 9);

        for overlay in [
            MapOverlayInput::Pollution,
            MapOverlayInput::LandValue,
            MapOverlayInput::Desirability,
            MapOverlayInput::Population,
        ] {
            let glyph =
                TileTheme::AsciiDetailed.tile_for_cell(&cell, overlay, false, PreviewState::None);
            assert_eq!(glyph.tile.len(), 2);
            assert!(glyph.tile.is_ascii());
        }
    }

    #[test]
    fn tile_theme_styles_cursor_and_build_preview() {
        let cell = themed_cell(None, '.', None, None, None, 0);

        let valid = TileTheme::AsciiDetailed.tile_for_cell(
            &cell,
            MapOverlayInput::Normal,
            true,
            PreviewState::Valid,
        );
        let invalid = TileTheme::AsciiDetailed.tile_for_cell(
            &cell,
            MapOverlayInput::Normal,
            false,
            PreviewState::Invalid,
        );

        assert!(valid.style.add_modifier.contains(Modifier::REVERSED));
        assert_eq!(invalid.style.fg, Some(Color::Red));
        assert!(invalid.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn manual_tick_advances_only_when_paused() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);
        runtime.game = Game::new(10, 10);
        runtime.state.run_speed = RunSpeed::Four;

        assert_eq!(
            runtime.apply_action(TuiAction::Tick, now),
            TuiFlow::Continue
        );

        assert_eq!(runtime.game.view().status.turn, 1);
        assert!(runtime.state.message.contains("Advanced to turn 1"));
        assert!(runtime.dirty);
    }

    #[test]
    fn manual_tick_is_blocked_while_running() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);
        runtime.game = Game::new(10, 10);
        runtime.state.is_running = true;
        runtime.state.run_speed = RunSpeed::Four;

        assert_eq!(
            runtime.apply_action(TuiAction::Tick, now),
            TuiFlow::Continue
        );

        assert_eq!(runtime.game.view().status.turn, 0);
        assert_eq!(runtime.state.message, "Pause before using manual next turn");
        assert!(runtime.dirty);
    }

    #[test]
    fn apply_action_updates_build_tool_and_cursor() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);
        runtime.game = Game::new(3, 3);
        runtime.mark_clean();

        assert_eq!(
            runtime.apply_action(TuiAction::SelectBuild(BuildingKind::Road), now),
            TuiFlow::Continue
        );
        assert_eq!(runtime.state.selected_build, BuildingKind::Road);
        assert_eq!(runtime.state.message, "Selected Road");
        assert!(runtime.dirty);

        runtime.mark_clean();
        assert_eq!(
            runtime.apply_action(TuiAction::MoveRight, now),
            TuiFlow::Continue
        );
        assert_eq!((runtime.state.cursor_x, runtime.state.cursor_y), (1, 0));
        assert!(runtime.dirty);
    }

    #[test]
    fn runtime_applies_due_auto_tick_when_running() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);
        runtime.game = Game::new(10, 10);
        runtime.state.is_running = true;
        runtime.next_auto_tick = now;
        runtime.mark_clean();

        runtime.apply_due_auto_tick(now);

        assert_eq!(runtime.game.view().status.turn, 1);
        assert_eq!(runtime.next_auto_tick, now + AUTO_TICK_INTERVAL);
        assert!(runtime.dirty);
    }

    #[test]
    fn runtime_applies_due_auto_tick_using_current_speed_interval() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);
        runtime.game = Game::new(10, 10);
        runtime.state.is_running = true;
        runtime.state.run_speed = RunSpeed::Four;
        runtime.next_auto_tick = now;

        runtime.apply_due_auto_tick(now);

        assert_eq!(runtime.game.view().status.turn, 1);
        assert_eq!(runtime.next_auto_tick, now + Duration::from_millis(250));
    }

    #[test]
    fn prompt_input_and_cancel_are_runtime_state_only() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);
        runtime.apply_action(TuiAction::Save, now);
        runtime.mark_clean();

        assert!(
            runtime
                .handle_prompt_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
                .expect("prompt char handled")
        );
        assert_eq!(runtime.state.prompt.as_ref().expect("prompt").input, "a");
        assert!(runtime.dirty);

        runtime
            .handle_prompt_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
            .expect("prompt backspace handled");
        assert_eq!(runtime.state.prompt.as_ref().expect("prompt").input, "");

        runtime
            .handle_prompt_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .expect("prompt escape handled");
        assert!(runtime.state.prompt.is_none());
        assert_eq!(runtime.state.message, "Cancelled prompt");
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
    fn running_next_tick_interval_follows_speed() {
        assert_eq!(RunSpeed::One.interval(), Duration::from_secs(1));
        assert_eq!(RunSpeed::Two.interval(), Duration::from_millis(500));
        assert_eq!(RunSpeed::Four.interval(), Duration::from_millis(250));
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
            "1x | press Space to resume",
            "Time: | Year 1, Month 1, Week 1, Day 1, 00:00",
        ] {
            assert!(
                output.contains(expected),
                "expected TUI output to contain {expected:?}\n{output}"
            );
        }
    }

    #[test]
    fn status_time_renders_in_panel_header() {
        let game = Game::new(10, 10);
        let output = render_test_screen(&game, TuiState::default());
        let header = output
            .lines()
            .find(|line| line.contains("Status"))
            .expect("status panel header");

        assert!(header.contains("Time: | Year 1, Month 1, Week 1, Day 1, 00:00"));
    }

    #[test]
    fn status_time_spinner_advances_with_turns() {
        let mut game = Game::new(10, 10);
        game.tick();
        let output = render_test_screen(&game, TuiState::default());

        assert!(output.contains("Time: / Year 1, Month 1, Week 1, Day 1, 01:00"));
    }

    #[test]
    fn time_spinner_cycles_by_turn() {
        assert_eq!(time_spinner(0), '|');
        assert_eq!(time_spinner(1), '/');
        assert_eq!(time_spinner(2), '-');
        assert_eq!(time_spinner(3), '\\');
        assert_eq!(time_spinner(4), '|');
    }

    #[test]
    fn small_terminal_renders_resize_warning() {
        let game = Game::new(10, 10);
        let output = render_test_screen_with_size(&game, TuiState::default(), 80, 24);

        assert!(output.contains("Small City"));
        assert!(output.contains("Terminal too small"));
        assert!(output.contains("Need at least 100x30"));
        assert!(output.contains("Current: 80x24"));
        assert!(output.contains("cargo run -- ascii"));
        assert!(output.contains("Press Q to quit"));
        assert!(!output.contains("City Map"));
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
        assert!(output.contains("Space         Pause / resume automatic ticks | +/- speed"));
        assert!(output.contains("O Cycle Overlay"));
        assert!(output.contains("Overlay order: Normal -> Power -> Pollution"));
        assert!(output.contains("Normal: .. Empty"));
        assert!(output.contains("Desirability: ! Bad"));
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
        assert!(output.contains("1x | auto tick every 1 second"));
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
        let cell = find_text_cell_after_prefix(buffer, "Tool: ", "Industrial")
            .expect("styled Industrial tool text");

        assert_eq!(cell.fg, Color::Magenta);
        assert!(cell.modifier.contains(Modifier::BOLD));
    }

    fn themed_cell(
        building: Option<BuildingKind>,
        symbol: char,
        upgrade_level: Option<u8>,
        powered: Option<bool>,
        road_connected: Option<bool>,
        effect_value: i32,
    ) -> CellView {
        CellView {
            x: 0,
            y: 0,
            symbol,
            building,
            label: building
                .map(|kind| kind.label().to_string())
                .unwrap_or_else(|| "Empty Land".to_string()),
            buildable: building.is_none(),
            population: if matches!(building, Some(BuildingKind::Residential)) {
                Some(effect_value)
            } else {
                None
            },
            max_population: None,
            powered,
            power_demand: powered.map(|_| 1),
            road_connected,
            upgrade_level,
            local_effects: crate::interface::view::LocalEffectsView {
                land_value: effect_value,
                pollution_pressure: effect_value,
                accessibility: 0,
                desirability: effect_value,
            },
        }
    }

    fn render_test_screen(game: &Game, state: TuiState) -> String {
        render_test_screen_with_size(game, state, 120, 36)
    }

    fn render_test_screen_with_size(
        game: &Game,
        state: TuiState,
        width: u16,
        height: u16,
    ) -> String {
        let terminal = render_test_terminal_with_size(game, state, width, height);
        buffer_text(terminal.backend().buffer())
    }

    fn render_test_terminal(game: &Game, state: TuiState) -> Terminal<TestBackend> {
        render_test_terminal_with_size(game, state, 120, 36)
    }

    fn render_test_terminal_with_size(
        game: &Game,
        mut state: TuiState,
        width: u16,
        height: u16,
    ) -> Terminal<TestBackend> {
        let view = game.view_with_overlay(state.current_overlay);
        state.clamp_cursor(&view);
        let inspect = game.inspect(state.cursor_x, state.cursor_y);
        let preview = game.preview_build(state.cursor_x, state.cursor_y, state.selected_build);

        let backend = TestBackend::new(width, height);
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

    fn find_text_cell_after_prefix<'a>(
        buffer: &'a ratatui::buffer::Buffer,
        prefix: &str,
        needle: &str,
    ) -> Option<&'a ratatui::buffer::Cell> {
        let area = buffer.area();
        let width = area.width as usize;
        let full_needle = format!("{prefix}{needle}");

        for row in buffer.content().chunks(width) {
            let mut line = String::new();
            for cell in row {
                line.push_str(cell.symbol());
            }

            if let Some(byte_x) = line.find(&full_needle) {
                let prefix_byte_x = byte_x + prefix.len();
                let cell_x = line[..prefix_byte_x].chars().count();
                let width = needle.chars().count();
                return row
                    .get(cell_x..cell_x + width)
                    .and_then(|cells| cells.iter().find(|cell| cell.fg != Color::Reset))
                    .or_else(|| row.get(cell_x));
            }
        }

        None
    }
}
