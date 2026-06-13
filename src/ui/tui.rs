//! Panel-based ratatui terminal frontend built only from facade view models.

use std::env;
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

use crate::interface::input::{BuildingKind, MapOverlayInput};
use crate::interface::view::{
    BuildPreviewView, CellView, DemandLevel, GameView, InspectDetailsView, InspectView,
};
use crate::ui::ascii;
use crate::ui::city_driver::{CityDriver, CityLaunchMode};
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
    viewport_x: usize,
    viewport_y: usize,
    selected_build: BuildingKind,
    current_overlay: MapOverlayInput,
    tile_theme: TileTheme,
    message: String,
    region_label: String,
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
            viewport_x: 0,
            viewport_y: 0,
            selected_build: BuildingKind::Residential,
            current_overlay: MapOverlayInput::Normal,
            tile_theme: TileTheme::AsciiDetailed,
            message: "Tiny City Builder".to_string(),
            region_label: "Region: single city".to_string(),
            is_running: false,
            run_speed: RunSpeed::One,
            show_help: false,
            prompt: None,
        }
    }
}

/// UI-only fast-forward speed. The simulation still advances by repeated facade tick calls.
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
/// The live TUI selects `Unicode` on startup when the locale advertises UTF-8 support. Otherwise,
/// `AsciiDetailed` keeps every tile to exactly two ASCII characters so map rows stay aligned on
/// constrained terminals. `TuiState::default()` stays deterministic for tests and starts at
/// `AsciiDetailed`. `AsciiCompact` preserves the older one-character language inside a two-character
/// tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TileTheme {
    AsciiCompact,
    AsciiDetailed,
    Unicode,
}

fn default_tile_theme() -> TileTheme {
    if locale_supports_unicode(current_terminal_locale().as_deref()) {
        TileTheme::Unicode
    } else {
        TileTheme::AsciiDetailed
    }
}

fn current_terminal_locale() -> Option<String> {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|name| env::var(name).ok().filter(|value| !value.trim().is_empty()))
}

fn locale_supports_unicode(locale: Option<&str>) -> bool {
    let Some(locale) = locale else {
        return false;
    };
    let locale = locale.to_ascii_lowercase();

    locale.contains("utf-8") || locale.contains("utf8")
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
        self.viewport_x = self.viewport_x.min(view.map.width.saturating_sub(1));
        self.viewport_y = self.viewport_y.min(view.map.height.saturating_sub(1));
    }

    fn reset_cursor(&mut self) {
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.viewport_x = 0;
        self.viewport_y = 0;
    }

    fn follow_cursor_in_map_viewport(&mut self, view: &GameView, area: Rect) {
        let gap = map_cell_gap(area, view);
        let visible_columns = visible_map_columns(area, gap, view);
        let visible_rows = visible_map_rows(area, view, self.tile_theme, self.current_overlay);

        self.viewport_x = follow_axis(
            self.cursor_x,
            self.viewport_x,
            visible_columns,
            view.map.width,
        );
        self.viewport_y = follow_axis(
            self.cursor_y,
            self.viewport_y,
            visible_rows,
            view.map.height,
        );
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

    fn cycle_tile_theme(&mut self) {
        self.tile_theme = match self.tile_theme {
            TileTheme::AsciiDetailed => TileTheme::AsciiCompact,
            TileTheme::AsciiCompact => TileTheme::Unicode,
            TileTheme::Unicode => TileTheme::AsciiDetailed,
        };
        self.message = format!("Tile theme: {}", self.tile_theme.label());
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
    game: CityDriver,
    state: TuiState,
    next_auto_tick: Instant,
    dirty: bool,
}

impl TuiRuntime {
    #[cfg(test)]
    fn new(now: Instant) -> Self {
        Self::with_mode(now, CityLaunchMode::RegionalMultiRegion).expect("regional TUI runtime")
    }

    fn with_mode(
        now: Instant,
        mode: CityLaunchMode,
    ) -> Result<Self, crate::ui::city_driver::CityDriverError> {
        Ok(Self {
            game: CityDriver::new(mode)?,
            state: TuiState::default(),
            next_auto_tick: now + RunSpeed::One.interval(),
            dirty: true,
        })
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
                        self.state.message = match self.game.load_from_file(&filename) {
                            Ok(()) => {
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
            TuiAction::CycleTheme => self.state.cycle_tile_theme(),
            TuiAction::PreviousRegion => {
                self.state.message = self.game.select_previous_region();
                self.state.region_label = self.game.region_label();
                self.state.reset_cursor();
            }
            TuiAction::NextRegion => {
                self.state.message = self.game.select_next_region();
                self.state.region_label = self.game.region_label();
                self.state.reset_cursor();
            }
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

/// Runs the ratatui frontend on the regional facade.
pub fn run() -> io::Result<()> {
    run_with_mode(CityLaunchMode::RegionalMultiRegion)
}

/// Backward-compatible alias for the default regional TUI mode.
pub fn run_regional() -> io::Result<()> {
    run_with_mode(CityLaunchMode::RegionalMultiRegion)
}

fn run_with_mode(mode: CityLaunchMode) -> io::Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    let mut runtime = TuiRuntime::with_mode(Instant::now(), mode)
        .map_err(|error| io::Error::other(error.to_string()))?;
    runtime.state.tile_theme = default_tile_theme();

    loop {
        runtime.apply_due_auto_tick(Instant::now());

        if runtime.dirty {
            // Pull all render data through the public API only when the screen needs repainting.
            // This keeps the TUI from caching or reaching into ECS internals while avoiding idle
            // redraw work.
            let view = runtime
                .game
                .view_with_overlay(runtime.state.current_overlay);
            runtime.state.region_label = runtime.game.region_label();
            runtime.state.clamp_cursor(&view);
            let inspect = runtime
                .game
                .inspect(runtime.state.cursor_x, runtime.state.cursor_y);
            let preview = runtime.game.preview_build(
                runtime.state.cursor_x,
                runtime.state.cursor_y,
                runtime.state.selected_build,
            );
            if let Some(error) = runtime.game.take_read_error_message() {
                runtime.state.message = error;
            }

            // ratatui redraws the whole frame into an off-screen buffer and then flushes the diff
            // to the terminal. The closure receives a `Frame` that all render functions write into.
            terminal.draw(|frame| render(frame, &view, &inspect, &preview, &mut runtime.state))?;
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

        // Non-modal keys are normalized into actions before mutating UI state or calling facades.
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
    state: &mut TuiState,
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

    // The map viewport depends on the panel `Rect`, which is only known during draw. Keep that
    // layout-derived scroll state in `TuiState` so cursor movement can follow the visible window.
    state.follow_cursor_in_map_viewport(view, top[0]);

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
    lines.push(Line::from(map_overlay_header(
        state.tile_theme,
        state.current_overlay,
    )));

    let gap = map_cell_gap(area, view);
    let visible_columns = visible_map_columns(area, gap, view);
    let visible_rows = visible_map_rows(area, view, state.tile_theme, state.current_overlay);
    let end_x = state
        .viewport_x
        .saturating_add(visible_columns)
        .min(view.map.width);
    let end_y = state
        .viewport_y
        .saturating_add(visible_rows)
        .min(view.map.height);
    let cell_width = 2 + gap.len();
    let mut header = vec![Span::raw("   ")];
    for x in state.viewport_x..end_x {
        header.push(Span::raw(format!("{x:^cell_width$}")));
    }
    lines.push(Line::from(header));

    for y in state.viewport_y..end_y {
        let mut row = vec![Span::raw(format!("{y:>2} "))];
        for x in state.viewport_x..end_x {
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
            // Each tile is exactly two display columns. The optional gap is outside the styled
            // tile so cursor highlighting never changes map width.
            row.push(Span::styled(glyph.tile, glyph.style));
            if !gap.is_empty() {
                let next_cell = (x + 1 < end_x).then(|| &view.map.cells[index + 1]);
                row.push(Span::raw(map_gap_after_cell(
                    gap,
                    state.tile_theme,
                    state.current_overlay,
                    cell,
                    next_cell,
                )));
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

fn visible_map_columns(area: Rect, gap: &'static str, view: &GameView) -> usize {
    let inner_width = usize::from(area.width.saturating_sub(2));
    let label_width = 3;
    // Count a possible trailing gap in every tile slot. This can leave one extra column unused,
    // but it prevents the map row from overflowing the panel.
    let cell_width = 2 + gap.len();
    inner_width
        .saturating_sub(label_width)
        .checked_div(cell_width)
        .unwrap_or(0)
        .max(1)
        .min(view.map.width)
}

fn visible_map_rows(
    area: Rect,
    view: &GameView,
    theme: TileTheme,
    overlay: MapOverlayInput,
) -> usize {
    let inner_height = usize::from(area.height.saturating_sub(2));
    let inner_width = usize::from(area.width.saturating_sub(2)).max(1);
    // Ratatui wraps the header by display width and word boundaries. This estimate is intentionally
    // simple; if it is short by a row on a narrow terminal, the Paragraph clips the extra map line.
    let overlay_rows = map_overlay_header(theme, overlay)
        .chars()
        .count()
        .div_ceil(inner_width)
        .max(1);
    let overlay_and_header_rows = overlay_rows + 1;
    inner_height
        .saturating_sub(overlay_and_header_rows)
        .max(1)
        .min(view.map.height)
}

fn map_overlay_header(theme: TileTheme, overlay: MapOverlayInput) -> String {
    format!(
        "Overlay: {} | Theme: {} | {}",
        overlay_label(overlay),
        theme.label(),
        theme.legend(overlay)
    )
}

fn follow_axis(cursor: usize, viewport: usize, visible: usize, total: usize) -> usize {
    if total == 0 || visible == 0 || visible >= total {
        return 0;
    }

    let max_viewport = total - visible;
    if cursor < viewport {
        cursor.min(max_viewport)
    } else if cursor >= viewport + visible {
        cursor
            .saturating_add(1)
            .saturating_sub(visible)
            .min(max_viewport)
    } else {
        viewport.min(max_viewport)
    }
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
        Line::from(state.region_label.clone()),
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
            "Space pause/resume | +/- speed | WASD/Arrows move | 1-6 tools | N next | O overlay | T theme | [ ] region | H help | Q quit",
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
        Line::from("  N             Next turn        [ / ] Previous / next region"),
        Line::from(""),
        help_section("Files And UI"),
        Line::from("  S             Save city"),
        Line::from("  L             Load city"),
        Line::from("  H             Close Help"),
        Line::from("  Q             Quit"),
        Line::from("  Enter at save/load prompt uses city1"),
        Line::from(format!(
            "  T             Cycle tile theme: {}",
            tile_theme_labels()
        )),
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

fn map_gap_after_cell(
    base_gap: &'static str,
    theme: TileTheme,
    overlay: MapOverlayInput,
    cell: &CellView,
    next_cell: Option<&CellView>,
) -> &'static str {
    if base_gap.is_empty() || theme != TileTheme::Unicode || overlay != MapOverlayInput::Normal {
        return base_gap;
    }

    let Some(next_cell) = next_cell else {
        return base_gap;
    };

    if cell.building == Some(BuildingKind::Road)
        && next_cell.building == Some(BuildingKind::Road)
        && cell.road_links.east
        && next_cell.road_links.west
    {
        "─"
    } else {
        base_gap
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

    if let Some(marker) = building_problem_marker(cell) {
        return format!("{}{}", tile_type(cell), marker);
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
    if cell.building == Some(BuildingKind::Road) {
        return unicode_road_tile(cell);
    }

    let Some(kind) = cell.building else {
        return "..".to_string();
    };

    if building_problem_marker(cell).is_some() {
        return ascii_detailed_normal_tile(cell);
    }

    format!("{}{}", tile_type(cell), unicode_building_shade(cell, kind))
}

fn unicode_road_tile(cell: &CellView) -> String {
    let links = cell.road_links;
    let left = match (links.north, links.east, links.south, links.west) {
        (false, false, false, false) => '─',
        (true, false, false, false) | (false, false, true, false) => '│',
        (false, true, false, false) | (false, false, false, true) | (false, true, false, true) => {
            '─'
        }
        (true, true, false, false) => '└',
        (true, false, false, true) => '┘',
        (false, true, true, false) => '┌',
        (false, false, true, true) => '┐',
        (true, true, false, true) => '┴',
        (false, true, true, true) => '┬',
        (true, true, true, false) => '├',
        (true, false, true, true) => '┤',
        (true, false, true, false) => '│',
        (true, true, true, true) => '┼',
    };
    let right = if links.east || !any_road_link(links) {
        '─'
    } else {
        ' '
    };

    let mut tile = String::with_capacity(6);
    tile.push(left);
    tile.push(right);
    tile
}

fn any_road_link(links: crate::interface::view::RoadLinks) -> bool {
    links.north || links.east || links.south || links.west
}

fn building_problem_marker(cell: &CellView) -> Option<char> {
    if matches!(cell.road_connected, Some(false)) {
        Some('!')
    } else if matches!(cell.powered, Some(false)) && cell.power_demand.unwrap_or_default() > 0 {
        Some('-')
    } else {
        None
    }
}

fn unicode_building_shade(cell: &CellView, kind: BuildingKind) -> char {
    match kind {
        BuildingKind::Residential => unicode_residential_occupancy_shade(cell),
        BuildingKind::Commercial
        | BuildingKind::Industrial
        | BuildingKind::PowerPlant
        | BuildingKind::Park => unicode_level_shade(cell.upgrade_level.unwrap_or(1)),
        BuildingKind::Road => '░',
    }
}

fn unicode_residential_occupancy_shade(cell: &CellView) -> char {
    let population = cell.population.unwrap_or_default().max(0) as i64;
    let max_population = cell.max_population.unwrap_or_default().max(0) as i64;

    if population == 0 || max_population == 0 {
        '░'
    } else if population >= max_population {
        '█'
    } else if population * 2 < max_population {
        '▒'
    } else {
        '▓'
    }
}

fn unicode_level_shade(level: u8) -> char {
    match level {
        0 | 1 => '░',
        2 => '▒',
        _ => '▓',
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
    let Some(kind) = cell.building else {
        return empty_style();
    };

    if matches!(cell.road_connected, Some(false))
        || (matches!(cell.powered, Some(false)) && cell.power_demand.unwrap_or_default() > 0)
    {
        return problem_style();
    }

    let mut style = building_style(kind);
    if let Some(modifier) = zone_activity_modifier(cell, kind) {
        style = style.add_modifier(modifier);
    }

    style
}

fn zone_activity_modifier(cell: &CellView, kind: BuildingKind) -> Option<Modifier> {
    let score = match kind {
        BuildingKind::Residential => residential_activity_score(cell)?,
        BuildingKind::Commercial | BuildingKind::Industrial => {
            commercial_industrial_activity_score(cell)
        }
        BuildingKind::Road | BuildingKind::PowerPlant | BuildingKind::Park => return None,
    };

    match score {
        i32::MIN..=2 => Some(Modifier::DIM),
        7..=i32::MAX => Some(Modifier::BOLD),
        _ => None,
    }
}

fn residential_activity_score(cell: &CellView) -> Option<i32> {
    let population = cell.population?;
    let max_population = cell.max_population?;
    if max_population <= 0 {
        return None;
    }

    Some((population.clamp(0, max_population) * 10) / max_population)
}

fn commercial_industrial_activity_score(cell: &CellView) -> i32 {
    // Core local effects clamp land value to 0..9; normalize it to the same 0..10 score used by
    // residential occupancy before applying shared style thresholds.
    (cell.local_effects.land_value.clamp(0, 9) * 10) / 9
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
    use crate::core::regional_game::RegionalGame;
    use crate::core::regions::RegionId;
    use crate::interface::view::RoadLinks;
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
    fn tile_theme_cycles_in_display_order() {
        let mut state = TuiState {
            tile_theme: TileTheme::AsciiDetailed,
            ..TuiState::default()
        };

        state.cycle_tile_theme();
        assert_eq!(state.tile_theme, TileTheme::AsciiCompact);
        assert_eq!(state.message, "Tile theme: ASCII Compact");

        state.cycle_tile_theme();
        assert_eq!(state.tile_theme, TileTheme::Unicode);
        assert_eq!(state.message, "Tile theme: Unicode");

        state.cycle_tile_theme();
        assert_eq!(state.tile_theme, TileTheme::AsciiDetailed);
        assert_eq!(state.message, "Tile theme: ASCII-2");
    }

    #[test]
    fn default_theme_uses_unicode_when_locale_supports_utf8() {
        assert!(locale_supports_unicode(Some("en_US.UTF-8")));
        assert!(locale_supports_unicode(Some("C.UTF8")));
        assert!(!locale_supports_unicode(Some("C")));
        assert!(!locale_supports_unicode(Some("POSIX")));
        assert!(!locale_supports_unicode(None));
    }

    #[test]
    fn tui_state_default_uses_deterministic_ascii_theme() {
        assert_eq!(TuiState::default().tile_theme, TileTheme::AsciiDetailed);
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
    fn map_viewport_follows_cursor_to_right_and_bottom_edges() {
        let view = viewport_test_view(10, 10);
        let area = Rect::new(0, 0, 16, 8);
        let visible_columns = visible_map_columns(area, map_cell_gap(area, &view), &view);
        let visible_rows =
            visible_map_rows(area, &view, TileTheme::Unicode, MapOverlayInput::Normal);
        let mut state = TuiState {
            cursor_x: 9,
            cursor_y: 9,
            tile_theme: TileTheme::Unicode,
            ..TuiState::default()
        };

        state.follow_cursor_in_map_viewport(&view, area);

        assert_eq!(
            (state.viewport_x, state.viewport_y),
            (
                view.map.width - visible_columns,
                view.map.height - visible_rows
            )
        );
    }

    #[test]
    fn map_viewport_follows_cursor_back_to_left_and_top_edges() {
        let view = viewport_test_view(10, 10);
        let area = Rect::new(0, 0, 16, 8);
        let mut state = TuiState {
            cursor_x: 1,
            cursor_y: 1,
            viewport_x: 5,
            viewport_y: 6,
            tile_theme: TileTheme::Unicode,
            ..TuiState::default()
        };

        state.follow_cursor_in_map_viewport(&view, area);

        assert_eq!((state.viewport_x, state.viewport_y), (1, 1));

        state.cursor_x = 0;
        state.cursor_y = 0;
        state.follow_cursor_in_map_viewport(&view, area);

        assert_eq!((state.viewport_x, state.viewport_y), (0, 0));
    }

    #[test]
    fn map_viewport_clamps_when_map_fits() {
        let view = viewport_test_view(3, 3);
        let area = Rect::new(0, 0, 80, 20);
        let mut state = TuiState {
            cursor_x: 2,
            cursor_y: 2,
            viewport_x: 9,
            viewport_y: 9,
            tile_theme: TileTheme::Unicode,
            ..TuiState::default()
        };

        state.follow_cursor_in_map_viewport(&view, area);

        assert_eq!((state.viewport_x, state.viewport_y), (0, 0));
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
    fn unicode_theme_maps_road_links_to_line_art_tiles() {
        let cases = [
            (road_cell(false, false, false, false), "──"),
            (road_cell(true, false, false, false), "│ "),
            (road_cell(false, true, false, false), "──"),
            (road_cell(false, false, true, false), "│ "),
            (road_cell(false, false, false, true), "─ "),
            (road_cell(true, false, true, false), "│ "),
            (road_cell(false, true, false, true), "──"),
            (road_cell(true, true, false, false), "└─"),
            (road_cell(true, false, false, true), "┘ "),
            (road_cell(false, true, true, false), "┌─"),
            (road_cell(false, false, true, true), "┐ "),
            (road_cell(true, true, false, true), "┴─"),
            (road_cell(false, true, true, true), "┬─"),
            (road_cell(true, true, true, false), "├─"),
            (road_cell(true, false, true, true), "┤ "),
            (road_cell(true, true, true, true), "┼─"),
        ];

        for (cell, expected) in cases {
            let glyph = TileTheme::Unicode.tile_for_cell(
                &cell,
                MapOverlayInput::Normal,
                false,
                PreviewState::None,
            );

            assert_eq!(glyph.tile, expected);
        }
    }

    #[test]
    fn unicode_road_tiles_are_two_allowed_width_safe_characters() {
        for mask in 0..16 {
            let cell = road_cell(
                mask & 0b1000 != 0,
                mask & 0b0100 != 0,
                mask & 0b0010 != 0,
                mask & 0b0001 != 0,
            );
            let glyph = TileTheme::Unicode.tile_for_cell(
                &cell,
                MapOverlayInput::Normal,
                false,
                PreviewState::None,
            );

            assert_eq!(glyph.tile.chars().count(), 2);
            assert!(glyph.tile.chars().all(is_allowed_unicode_road_char));
        }
    }

    #[test]
    fn ascii_themes_keep_plain_road_tiles() {
        let road = road_cell(true, true, true, true);

        assert_eq!(
            TileTheme::AsciiDetailed
                .tile_for_cell(&road, MapOverlayInput::Normal, false, PreviewState::None)
                .tile,
            "=="
        );
        assert_eq!(
            TileTheme::AsciiCompact
                .tile_for_cell(&road, MapOverlayInput::Normal, false, PreviewState::None)
                .tile,
            "=."
        );
    }

    #[test]
    fn unicode_residential_tiles_show_occupancy_buckets() {
        let cases = [
            (residential_cell_with_population(0, 10), "R░"),
            (residential_cell_with_population(1, 10), "R▒"),
            (residential_cell_with_population(5, 10), "R▓"),
            (residential_cell_with_population(10, 10), "R█"),
        ];

        for (cell, expected) in cases {
            let glyph = TileTheme::Unicode.tile_for_cell(
                &cell,
                MapOverlayInput::Normal,
                false,
                PreviewState::None,
            );

            assert_eq!(glyph.tile, expected);
        }
    }

    #[test]
    fn unicode_non_residential_buildings_show_level_buckets() {
        let cases = [
            (
                themed_cell(Some(BuildingKind::Commercial), 'C', Some(1), None, None, 0),
                "C░",
            ),
            (
                themed_cell(Some(BuildingKind::Commercial), 'C', Some(2), None, None, 0),
                "C▒",
            ),
            (
                themed_cell(Some(BuildingKind::Commercial), 'C', Some(3), None, None, 0),
                "C▓",
            ),
            (
                themed_cell(Some(BuildingKind::Industrial), 'I', Some(3), None, None, 0),
                "I▓",
            ),
            (
                themed_cell(Some(BuildingKind::PowerPlant), 'P', Some(2), None, None, 0),
                "T▒",
            ),
            (
                themed_cell(Some(BuildingKind::Park), 'P', Some(2), None, None, 0),
                "P▒",
            ),
        ];

        for (cell, expected) in cases {
            let glyph = TileTheme::Unicode.tile_for_cell(
                &cell,
                MapOverlayInput::Normal,
                false,
                PreviewState::None,
            );

            assert_eq!(glyph.tile, expected);
        }
    }

    #[test]
    fn unicode_building_problem_tiles_take_precedence_over_shades() {
        let unpowered = themed_cell(
            Some(BuildingKind::Residential),
            'R',
            Some(1),
            Some(false),
            None,
            9,
        );
        let disconnected = themed_cell(
            Some(BuildingKind::Commercial),
            'C',
            Some(3),
            Some(true),
            Some(false),
            9,
        );

        assert_eq!(
            TileTheme::Unicode
                .tile_for_cell(
                    &unpowered,
                    MapOverlayInput::Normal,
                    false,
                    PreviewState::None
                )
                .tile,
            "R-"
        );
        assert_eq!(
            TileTheme::Unicode
                .tile_for_cell(
                    &disconnected,
                    MapOverlayInput::Normal,
                    false,
                    PreviewState::None
                )
                .tile,
            "C!"
        );
    }

    #[test]
    fn unicode_building_tiles_are_two_allowed_width_safe_characters() {
        let cells = [
            residential_cell_with_population(0, 10),
            residential_cell_with_population(1, 10),
            residential_cell_with_population(5, 10),
            residential_cell_with_population(10, 10),
            themed_cell(Some(BuildingKind::Commercial), 'C', Some(1), None, None, 0),
            themed_cell(Some(BuildingKind::Commercial), 'C', Some(2), None, None, 0),
            themed_cell(Some(BuildingKind::Industrial), 'I', Some(3), None, None, 0),
            themed_cell(Some(BuildingKind::PowerPlant), 'P', Some(2), None, None, 0),
            themed_cell(Some(BuildingKind::Park), 'P', Some(2), None, None, 0),
        ];

        for cell in cells {
            let glyph = TileTheme::Unicode.tile_for_cell(
                &cell,
                MapOverlayInput::Normal,
                false,
                PreviewState::None,
            );

            assert_eq!(glyph.tile.chars().count(), 2);
            assert!(glyph.tile.chars().all(is_allowed_unicode_tile_char));
        }
    }

    #[test]
    fn ascii_detailed_building_tiles_are_unchanged() {
        let cases = [
            (residential_cell_with_population(10, 10), "R1"),
            (
                themed_cell(Some(BuildingKind::Commercial), 'C', Some(2), None, None, 0),
                "C2",
            ),
            (
                themed_cell(Some(BuildingKind::Industrial), 'I', Some(3), None, None, 0),
                "I3",
            ),
            (
                themed_cell(Some(BuildingKind::PowerPlant), 'P', Some(2), None, None, 0),
                "T2",
            ),
            (
                themed_cell(Some(BuildingKind::Park), 'P', Some(2), None, None, 0),
                "P2",
            ),
        ];

        for (cell, expected) in cases {
            let glyph = TileTheme::AsciiDetailed.tile_for_cell(
                &cell,
                MapOverlayInput::Normal,
                false,
                PreviewState::None,
            );

            assert_eq!(glyph.tile, expected);
        }
    }

    #[test]
    fn unicode_gap_suppression_joins_horizontal_road_cells() {
        let left = road_cell(false, true, false, false);
        let right = road_cell(false, false, false, true);
        let disconnected = road_cell(false, false, false, false);

        assert_eq!(
            map_gap_after_cell(
                " ",
                TileTheme::Unicode,
                MapOverlayInput::Normal,
                &left,
                Some(&right)
            ),
            "─"
        );
        assert_eq!(
            map_gap_after_cell(
                " ",
                TileTheme::AsciiDetailed,
                MapOverlayInput::Normal,
                &left,
                Some(&right)
            ),
            " "
        );
        assert_eq!(
            map_gap_after_cell(
                " ",
                TileTheme::Unicode,
                MapOverlayInput::Normal,
                &left,
                Some(&disconnected)
            ),
            " "
        );
    }

    #[test]
    fn normal_overlay_styles_residential_activity_intensity() {
        let low_activity = residential_cell_with_population(1, 10);
        let high_activity = residential_cell_with_population(9, 10);

        let low_glyph = TileTheme::AsciiDetailed.tile_for_cell(
            &low_activity,
            MapOverlayInput::Normal,
            false,
            PreviewState::None,
        );
        let high_glyph = TileTheme::AsciiDetailed.tile_for_cell(
            &high_activity,
            MapOverlayInput::Normal,
            false,
            PreviewState::None,
        );
        let population_overlay_glyph = TileTheme::AsciiDetailed.tile_for_cell(
            &high_activity,
            MapOverlayInput::Population,
            false,
            PreviewState::None,
        );

        assert!(low_glyph.style.add_modifier.contains(Modifier::DIM));
        assert!(high_glyph.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(low_glyph.style.fg, high_glyph.style.fg);
        assert!(
            !population_overlay_glyph
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn normal_overlay_styles_commercial_and_industrial_activity_intensity() {
        let low_commercial = zone_cell_with_land_value(BuildingKind::Commercial, 'C', 1);
        let high_commercial = zone_cell_with_land_value(BuildingKind::Commercial, 'C', 8);
        let high_industrial = zone_cell_with_land_value(BuildingKind::Industrial, 'I', 8);

        let low_commercial_glyph = TileTheme::AsciiDetailed.tile_for_cell(
            &low_commercial,
            MapOverlayInput::Normal,
            false,
            PreviewState::None,
        );
        let high_commercial_glyph = TileTheme::AsciiDetailed.tile_for_cell(
            &high_commercial,
            MapOverlayInput::Normal,
            false,
            PreviewState::None,
        );
        let high_industrial_glyph = TileTheme::AsciiDetailed.tile_for_cell(
            &high_industrial,
            MapOverlayInput::Normal,
            false,
            PreviewState::None,
        );
        let land_value_overlay_glyph = TileTheme::AsciiDetailed.tile_for_cell(
            &high_commercial,
            MapOverlayInput::LandValue,
            false,
            PreviewState::None,
        );

        assert!(
            low_commercial_glyph
                .style
                .add_modifier
                .contains(Modifier::DIM)
        );
        assert!(
            high_commercial_glyph
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            high_industrial_glyph
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(high_commercial_glyph.style.fg, Some(Color::Yellow));
        assert_eq!(high_industrial_glyph.style.fg, Some(Color::Magenta));
        assert!(
            !land_value_overlay_glyph
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn normal_overlay_keeps_non_zone_building_styles_unchanged() {
        for (kind, symbol) in [
            (BuildingKind::Road, '='),
            (BuildingKind::PowerPlant, 'P'),
            (BuildingKind::Park, 'P'),
        ] {
            let cell = zone_cell_with_land_value(kind, symbol, 9);
            let glyph = TileTheme::AsciiDetailed.tile_for_cell(
                &cell,
                MapOverlayInput::Normal,
                false,
                PreviewState::None,
            );

            assert_eq!(glyph.style, building_style(kind));
        }
    }

    #[test]
    fn normal_overlay_keeps_empty_cell_style_unchanged() {
        let empty = themed_cell(None, '.', None, None, None, 9);
        let glyph = TileTheme::AsciiDetailed.tile_for_cell(
            &empty,
            MapOverlayInput::Normal,
            false,
            PreviewState::None,
        );

        assert_eq!(glyph.style, empty_style());
    }

    #[test]
    fn unicode_theme_renders_aligned_connected_road_network() {
        let game = RegionalGame::single_region(3, 3).expect("regional test game");
        for (x, y) in [
            (0, 0),
            (1, 0),
            (2, 0),
            (0, 1),
            (2, 1),
            (0, 2),
            (1, 2),
            (2, 2),
        ] {
            game.build(RegionId(1), x, y, BuildingKind::Road)
                .expect("build road");
        }
        let state = TuiState {
            tile_theme: TileTheme::Unicode,
            ..TuiState::default()
        };

        let output = render_test_screen(&game, state);

        assert!(output.contains("0 ┌─────┐"));
        assert!(output.contains("1 │  .. │"));
        assert!(output.contains("2 └─────┘"));
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
        runtime.game = CityDriver::regional_with_size(10, 10).expect("regional UI driver");
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
        runtime.game = CityDriver::regional_with_size(10, 10).expect("regional UI driver");
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
        runtime.game = CityDriver::regional_with_size(3, 3).expect("regional UI driver");
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
    fn apply_action_cycles_tile_theme() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);
        runtime.state.tile_theme = TileTheme::AsciiDetailed;
        runtime.mark_clean();

        assert_eq!(
            runtime.apply_action(TuiAction::CycleTheme, now),
            TuiFlow::Continue
        );

        assert_eq!(runtime.state.tile_theme, TileTheme::AsciiCompact);
        assert_eq!(runtime.state.message, "Tile theme: ASCII Compact");
        assert!(runtime.dirty);
    }

    #[test]
    fn runtime_applies_due_auto_tick_when_running() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);
        runtime.game = CityDriver::regional_with_size(10, 10).expect("regional UI driver");
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
        runtime.game = CityDriver::regional_with_size(10, 10).expect("regional UI driver");
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
        let game = RegionalGame::single_region(10, 10).expect("regional test game");
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
        let game = RegionalGame::single_region(10, 10).expect("regional test game");
        let output = render_test_screen(&game, TuiState::default());
        let header = output
            .lines()
            .find(|line| line.contains("Status"))
            .expect("status panel header");

        assert!(header.contains("Time: | Year 1, Month 1, Week 1, Day 1, 00:00"));
    }

    #[test]
    fn status_time_spinner_advances_with_turns() {
        let game = RegionalGame::single_region(10, 10).expect("regional test game");
        game.tick_region(RegionId(1)).expect("tick region");
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
        let game = RegionalGame::single_region(10, 10).expect("regional test game");
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
    fn large_map_render_uses_cursor_following_viewport() {
        let game = RegionalGame::single_region(40, 30).expect("regional test game");
        let state = TuiState {
            cursor_x: 39,
            cursor_y: 29,
            ..TuiState::default()
        };

        let output = render_test_screen_with_size(&game, state, 100, 30);

        assert!(output.contains("│29 "));
        assert!(!output.contains("│ 0 "));
    }

    #[test]
    fn help_panel_contains_overlay_order_and_legends() {
        let game = RegionalGame::single_region(10, 10).expect("regional test game");
        let state = TuiState {
            show_help: true,
            ..TuiState::default()
        };

        let output = render_test_screen(&game, state);

        assert!(output.contains("Help"));
        assert!(output.contains("Space         Pause / resume automatic ticks | +/- speed"));
        assert!(output.contains("O Cycle Overlay"));
        assert!(output.contains("T             Cycle tile theme"));
        assert!(output.contains("Overlay order: Normal -> Power -> Pollution"));
        assert!(output.contains("Normal: .. Empty"));
        assert!(output.contains("Desirability: ! Bad"));
    }

    #[test]
    fn render_shows_running_status_when_auto_tick_is_enabled() {
        let game = RegionalGame::single_region(10, 10).expect("regional test game");
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
        let game = RegionalGame::single_region(10, 10).expect("regional test game");
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
            road_links: crate::interface::view::RoadLinks::default(),
            upgrade_level,
            job_assignments: Vec::new(),
            local_effects: crate::interface::view::LocalEffectsView {
                land_value: effect_value,
                pollution_pressure: effect_value,
                accessibility: 0,
                desirability: effect_value,
            },
        }
    }

    fn road_cell(north: bool, east: bool, south: bool, west: bool) -> CellView {
        let mut cell = themed_cell(Some(BuildingKind::Road), '=', None, None, None, 0);
        cell.road_links = RoadLinks {
            north,
            east,
            south,
            west,
        };
        cell
    }

    fn residential_cell_with_population(population: i32, max_population: i32) -> CellView {
        let mut cell = themed_cell(Some(BuildingKind::Residential), 'R', Some(1), None, None, 0);
        cell.population = Some(population);
        cell.max_population = Some(max_population);
        cell
    }

    fn zone_cell_with_land_value(kind: BuildingKind, symbol: char, land_value: i32) -> CellView {
        let mut cell = themed_cell(Some(kind), symbol, Some(1), None, None, 0);
        cell.local_effects.land_value = land_value;
        cell
    }

    fn is_allowed_unicode_tile_char(value: char) -> bool {
        value.is_ascii()
            || matches!(
                value,
                '─' | '│'
                    | '┌'
                    | '┐'
                    | '└'
                    | '┘'
                    | '├'
                    | '┤'
                    | '┬'
                    | '┴'
                    | '┼'
                    | '░'
                    | '▒'
                    | '▓'
                    | '█'
            )
    }

    fn is_allowed_unicode_road_char(value: char) -> bool {
        matches!(
            value,
            ' ' | '─' | '│' | '┌' | '┐' | '└' | '┘' | '├' | '┤' | '┬' | '┴' | '┼'
        )
    }

    fn viewport_test_view(width: usize, height: usize) -> GameView {
        RegionalGame::single_region(width, height)
            .expect("regional viewport test game")
            .selected_region_view()
            .expect("viewport test view")
    }

    fn render_test_screen(game: &RegionalGame, state: TuiState) -> String {
        render_test_screen_with_size(game, state, 120, 36)
    }

    fn render_test_screen_with_size(
        game: &RegionalGame,
        state: TuiState,
        width: u16,
        height: u16,
    ) -> String {
        let terminal = render_test_terminal_with_size(game, state, width, height);
        buffer_text(terminal.backend().buffer())
    }

    fn render_test_terminal(game: &RegionalGame, state: TuiState) -> Terminal<TestBackend> {
        render_test_terminal_with_size(game, state, 120, 36)
    }

    fn render_test_terminal_with_size(
        game: &RegionalGame,
        mut state: TuiState,
        width: u16,
        height: u16,
    ) -> Terminal<TestBackend> {
        let view = game
            .selected_region_view_with_overlay(state.current_overlay)
            .expect("selected region view");
        state.clamp_cursor(&view);
        let inspect = game
            .inspect_region(RegionId(1), state.cursor_x, state.cursor_y)
            .expect("inspect selected region");
        let preview = game
            .preview_build(
                RegionId(1),
                state.cursor_x,
                state.cursor_y,
                state.selected_build,
            )
            .expect("preview selected region");

        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &view, &inspect, &preview, &mut state))
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
