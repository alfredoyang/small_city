//! Panel-based ratatui terminal frontend built only from facade view models.

use std::cmp::Ordering;
use std::collections::HashSet;
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
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{Frame, Terminal};

use crate::core::regions::RegionId;
use crate::interface::events::CommandResult;
use crate::interface::input::{BuildingKind, MapOverlayInput};
use crate::interface::view::{
    BuildPreviewView, CellView, CitizenDetailView, CitizenRelation, DemandLevel, GameView,
    InspectDetailsView, InspectFlag, InspectView, RoadTravelerEndpointView,
};
use crate::ui::city_driver::CityDriver;
use crate::ui::tui_input::{TuiAction, map_key_event};

const DEFAULT_SAVE_FILE: &str = "city1";
const AUTO_ADVANCE_INTERVAL: Duration = Duration::from_millis(500);
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
    /// When set, a modal asks the player to confirm quitting (with a save option) instead of
    /// exiting immediately. Cleared on cancel or once the quit is carried out.
    quit_confirm: bool,
    /// When set, a modal lists the citizens of the selected building (residents for Residential,
    /// local workers for Commercial/Industrial). Opened by Enter on a populated zone.
    citizen_panel: bool,
    /// Selected (highlighted) row in the open citizen roster — the in-list cursor. Clamped to the
    /// roster length by the key handler; the `Table` auto-scrolls so the selection stays visible.
    citizen_selected: usize,
    /// Cross-region commuters staffing the open workplace roster, fetched once when the panel opens
    /// (and refreshed on tick), never per frame. Empty for residential rosters and workplaces with
    /// no remote staff. Rendered below the local workers, region-tagged via `LivesAt { region }`.
    citizen_remote: Vec<CitizenDetailView>,
    /// When set, a modal lists the travelers on the selected road cell: local citizens,
    /// local trucks, plus visiting bodies' endpoint summaries.
    road_traveler_panel: bool,
    /// Local travelers' detail rows, fetched once when the panel opens.
    /// ponytail: snapshot-only, not refreshed on tick like `citizen_remote` — tokens
    /// move every sub-tick, so a live refresh would need a facade round-trip that
    /// often; close/reopen to refresh. Revisit if playtesting shows this is confusing.
    road_traveler_locals: Vec<CitizenDetailView>,
    /// Local factory trucks on the inspected road cell.
    road_traveler_trucks: usize,
    /// Visiting bodies' endpoint summaries, fetched once alongside `road_traveler_locals`.
    road_traveler_visitors: Vec<RoadTravelerEndpointView>,
    /// Whether the chrome panels (header bar, tool strip, City HUD, legend) may use emoji icons.
    /// The map grid is always emoji-free; only these panels fall back to ASCII on bare terminals.
    use_emoji: bool,
    /// Snapshot of headline stats at the last turn boundary, used to draw HUD trend arrows.
    hud_prev: Option<HudStats>,
    /// Direction each headline stat moved over the last turn (▲/▼/→). Stable within a turn.
    hud_trend: HudTrend,
    /// Transient "build juice": a short-lived flash on the last edited cell plus its money delta.
    build_flash: Option<BuildFlash>,
    /// Paint mode: while on, moving the cursor lays the selected build tool along the path so roads
    /// and zones can be "drawn" instead of placed one cell at a time.
    paint_mode: bool,
    /// Whether live map animation (power pulse, industrial smoke, cursor pulse) is enabled. Only
    /// runs while the simulation is also running, so a paused city is perfectly still.
    animate: bool,
    /// UI-only animation frame counter (advanced off a wall-clock timer, never the simulation), so
    /// animation never affects determinism. Drives which phase of a pulse a tile shows.
    anim_frame: u64,
}

/// How often the animation frame advances. The map repaints on this cadence only while animating.
const ANIM_INTERVAL: Duration = Duration::from_millis(300);

/// How long a build/bulldoze flash stays on screen before it fades.
const FLASH_DURATION: Duration = Duration::from_millis(900);

/// A transient confirmation flash for the most recent successful map edit.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BuildFlash {
    x: usize,
    y: usize,
    /// Money delta readout, e.g. "+$100" / "-$50".
    text: String,
    color: Color,
    expires_at: Instant,
}

impl BuildFlash {
    fn is_active(&self, now: Instant) -> bool {
        now < self.expires_at
    }
}

/// Headline city stats captured once per turn so the HUD can show whether they are rising/falling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HudStats {
    turn: u32,
    money: i32,
    population: i32,
    happiness: i32,
}

/// The direction each headline stat moved across the last turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HudTrend {
    money: Ordering,
    population: Ordering,
    happiness: Ordering,
}

impl Default for HudTrend {
    fn default() -> Self {
        Self {
            money: Ordering::Equal,
            population: Ordering::Equal,
            happiness: Ordering::Equal,
        }
    }
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
            quit_confirm: false,
            citizen_panel: false,
            citizen_selected: 0,
            citizen_remote: Vec::new(),
            road_traveler_panel: false,
            road_traveler_locals: Vec::new(),
            road_traveler_trucks: 0,
            road_traveler_visitors: Vec::new(),
            use_emoji: true,
            hud_prev: None,
            hud_trend: HudTrend::default(),
            build_flash: None,
            paint_mode: false,
            animate: true,
            anim_frame: 0,
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

    /// P7d: wall-clock period between auto movement sub-ticks (`advance`). At 1× the
    /// sim advances ~2 sub-ticks/sec (one game hour ≈ 3 s); 2×/4× scale it.
    fn interval(self) -> Duration {
        match self {
            Self::One => AUTO_ADVANCE_INTERVAL,
            Self::Two => Duration::from_millis(250),
            Self::Four => Duration::from_millis(125),
        }
    }

    /// How much faster than 1x this speed runs. Animation cadence scales by the same factor so the
    /// city visibly speeds up at 2x / 4x.
    fn multiplier(self) -> u32 {
        match self {
            Self::One => 1,
            Self::Two => 2,
            Self::Four => 4,
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

        // Build preview tints the targeted cell with a bright background so it reads as a
        // highlighted lot on the zoning map (basic named colors stay legible on 16-color terminals).
        if preview_state != PreviewState::None {
            glyph.style = match preview_state {
                PreviewState::Valid => glyph.style.bg(Color::Green).fg(Color::Black),
                PreviewState::Invalid => glyph.style.bg(Color::Red).fg(Color::White),
                PreviewState::None => glyph.style,
            }
            .add_modifier(Modifier::BOLD);
        }

        // The cursor wins over any preview tint: a bright white lot marker that never changes the
        // tile width.
        if is_cursor {
            glyph.style = glyph
                .style
                .bg(Color::White)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD);
        }

        glyph
    }

    fn normal_tile(self, cell: &CellView) -> TileGlyph {
        // Non-anchor cells of a healthy multi-cell building render as a dim continuation fill so
        // the whole footprint reads as one lot (the icon shows only on the anchor cell). A problem
        // (unpowered/disconnected) is building-wide, so those cells fall through to the normal path
        // and keep their `R-`/`C!` marker on every footprint cell. Roads are always 1x1 anchors.
        if cell.building.is_some()
            && !cell.footprint_anchor
            && building_problem_marker(cell).is_none()
        {
            return self.footprint_fill_tile(cell);
        }

        // The City (Unicode) theme paints a muted-earth ground plus per-zone background tints so
        // the map reads like a SimCity zoning plan. The ASCII fallbacks stay foreground-only for
        // bare terminals.
        match self {
            TileTheme::AsciiCompact => TileGlyph {
                tile: format!("{}.", cell.symbol),
                style: cell_base_style(cell),
            },
            TileTheme::AsciiDetailed => TileGlyph {
                tile: ascii_detailed_normal_tile(cell),
                style: cell_base_style(cell),
            },
            TileTheme::Unicode => TileGlyph {
                tile: unicode_normal_tile(cell),
                style: city_cell_style(cell),
            },
        }
    }

    /// Continuation fill for a non-anchor footprint cell: a dim two-column block that
    /// sits on the building's (size-brightened) zone background, so a 2x1 / 2x2 reads
    /// as one lot rather than several identical icons. Width stays two columns.
    fn footprint_fill_tile(self, cell: &CellView) -> TileGlyph {
        match self {
            TileTheme::Unicode => TileGlyph {
                tile: "░░".to_string(),
                style: city_cell_style(cell),
            },
            TileTheme::AsciiCompact | TileTheme::AsciiDetailed => TileGlyph {
                tile: "::".to_string(),
                style: cell_base_style(cell),
            },
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
            // The City theme draws SimCity-style letters (ϟ power, ♣ park); the ASCII fallbacks
            // keep the older T/P markers, so the legend follows the theme.
            MapOverlayInput::Normal => match self {
                TileTheme::Unicode => {
                    ".. Empty | ══ Road | R Residential | C Commercial | I Industrial | ϟ Power | ♣ Park"
                }
                TileTheme::AsciiCompact | TileTheme::AsciiDetailed => {
                    ".. Empty | == Road | R1/R2 Residential | C1 Commercial | I1 Industrial | T1 Power | P1 Park"
                }
            },
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
            TileTheme::Unicode => "City",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TileGlyph {
    pub tile: String,
    pub style: Style,
}

/// P4: overlay a moving-citizen dot on a tile. Drawn only in the Normal overlay
/// and never on the cursor cell (the cursor highlight wins); when drawn it becomes
/// a two-column yellow bold `•·`, keeping the tile's existing background.
fn overlay_traveler_dot(
    glyph: &mut TileGlyph,
    overlay: MapOverlayInput,
    has_traveler: bool,
    is_cursor: bool,
) {
    if has_traveler && !is_cursor && overlay == MapOverlayInput::Normal {
        glyph.tile = "•·".to_string();
        glyph.style = glyph.style.fg(Color::Yellow).add_modifier(Modifier::BOLD);
    }
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
    /// Refreshes the HUD trend arrows once per turn. Comparing against the previous turn's snapshot
    /// (not the previous frame) keeps the arrows stable while the turn is paused. The first observed
    /// turn just seeds the baseline so nothing spikes from zero.
    fn update_hud_trend(&mut self, turn: u32, money: i32, population: i32, happiness: i32) {
        let current = HudStats {
            turn,
            money,
            population,
            happiness,
        };
        match self.hud_prev {
            Some(prev) if prev.turn != current.turn => {
                self.hud_trend = HudTrend {
                    money: current.money.cmp(&prev.money),
                    population: current.population.cmp(&prev.population),
                    happiness: current.happiness.cmp(&prev.happiness),
                };
                self.hud_prev = Some(current);
            }
            None => self.hud_prev = Some(current),
            _ => {}
        }
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
                "Simulation running at {}: advancing every {}",
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
    /// When `true` (only for a Save raised from the quit dialog), a successful save quits the app.
    then_quit: bool,
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
    /// Set when the player has confirmed a quit (directly, or after a save-and-quit). The event
    /// loop checks it after each modal key so it can exit cleanly without an extra action enum.
    pending_quit: bool,
    /// Wall-clock instant of the last animation frame advance (the accumulator cursor).
    last_anim_at: Instant,
}

impl TuiRuntime {
    #[cfg(test)]
    fn new(now: Instant) -> Self {
        Self::launch(now).expect("regional TUI runtime")
    }

    fn launch(now: Instant) -> Result<Self, crate::ui::city_driver::CityDriverError> {
        Ok(Self {
            game: CityDriver::regional_multi_region()?,
            state: TuiState::default(),
            next_auto_tick: now + RunSpeed::One.interval(),
            dirty: true,
            pending_quit: false,
            last_anim_at: now,
        })
    }

    /// Whether live animation should currently advance: enabled and the simulation is running.
    fn animation_active(&self) -> bool {
        self.state.animate && self.state.is_running
    }

    /// Time between animation frames at the current run speed: faster at 2x / 4x.
    fn anim_interval(&self) -> Duration {
        ANIM_INTERVAL / self.state.run_speed.multiplier()
    }

    /// Advances the animation frame, accumulating elapsed time so the cadence tracks the run speed.
    /// When inactive it just keeps the cursor at `now`, so re-enabling never bursts a backlog.
    fn tick_animation(&mut self, now: Instant) {
        if !self.animation_active() {
            self.last_anim_at = now;
            return;
        }
        let interval = self.anim_interval();
        let elapsed = now.saturating_duration_since(self.last_anim_at);
        if elapsed < interval {
            return;
        }
        let steps = (elapsed.as_millis() / interval.as_millis()) as u64;
        self.state.anim_frame = self.state.anim_frame.wrapping_add(steps);
        self.last_anim_at += interval * steps as u32;
        self.dirty = true;
    }

    fn apply_due_auto_tick(&mut self, now: Instant) {
        if self.state.is_running && now >= self.next_auto_tick {
            // P7d: each auto step is one movement sub-tick (smooth cell-by-cell). A
            // `Some` result means the hourly economy fired (every 6th sub-tick) or an
            // error — surface it; on a movement-only sub-tick keep the status line.
            if let Some(result) = self.game.advance() {
                self.state.message = result.message();
                self.refresh_citizen_remote();
            }
            self.next_auto_tick = now + self.state.run_speed.interval();
            self.dirty = true;
        }
    }

    fn poll_timeout(&self, now: Instant) -> Duration {
        let mut timeout = poll_timeout(self.state.is_running, self.next_auto_tick, now);
        // While a build flash is showing, wake at its expiry so the redraw can clear it.
        if let Some(flash) = &self.state.build_flash
            && flash.is_active(now)
        {
            timeout = timeout.min(flash.expires_at.saturating_duration_since(now));
        }
        // While animating, wake on the (speed-scaled) animation cadence so the next frame can paint.
        if self.animation_active() {
            timeout = timeout.min(self.anim_interval());
        }
        timeout
    }

    /// Drops an expired build flash and asks for a repaint so it disappears on time.
    fn expire_build_flash(&mut self, now: Instant) {
        if let Some(flash) = &self.state.build_flash
            && !flash.is_active(now)
        {
            self.state.build_flash = None;
            self.dirty = true;
        }
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
                            Ok(()) => {
                                // A save raised from the quit dialog exits once it succeeds; a
                                // failed save keeps the app open so progress is never lost silently.
                                if prompt.then_quit {
                                    self.pending_quit = true;
                                    format!("Saved {filename}; quitting")
                                } else {
                                    format!("Saved {filename}")
                                }
                            }
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

    /// Remote (cross-region) staff for an open workplace roster. Residential cells
    /// and non-workplaces have none, so this avoids the cross-worker fan-out there.
    fn fetch_citizen_remote(
        &mut self,
        inspect: &InspectView,
        x: usize,
        y: usize,
    ) -> Vec<CitizenDetailView> {
        let is_workplace = matches!(
            inspect.details,
            Some(InspectDetailsView::Commercial { .. })
                | Some(InspectDetailsView::Industrial { .. })
        );
        if is_workplace {
            self.game.remote_workers_at(x, y)
        } else {
            Vec::new()
        }
    }

    /// Refreshes the cached remote roster after a tick while the panel stays open,
    /// so commuter changes show without a per-frame cross-region query.
    fn refresh_citizen_remote(&mut self) {
        if !self.state.citizen_panel {
            return;
        }
        let (x, y) = (self.state.cursor_x, self.state.cursor_y);
        let inspect = self.game.inspect(x, y);
        self.state.citizen_remote = self.fetch_citizen_remote(&inspect, x, y);
    }

    /// Jump target (region + cell) of the row the in-list cursor is on, read from
    /// the combined roster in render order (local workers, then remote commuters).
    ///
    /// The index is clamped the same way `render_citizen_panel` clamps the highlight
    /// (to the last row), so a roster that shrank while the panel was open jumps from
    /// the row the cursor actually shows, not a stale out-of-range index.
    fn selected_citizen_target(&mut self) -> Option<(Option<RegionId>, usize, usize)> {
        let inspect = self.game.inspect(self.state.cursor_x, self.state.cursor_y);
        let total = inspect.roster.len() + self.state.citizen_remote.len();
        let index = self.state.citizen_selected.min(total.saturating_sub(1));
        let citizen = inspect
            .roster
            .iter()
            .chain(self.state.citizen_remote.iter())
            .nth(index)?;
        relation_target(citizen.relation)
    }

    /// Closes the roster and moves the map cursor to the selected citizen's related
    /// cell (a resident's workplace, or a worker's home), switching the selected
    /// region first when that cell is in another region.
    fn jump_to_selected_citizen(&mut self) {
        self.state.citizen_panel = false;
        let Some((region, x, y)) = self.selected_citizen_target() else {
            self.state.message = "No location to jump to.".to_string();
            return;
        };
        if let Some(region) = region {
            self.game.select_region(region);
            self.state.region_label = self.game.region_label();
        }
        self.state.cursor_x = x;
        self.state.cursor_y = y;
        self.state.message = format!("Jumped to ({x},{y}).");
    }

    /// Handles a key while the citizen roster modal is open. Returns `true` when the modal
    /// consumed the key (so it should not fall through to a gameplay action).
    ///
    /// ```text
    ///   [↑]/[↓]  move the in-list cursor (clamped to the roster length)
    ///   [Enter]  jump: close, move the map cursor to the selected citizen's cell
    ///   [Esc]/[q]  close the panel without moving
    /// ```
    fn handle_citizen_panel_key(&mut self, key: KeyEvent) -> bool {
        if !self.state.citizen_panel {
            return false;
        }

        match key.code {
            KeyCode::Up | KeyCode::Char('w') | KeyCode::Char('W') => {
                self.state.citizen_selected = self.state.citizen_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('s') => {
                // Clamp against the live roster (local workers + cached remote commuters) so the
                // cursor can never run past the last citizen.
                let count = self
                    .game
                    .inspect(self.state.cursor_x, self.state.cursor_y)
                    .roster
                    .len()
                    + self.state.citizen_remote.len();
                if self.state.citizen_selected + 1 < count {
                    self.state.citizen_selected += 1;
                }
            }
            KeyCode::Enter => {
                self.jump_to_selected_citizen();
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                self.state.citizen_panel = false;
            }
            _ => {}
        }

        self.dirty = true;
        true
    }

    /// Handles a key while the road-traveler modal is open. Returns `true` when the modal
    /// consumed the key. A static snapshot view: no in-list cursor or jump, just close.
    fn handle_road_traveler_panel_key(&mut self, key: KeyEvent) -> bool {
        if !self.state.road_traveler_panel {
            return false;
        }

        if matches!(
            key.code,
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q')
        ) {
            self.state.road_traveler_panel = false;
        }

        self.dirty = true;
        true
    }

    /// Handles a key while the quit-confirmation modal is open. Returns `true` when the modal
    /// consumed the key (so it should not fall through to a gameplay action). The actual exit is
    /// signalled via `pending_quit` and carried out by the event loop.
    ///
    /// ```text
    ///   [S] save & quit ─► open Save prompt (then_quit) ─► save ok ─► pending_quit
    ///   [Q]/[Enter] quit ─────────────────────────────────────────► pending_quit
    ///   [Esc]/[N]/[C] cancel ─► close modal, stay in game
    /// ```
    fn handle_quit_confirm_key(&mut self, key: KeyEvent) -> bool {
        if !self.state.quit_confirm {
            return false;
        }

        match key.code {
            KeyCode::Char('s') | KeyCode::Char('S') => {
                // Route through the normal Save filename prompt, flagged to quit once it succeeds.
                self.state.quit_confirm = false;
                self.state.prompt = Some(PromptState {
                    kind: PromptKind::Save,
                    input: String::new(),
                    then_quit: true,
                });
            }
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Enter => {
                self.pending_quit = true;
            }
            KeyCode::Esc
            | KeyCode::Char('n')
            | KeyCode::Char('N')
            | KeyCode::Char('c')
            | KeyCode::Char('C') => {
                self.state.quit_confirm = false;
                self.state.message = "Quit cancelled".to_string();
            }
            _ => {}
        }

        self.dirty = true;
        true
    }

    fn apply_action(&mut self, action: TuiAction, now: Instant) -> TuiFlow {
        match action {
            TuiAction::MoveUp => {
                self.move_cursor(0, -1);
                self.paint_if_active(now);
            }
            TuiAction::MoveDown => {
                self.move_cursor(0, 1);
                self.paint_if_active(now);
            }
            TuiAction::MoveLeft => {
                self.move_cursor(-1, 0);
                self.paint_if_active(now);
            }
            TuiAction::MoveRight => {
                self.move_cursor(1, 0);
                self.paint_if_active(now);
            }
            TuiAction::SelectBuild(kind) => {
                self.state.selected_build = kind;
                self.state.message = format!("Selected {}", kind.label());
            }
            TuiAction::Build => {
                let (x, y) = (self.state.cursor_x, self.state.cursor_y);
                let before = self.current_money();
                let result = self.game.build(x, y, self.state.selected_build);
                self.finish_map_command(x, y, before, result, now);
            }
            TuiAction::EnterCell => {
                // Context-sensitive: a populated zone opens its citizen roster; a road
                // with travelers opens the traveler panel; anything else (empty land,
                // an empty road, power, park) behaves as Build.
                let (x, y) = (self.state.cursor_x, self.state.cursor_y);
                let inspect = self.game.inspect(x, y);
                if cell_has_roster(&inspect) {
                    self.state.citizen_panel = true;
                    self.state.citizen_selected = 0;
                    self.state.citizen_remote = self.fetch_citizen_remote(&inspect, x, y);
                    self.state.message = "Citizen roster (↑/↓ select · Esc close)".to_string();
                    self.dirty = true;
                } else if inspect.road_traveler_count > 0 {
                    let seed = self.game.road_traveler_panel_seed(x, y);
                    self.state.road_traveler_panel = true;
                    self.state.road_traveler_locals = seed.local_details;
                    self.state.road_traveler_trucks = seed.local_truck_count;
                    self.state.road_traveler_visitors = seed.visitor_endpoints;
                    self.state.message = "Traveler details (Esc close)".to_string();
                    self.dirty = true;
                } else {
                    let before = self.current_money();
                    let result = self.game.build(x, y, self.state.selected_build);
                    self.finish_map_command(x, y, before, result, now);
                }
            }
            TuiAction::Replace => {
                let (x, y) = (self.state.cursor_x, self.state.cursor_y);
                let before = self.current_money();
                let result = self.game.replace(x, y, self.state.selected_build);
                self.finish_map_command(x, y, before, result, now);
            }
            TuiAction::Upgrade => {
                let (x, y) = (self.state.cursor_x, self.state.cursor_y);
                let before = self.current_money();
                let result = self.game.upgrade(x, y);
                self.finish_map_command(x, y, before, result, now);
            }
            TuiAction::Bulldoze => {
                let (x, y) = (self.state.cursor_x, self.state.cursor_y);
                let before = self.current_money();
                let result = self.game.bulldoze(x, y);
                self.finish_map_command(x, y, before, result, now);
            }
            TuiAction::Tick => self.manual_tick(),
            TuiAction::Save => {
                self.state.prompt = Some(PromptState {
                    kind: PromptKind::Save,
                    input: String::new(),
                    then_quit: false,
                });
            }
            TuiAction::Load => {
                self.state.prompt = Some(PromptState {
                    kind: PromptKind::Load,
                    input: String::new(),
                    then_quit: false,
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
            TuiAction::TogglePaint => {
                self.state.paint_mode = !self.state.paint_mode;
                self.state.message = if self.state.paint_mode {
                    format!(
                        "Paint mode ON: move to lay {}",
                        self.state.selected_build.label()
                    )
                } else {
                    "Paint mode OFF".to_string()
                };
            }
            TuiAction::ToggleAnimation => {
                self.state.animate = !self.state.animate;
                self.state.message = if self.state.animate {
                    "Animation ON (runs while the city is running)".to_string()
                } else {
                    "Animation OFF".to_string()
                };
            }
            TuiAction::RequestQuit => {
                // Esc / q / Q never exit straight away: open the confirm-and-save modal instead.
                self.state.quit_confirm = true;
                self.state.message = "Quit? S save & quit · Q quit · Esc cancel".to_string();
            }
            TuiAction::Quit => return TuiFlow::Quit,
            TuiAction::None => return TuiFlow::Continue,
        }

        self.dirty = true;
        TuiFlow::Continue
    }

    /// Current city money, read through the public view (no ECS access).
    fn current_money(&mut self) -> i32 {
        self.game
            .view_with_overlay(self.state.current_overlay)
            .status
            .money
    }

    /// Records a map command's message and, on success, raises a transient "build juice" flash on
    /// the edited cell showing the money delta. The flash is pure UI state driven by a timer; it
    /// never feeds back into the simulation.
    fn finish_map_command(
        &mut self,
        x: usize,
        y: usize,
        before_money: i32,
        result: CommandResult,
        now: Instant,
    ) {
        self.state.message = result.message();
        if !result.success {
            return;
        }
        let delta = self.current_money() - before_money;
        let (text, color) = match delta.cmp(&0) {
            Ordering::Greater => (format!("+${delta}"), Color::Green),
            Ordering::Less => (format!("-${}", -delta), Color::Yellow),
            Ordering::Equal => ("✔".to_string(), Color::Green),
        };
        self.state.build_flash = Some(BuildFlash {
            x,
            y,
            text,
            color,
            expires_at: now + FLASH_DURATION,
        });
    }

    /// In paint mode, lays the selected tool on the cell the cursor just entered. Re-uses the
    /// normal build path (so it flashes and reports cost); placing on an occupied/identical cell
    /// just fails quietly and the player keeps drawing.
    fn paint_if_active(&mut self, now: Instant) {
        if !self.state.paint_mode {
            return;
        }
        let (x, y) = (self.state.cursor_x, self.state.cursor_y);
        let before = self.current_money();
        let result = self.game.build(x, y, self.state.selected_build);
        self.finish_map_command(x, y, before, result, now);
    }

    fn move_cursor(&mut self, dx: isize, dy: isize) {
        let view = self.game.view_with_overlay(self.state.current_overlay);
        let (cursor_x, cursor_y) = self.game.move_cursor_across_region(
            self.state.cursor_x,
            self.state.cursor_y,
            dx,
            dy,
            &view,
        );
        self.state.cursor_x = cursor_x;
        self.state.cursor_y = cursor_y;
        self.state.region_label = self.game.region_label();
    }

    fn manual_tick(&mut self) {
        if self.state.is_running {
            self.state.message = "Pause before stepping manually".to_string();
            return;
        }

        // P7d: one press advances a single 10-minute movement sub-tick (option a).
        // Show the hourly economy message when this sub-tick crossed an hour boundary
        // (every 6th); otherwise note the fine step.
        if let Some(result) = self.game.advance() {
            self.state.message = result.message();
            self.refresh_citizen_remote();
        } else {
            self.state.message = "Advanced 10 minutes".to_string();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TuiFlow {
    Continue,
    Quit,
}

/// Runs the ratatui frontend on the regional facade.
pub fn run() -> io::Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    let mut runtime =
        TuiRuntime::launch(Instant::now()).map_err(|error| io::Error::other(error.to_string()))?;
    runtime.state.tile_theme = default_tile_theme();
    // Chrome panels use emoji only when the locale advertises UTF-8 (matching the tile theme).
    runtime.state.use_emoji = locale_supports_unicode(current_terminal_locale().as_deref());

    loop {
        runtime.apply_due_auto_tick(Instant::now());
        runtime.expire_build_flash(Instant::now());
        runtime.tick_animation(Instant::now());

        if runtime.dirty {
            // Pull all render data through the public API only when the screen needs repainting.
            // This keeps the TUI from caching or reaching into ECS internals while avoiding idle
            // redraw work.
            let view = runtime
                .game
                .view_with_overlay(runtime.state.current_overlay);
            runtime.state.region_label = runtime.game.region_label();
            runtime.state.update_hud_trend(
                view.status.turn,
                view.status.money,
                view.status.population,
                view.status.happiness,
            );
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
            // A save raised by "save & quit" sets this once the write succeeds.
            if runtime.pending_quit {
                return Ok(());
            }
            continue;
        }

        // The quit-confirmation modal is also fully modal while open.
        if runtime.handle_quit_confirm_key(key) {
            if runtime.pending_quit {
                return Ok(());
            }
            continue;
        }

        // The citizen roster modal consumes its own keys while open.
        if runtime.handle_citizen_panel_key(key) {
            continue;
        }

        // The road-traveler modal consumes its own keys while open.
        if runtime.handle_road_traveler_panel_key(key) {
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

    // The screen is a SimCity-style stack: a one-line header bar, then three horizontal bands.
    // `Constraint::Min` gives the map the flexible space; fixed-height panels stay readable.
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(12),
            Constraint::Length(8),
            Constraint::Length(5),
        ])
        .split(root);
    let header = vertical[0];
    // The top band carries the left tool strip, the map, and the inspect panel.
    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(10), Constraint::Min(1)])
        .split(vertical[1]);
    let tool_strip = top[0];
    let map_inspect = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(top[1]);
    let map_area = map_inspect[0];
    let inspect_area = map_inspect[1];
    let middle = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(vertical[2]);

    // The map viewport depends on the panel `Rect`, which is only known during draw. Keep that
    // layout-derived scroll state in `TuiState` so cursor movement can follow the visible window.
    state.follow_cursor_in_map_viewport(view, map_area);

    render_header_bar(frame, header, view, state);
    render_tool_strip(frame, tool_strip, state);
    render_map(frame, map_area, view, preview, state);
    render_selected_cell(frame, inspect_area, inspect);
    render_city_hud(frame, middle[0], view, state);
    render_build_preview(frame, middle[1], view, preview, state);
    render_messages(frame, vertical[3], state);

    // Modal panels are rendered after the base layout so they appear on top. `Clear` inside the
    // modal renderers blanks the covered area before drawing the popup border and text.
    if state.show_help {
        render_help(frame, root);
    }
    if let Some(prompt) = &state.prompt {
        render_prompt(frame, root, prompt);
    }
    if state.quit_confirm {
        render_quit_confirm(frame, root);
    }
    if state.citizen_panel {
        render_citizen_panel(
            frame,
            root,
            inspect,
            &state.citizen_remote,
            state.citizen_selected,
        );
    }
    if state.road_traveler_panel {
        render_road_traveler_panel(
            frame,
            root,
            inspect,
            &state.road_traveler_locals,
            state.road_traveler_trucks,
            &state.road_traveler_visitors,
        );
    }
}

/// Modal shown when the player presses Esc/Q to quit. Double-confirms and offers to save first so
/// progress is never lost by an accidental keypress.
fn render_quit_confirm(frame: &mut Frame<'_>, area: Rect) {
    let popup = centered_rect(50, 30, area);
    let lines = vec![
        Line::from(Span::styled(
            "Quit Small City?",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Unsaved progress will be lost."),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "S",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Save & Quit    "),
            Span::styled(
                "Q",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Quit    "),
            Span::styled(
                "Esc",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Cancel"),
        ]),
    ];

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title("Confirm Quit").borders(Borders::ALL))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        popup,
    );
}

/// True when the inspected cell is a zone whose citizens we can list (residents
/// for Residential, local workers for Commercial/Industrial).
fn cell_has_roster(inspect: &InspectView) -> bool {
    matches!(
        inspect.details,
        Some(InspectDetailsView::Residential { .. })
            | Some(InspectDetailsView::Commercial { .. })
            | Some(InspectDetailsView::Industrial { .. })
    )
}

/// Modal roster of the selected building's citizens.
///
/// ```text
///  Residential          -> residents,      each row: arrival status and where they work
///  Commercial/Industrial -> local workers,  each row: where they live
/// ```
///
/// `scroll` is the index of the first row shown; it is clamped here so a roster
/// that shrank while the panel was open never leaves a blank window.
/// Renders the citizen roster as a `Table`: a fixed header row on top, one aligned row per citizen,
/// and the `selected` row highlighted as an in-list cursor. The `TableState` is rebuilt each frame
/// from `selected` (offset 0) — ratatui's table render scrolls the viewport so the selection stays
/// visible, so no persistent scroll offset is needed.
///
/// ```text
/// ┌ Workers at (1,2) — 3 worker(s) · ↑/↓ · Esc close ───┐
/// │ #   Age  Happy  $    Lives at                       │  (header: "Works at" on
/// │ #1  27   72     $14  (0,1)                           │   residential, "Lives at"
/// │>#2  34   41     $3   region 1 (4,11)        (cursor) │   on a workplace; remote
/// │ 1 local · 1 remote  ← footnote on workplaces only    │   commuters tagged by region)
/// └─────────────────────────────────────────────────────┘
/// ```
///
/// On a workplace the body lists local workers first, then the cross-region
/// commuters in `remote` (already merged/ordered by the facade); the `selected`
/// cursor and clamp run over the combined list.
fn render_citizen_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    inspect: &InspectView,
    remote: &[CitizenDetailView],
    selected: usize,
) {
    let popup = centered_rect(60, 60, area);
    let is_workplace = matches!(
        inspect.details,
        Some(InspectDetailsView::Commercial { .. }) | Some(InspectDetailsView::Industrial { .. })
    );
    let heading = if is_workplace {
        "Workers at"
    } else {
        "Residents of"
    };
    // Remote commuters only staff a workplace; ignore any stray cache on other cells.
    let remote: &[CitizenDetailView] = if is_workplace { remote } else { &[] };
    let total = inspect.roster.len() + remote.len();
    let title = format!(
        "{heading} ({},{}) — {} citizen(s) · ↑/↓ · Esc close",
        inspect.x, inspect.y, total
    );

    frame.render_widget(Clear, popup);
    let block = Block::default().title(title).borders(Borders::ALL);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Workplaces reserve the bottom line for the local/remote count footnote.
    let (body, footer) = if is_workplace {
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);
        (parts[0], Some(parts[1]))
    } else {
        (inner, None)
    };

    if total == 0 {
        frame.render_widget(Paragraph::new("No citizens yet."), body);
    } else {
        // Last column is context-sensitive: on a workplace it lists where each worker lives;
        // on a residential it lists where each resident works.
        let relation_header = if is_workplace { "Lives at" } else { "Works at" };
        let mut header_cells = vec!["#", "Age", "Happy", "$"];
        if !is_workplace {
            header_cells.push("Arrival");
        }
        header_cells.push(relation_header);
        let header = Row::new(header_cells).style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
        // Local workers first, then remote commuters; the row index runs across both.
        let rows =
            inspect
                .roster
                .iter()
                .chain(remote.iter())
                .enumerate()
                .map(|(index, citizen)| {
                    let mut cells = vec![
                        format!("#{}", index + 1),
                        citizen.age.to_string(),
                        citizen.happiness.to_string(),
                        format!("${}", citizen.money),
                    ];
                    if !is_workplace {
                        cells.push(arrival_text(citizen).to_string());
                    }
                    cells.push(relation_text(citizen));
                    Row::new(cells)
                });
        let mut widths = vec![
            Constraint::Length(4),
            Constraint::Length(5),
            Constraint::Length(6),
            Constraint::Length(6),
        ];
        if !is_workplace {
            widths.push(Constraint::Length(12));
        }
        widths.push(Constraint::Min(12));
        let table = Table::new(rows, widths)
            .header(header)
            .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("> ");
        // Clamp in case the roster shrank while the panel was open: ratatui scrolls an
        // out-of-range selection into view but won't draw the highlight, so the cursor
        // would vanish. `total` is non-zero here, so the subtraction is safe.
        let selected = selected.min(total - 1);
        let mut state = TableState::default().with_selected(Some(selected));
        frame.render_stateful_widget(table, body, &mut state);
    }

    if let Some(footer) = footer {
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!("{} local · {} remote", inspect.roster.len(), remote.len()),
                Style::default().fg(Color::DarkGray),
            )),
            footer,
        );
    }
}

/// Static (snapshot, non-navigable) detail view of the travelers on the
/// inspected road cell: local citizens as roster rows, then visiting bodies'
/// endpoint summaries as plain text lines.
///
/// ```text
/// ┌ Travelers at (1,2) — 4 traveler(s) · Esc close ───────┐
/// │ #   Age  Happy  $    Works at                         │  (local rows, same
/// │ #1  27   72     $14  (0,1)                             │   shape as the
/// ├────────────────────────────────────────────────────────┤   citizen panel)
/// │ Visitors:                                                │
/// │  2× region 3 → here (1,0)                                │  (grouped, see
/// │  1× region 4 → region 5                                  │   RoadTravelerEndpointView)
/// └────────────────────────────────────────────────────────┘
/// ```
fn render_road_traveler_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    inspect: &InspectView,
    locals: &[CitizenDetailView],
    trucks: usize,
    visitors: &[RoadTravelerEndpointView],
) {
    let popup = centered_rect(60, 60, area);
    let visitor_total: usize = visitors.iter().map(|endpoint| endpoint.count).sum();
    let total = locals.len() + trucks + visitor_total;
    let title = format!(
        "Travelers at ({},{}) — {} traveler(s) · Esc close",
        inspect.x, inspect.y, total
    );

    frame.render_widget(Clear, popup);
    let block = Block::default().title(title).borders(Borders::ALL);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if total == 0 {
        frame.render_widget(Paragraph::new("No travelers here."), inner);
        return;
    }

    // Reserve summary lines only when trucks or visitors are present.
    let summary_lines = visitors.len() as u16 + u16::from(trucks > 0);
    let (local_area, visitor_area) = if summary_lines == 0 {
        (inner, None)
    } else {
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(summary_lines + 1)])
            .split(inner);
        (parts[0], Some(parts[1]))
    };

    if locals.is_empty() {
        frame.render_widget(Paragraph::new("No local residents here."), local_area);
    } else {
        let header = Row::new(["#", "Age", "Happy", "$", "Works at"]).style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
        let rows = locals.iter().enumerate().map(|(index, citizen)| {
            Row::new([
                format!("#{}", index + 1),
                citizen.age.to_string(),
                citizen.happiness.to_string(),
                format!("${}", citizen.money),
                relation_text(citizen),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(4),
                Constraint::Length(5),
                Constraint::Length(6),
                Constraint::Length(6),
                Constraint::Min(12),
            ],
        )
        .header(header);
        frame.render_widget(table, local_area);
    }

    if let Some(visitor_area) = visitor_area {
        let mut lines = vec![Line::from(Span::styled(
            "Other travelers:",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))];
        if trucks > 0 {
            lines.push(Line::from(format!("  {} local truck(s)", trucks)));
        }
        lines.extend(
            visitors
                .iter()
                .map(|endpoint| Line::from(format!("  {}", visitor_endpoint_text(endpoint)))),
        );
        frame.render_widget(Paragraph::new(lines), visitor_area);
    }
}

/// Formats one grouped visitor endpoint row, e.g. `2× region 3 → here (1,0)`,
/// `1× region 4 → region 5`, or `3× region 3 → no job` for a jobless transit
/// visitor. `local_workplace` takes priority over the bare `work_region` since
/// it is the more precise fact.
fn visitor_endpoint_text(endpoint: &RoadTravelerEndpointView) -> String {
    let destination = match (endpoint.local_workplace, endpoint.work_region) {
        (Some(cell), _) => format!("here ({},{})", cell.x, cell.y),
        (None, Some(region)) => format!("region {}", region.0),
        (None, None) => "no job".to_string(),
    };
    format!(
        "{}× region {} → {destination}",
        endpoint.count, endpoint.home_region.0
    )
}

/// The cell a roster row points at, for the Enter-to-jump action: a resident's
/// workplace, or a worker's home. The inner `Option<RegionId>` is the region to
/// switch to — `None` means "stay in the current region" (a local home), `Some(r)`
/// a remote region. `Unemployed` rows have nowhere to jump.
///
/// ```text
///   WorksAt { cell, .. }     -> Some((Some(cell.region), cell.x, cell.y))   resident -> workplace
///   LivesAt { region, x, y } -> Some((region, x, y))                        worker   -> home (region: Option)
///   Unemployed               -> None
/// ```
fn relation_target(relation: CitizenRelation) -> Option<(Option<RegionId>, usize, usize)> {
    match relation {
        CitizenRelation::WorksAt { cell, .. } => Some((Some(cell.region), cell.x, cell.y)),
        CitizenRelation::LivesAt { region, x, y } => Some((region, x, y)),
        CitizenRelation::Unemployed => None,
    }
}

/// Formats just the relation column of a roster row, e.g. `(2,0) · $3` or `(1,0)`. The leading
/// verb is omitted — the column header ("Works at" / "Lives at") already supplies it.
fn relation_text(citizen: &CitizenDetailView) -> String {
    match citizen.relation {
        CitizenRelation::WorksAt {
            cell,
            salary,
            is_remote,
        } => {
            let location = if is_remote {
                format!("region {} ({},{})", cell.region.0, cell.x, cell.y)
            } else {
                format!("({},{})", cell.x, cell.y)
            };
            format!("{location} · ${salary}")
        }
        CitizenRelation::Unemployed => "unemployed".to_string(),
        CitizenRelation::LivesAt { region, x, y } => match region {
            Some(region) => format!("region {} ({},{})", region.0, x, y),
            None => format!("({x},{y})"),
        },
    }
}

fn arrival_text(citizen: &CitizenDetailView) -> &'static str {
    match citizen.relation {
        CitizenRelation::WorksAt { .. } if citizen.unpaid_since_daily_settlement => "not arrived",
        CitizenRelation::WorksAt { .. } => "arrived",
        _ => "-",
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

    // "Build juice": a short-lived flash on the last edited cell. Only honoured while it is active.
    let flash = state
        .build_flash
        .as_ref()
        .filter(|f| f.is_active(Instant::now()));

    // Live animation only applies to the City theme's Normal overlay. Day/night tint follows the
    // clock; the frame-driven pulses run only while `animation_active` (animate && running).
    let city_normal =
        state.tile_theme == TileTheme::Unicode && state.current_overlay == MapOverlayInput::Normal;
    let hour = (view.status.turn % 24) as u8;
    let animating = state.animate && state.is_running;

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

    // P4: moving-citizen cells as an O(1) lookup, built only for the Normal overlay
    // (no other overlay draws dots). `overlay_traveler_dot` also gates on Normal +
    // cursor, so the set is the perf guard and the fn is the correctness guard.
    let traveler_cells: HashSet<(usize, usize)> =
        if state.current_overlay == MapOverlayInput::Normal {
            view.travelers.iter().map(|t| (t.x, t.y)).collect()
        } else {
            HashSet::new()
        };

    for y in state.viewport_y..end_y {
        let mut row = vec![Span::raw(format!("{y:>2} "))];
        for x in state.viewport_x..end_x {
            let index = y * view.map.width + x;
            let cell = &view.map.cells[index];
            let is_cursor = x == state.cursor_x && y == state.cursor_y;
            let preview_state = preview_state_for_cell(x, y, preview, state);
            let mut glyph = state.tile_theme.tile_for_cell(
                cell,
                state.current_overlay,
                is_cursor,
                preview_state,
            );
            // Day/night tint + cursor pulse (City theme, Normal overlay). Only colours change, so
            // the tile stays exactly two columns.
            if city_normal {
                animate_city_tile(
                    &mut glyph,
                    is_cursor,
                    preview_state,
                    hour,
                    state.anim_frame,
                    animating,
                );
            }
            // A freshly edited cell flashes in its delta colour (build green / refund yellow),
            // without changing the tile width.
            if flash.is_some_and(|f| f.x == x && f.y == y) {
                let color = flash.expect("flash present").color;
                glyph.style = Style::default()
                    .bg(color)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD);
            }
            // P4: a moving citizen draws a yellow bold dot on its cell (Normal
            // overlay only; the cursor wins).
            overlay_traveler_dot(
                &mut glyph,
                state.current_overlay,
                traveler_cells.contains(&(x, y)),
                is_cursor,
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

    let mut block = Block::default().title("City Map").borders(Borders::ALL);
    // The money delta rides the map title as a transient floating readout (e.g. "+$100").
    if let Some(flash) = flash {
        block = block.title(
            Line::from(Span::styled(
                flash.text.clone(),
                Style::default()
                    .fg(flash.color)
                    .add_modifier(Modifier::BOLD),
            ))
            .right_aligned(),
        );
    }

    frame.render_widget(
        // A Paragraph is enough here because the map is already formatted into line spans. Ratatui
        // handles clipping if the terminal is smaller than the full map.
        Paragraph::new(lines)
            .block(block)
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
    let (title, mut lines) = tui_inspect_card(inspect);
    if !inspect.explanations.is_empty() {
        lines.push(String::new());
        lines.push("Notes:".to_string());
        lines.extend(inspect.explanations.iter().cloned());
    }
    let lines = lines.into_iter().map(tui_inspect_line).collect::<Vec<_>>();

    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title(title).borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn tui_inspect_line(line: String) -> Line<'static> {
    let Some(rest) = line.strip_prefix("Source  ▕") else {
        return Line::from(line);
    };
    let Some(end) = rest.find('▏') else {
        return Line::from(line);
    };

    let (bar, tail_with_close) = rest.split_at(end);
    let tail = tail_with_close.strip_prefix('▏').unwrap_or(tail_with_close);
    let mut spans = vec![Span::raw("Source  ▕")];
    spans.extend(styled_source_bar_segments(bar));
    spans.push(Span::raw(format!("▏{tail}")));
    Line::from(spans)
}

fn styled_source_bar_segments(bar: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut chars = bar.chars();
    let Some(mut current) = chars.next() else {
        return spans;
    };
    let mut segment = String::from(current);

    for ch in chars {
        if ch == current {
            segment.push(ch);
        } else {
            spans.push(Span::styled(segment, source_bar_style(current)));
            current = ch;
            segment = String::from(ch);
        }
    }
    spans.push(Span::styled(segment, source_bar_style(current)));
    spans
}

fn source_bar_style(ch: char) -> Style {
    match ch {
        '█' => Style::default().fg(Color::Green),
        '▓' => Style::default().fg(Color::Yellow),
        '░' => Style::default().fg(Color::DarkGray),
        _ => Style::default(),
    }
}

fn tui_inspect_card(inspect: &InspectView) -> (String, Vec<String>) {
    let Some(details) = &inspect.details else {
        return (
            "Selected Cell".to_string(),
            vec![format!("({}, {}) outside map", inspect.x, inspect.y)],
        );
    };

    let title = match details {
        InspectDetailsView::Empty { .. } => tui_header_line(inspect, "EMPTY LAND", None),
        InspectDetailsView::Road => tui_header_line(inspect, "ROAD", None),
        InspectDetailsView::Residential { upgrade_level, .. } => tui_header_line(
            inspect,
            "RESIDENTIAL",
            Some((
                *upgrade_level,
                BuildingKind::Residential.max_upgrade_level(),
            )),
        ),
        InspectDetailsView::Commercial { upgrade_level, .. } => tui_header_line(
            inspect,
            "COMMERCIAL",
            Some((*upgrade_level, BuildingKind::Commercial.max_upgrade_level())),
        ),
        InspectDetailsView::Industrial { upgrade_level, .. } => tui_header_line(
            inspect,
            "INDUSTRIAL",
            Some((*upgrade_level, BuildingKind::Industrial.max_upgrade_level())),
        ),
        InspectDetailsView::PowerPlant { upgrade_level, .. } => tui_header_line(
            inspect,
            "POWER PLANT",
            Some((*upgrade_level, BuildingKind::PowerPlant.max_upgrade_level())),
        ),
        InspectDetailsView::Park { upgrade_level, .. } => tui_header_line(
            inspect,
            "PARK",
            Some((*upgrade_level, BuildingKind::Park.max_upgrade_level())),
        ),
        InspectDetailsView::Unknown => tui_header_line(inspect, "UNKNOWN", None),
    };

    let mut lines = match details {
        InspectDetailsView::Empty { buildable } => vec![
            tui_status_line(None, None, None, None),
            format!("Buildable {}", if *buildable { "✓" } else { "✗" }),
        ],
        InspectDetailsView::Road => vec![
            tui_status_line(None, None, None, None),
            format!("Travellers {}", inspect.road_traveler_count),
        ],
        InspectDetailsView::Residential {
            powered,
            power_demand,
            road_connected,
            upgrade_level: _,
            maintenance_cost,
            rent_per_citizen,
            population,
            max_population,
            citizens,
            average_happiness,
            average_happiness_target,
            average_money,
            unpaid_citizens,
            job_assignments,
        } => vec![
            tui_status_line(
                Some((*powered, *power_demand)),
                Some(*road_connected),
                Some(*maintenance_cost),
                None,
            ),
            format!(
                "People  {} {}/{}  citizens {}",
                unicode_bar(*population, *max_population, 10),
                population,
                max_population,
                citizens
            ),
            format!(
                "Happy   {} {}  target {}",
                option_value(*average_happiness),
                happiness_target_marker(*average_happiness, *average_happiness_target),
                option_value(*average_happiness_target)
            ),
            format!(
                "Money   § {} /cit  rent {}",
                option_value(*average_money),
                rent_per_citizen
            ),
            format!("Unpaid  {} not arrived", unpaid_citizens),
            format!("Work    {}", tui_job_summary(job_assignments)),
        ],
        InspectDetailsView::Commercial {
            powered,
            power_demand,
            road_connected,
            upgrade_level: _,
            maintenance_cost,
            sales_tax_per_shopper,
            goods_stored,
            goods_capacity,
            business_cash,
            upgrade_threshold,
            recent_profit,
            upgrade_ready,
            jobs,
            goods_sold_from_city,
            goods_sold_from_outside,
        } => vec![
            tui_status_line(
                Some((*powered, *power_demand)),
                Some(*road_connected),
                Some(*maintenance_cost),
                Some(*jobs),
            ),
            format!(
                "Goods   {} {}/{}",
                unicode_bar(*goods_stored, *goods_capacity, 12),
                goods_stored,
                goods_capacity
            ),
            format!(
                "Cash    {} {}/{}  {}",
                unicode_bar(*business_cash, upgrade_threshold.unwrap_or(0), 12),
                business_cash,
                upgrade_threshold
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "—".to_string()),
                if *upgrade_ready {
                    "▲ ready"
                } else {
                    "▲ later"
                }
            ),
            format!(
                "Sales   {}/shopper  recent {}",
                sales_tax_per_shopper, recent_profit
            ),
            if *goods_sold_from_city + *goods_sold_from_outside > 0 {
                format!(
                    "Source  {}  🏭 {} city-made · 🌍 {} from outside",
                    split_bar(*goods_sold_from_city, *goods_sold_from_outside, 10),
                    goods_sold_from_city,
                    goods_sold_from_outside
                )
            } else {
                format!("Source  {}  no sales today", split_bar(0, 0, 10))
            },
        ],
        InspectDetailsView::Industrial {
            powered,
            power_demand,
            road_connected,
            upgrade_level: _,
            maintenance_cost,
            goods_production,
            business_cash,
            upgrade_threshold,
            recent_profit,
            upgrade_ready,
            jobs,
        } => vec![
            tui_status_line(
                Some((*powered, *power_demand)),
                Some(*road_connected),
                Some(*maintenance_cost),
                Some(*jobs),
            ),
            format!("Output  {} city goods/turn", goods_production),
            format!(
                "Cash    {} {}/{}  {}",
                unicode_bar(*business_cash, upgrade_threshold.unwrap_or(0), 12),
                business_cash,
                upgrade_threshold
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "—".to_string()),
                if *upgrade_ready {
                    "▲ ready"
                } else {
                    "▲ later"
                }
            ),
            format!("Recent  {}", recent_profit),
        ],
        InspectDetailsView::PowerPlant {
            road_connected,
            connected_to_road_network,
            upgrade_level: _,
            maintenance_cost,
            power_capacity,
        } => vec![
            tui_status_line(None, Some(*road_connected), Some(*maintenance_cost), None),
            format!("Output  {} power capacity", power_capacity),
            format!(
                "Network {}",
                if *connected_to_road_network {
                    "✓ connected"
                } else {
                    "✗ none"
                }
            ),
        ],
        InspectDetailsView::Park {
            road_connected,
            upgrade_level: _,
            maintenance_cost,
            happiness_effect,
        } => vec![
            tui_status_line(None, Some(*road_connected), Some(*maintenance_cost), None),
            format!("Happy   +{} local happiness", happiness_effect),
        ],
        InspectDetailsView::Unknown => Vec::new(),
    };

    if !lines.is_empty() {
        lines.insert(1, tui_local_effects_line(inspect));
        lines.insert(2, "─".repeat(39));
    }

    (title, with_inspect_footer(inspect, lines))
}

fn with_inspect_footer(inspect: &InspectView, mut lines: Vec<String>) -> Vec<String> {
    if !inspect.flags.is_empty() {
        lines.push(format!(
            "⚠ {}",
            inspect
                .flags
                .iter()
                .map(tui_flag_chip)
                .collect::<Vec<_>>()
                .join("   ")
        ));
    }
    lines
}

fn tui_header_line(inspect: &InspectView, label: &str, level: Option<(u8, u8)>) -> String {
    let level = level
        .map(|(level, max)| format!("Lvl {} {level}/{max}", level_gauge(level, max)))
        .unwrap_or_else(|| "Lvl —".to_string());
    format!("({},{}) {:<12} {}", inspect.x, inspect.y, label, level)
}

fn tui_status_line(
    power: Option<(bool, i32)>,
    road_connected: Option<bool>,
    maintenance_cost: Option<i32>,
    jobs: Option<i32>,
) -> String {
    let power = power
        .map(|(powered, demand)| format!("⚡ {} d{}", if powered { "on " } else { "off" }, demand))
        .unwrap_or_else(|| "⚡ —".to_string());
    let road = road_connected
        .map(|connected| format!("🛣 {}", if connected { "✓" } else { "✗" }))
        .unwrap_or_else(|| "🛣 —".to_string());
    let maintenance = maintenance_cost
        .map(|cost| format!("🔧 {cost}"))
        .unwrap_or_else(|| "🔧 —".to_string());
    let jobs = jobs
        .map(|jobs| format!("👷 {jobs} jobs"))
        .unwrap_or_default();
    format!("{:<11} {:<7} {:<5} {}", power, road, maintenance, jobs)
}

fn tui_local_effects_line(inspect: &InspectView) -> String {
    let Some(effects) = inspect.local_effects else {
        return "Land —  Poll —  Access —  Desir —".to_string();
    };
    format!(
        "Land {}  Poll {}  Access {}  Desir {}",
        block_meter(effects.land_value),
        block_meter(effects.pollution_pressure),
        block_meter(effects.accessibility),
        block_meter(effects.desirability)
    )
}

fn unicode_bar(value: i32, max: i32, width: usize) -> String {
    if max <= 0 {
        return format!("▕{}▏", "░".repeat(width));
    }
    let value = value.clamp(0, max) as usize;
    let max = max as usize;
    let filled = (value * width + max / 2) / max;
    format!("▕{}{}▏", "█".repeat(filled), "░".repeat(width - filled))
}

fn split_bar(city: i32, outside: i32, width: usize) -> String {
    let total = city.max(0) + outside.max(0);
    if total == 0 {
        return format!("▕{}▏", "░".repeat(width));
    }
    let city_width = (city.max(0) as usize * width + total as usize / 2) / total as usize;
    let outside_width = width - city_width;
    format!("▕{}{}▏", "█".repeat(city_width), "▓".repeat(outside_width))
}

fn level_gauge(level: u8, max: u8) -> String {
    let max = max.max(1) as usize;
    let filled = (level as usize).min(max);
    format!("{}{}", "█".repeat(filled), "░".repeat(max - filled))
}

fn block_meter(value: i32) -> char {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    BARS[(value.clamp(0, 9) * 7 / 9) as usize]
}

fn option_value(value: Option<i32>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "—".to_string())
}

fn happiness_target_marker(value: Option<i32>, target: Option<i32>) -> &'static str {
    match (value, target) {
        (Some(value), Some(target)) if value < target => "↗",
        (Some(value), Some(target)) if value > target => "↘",
        (Some(_), Some(_)) => "→",
        _ => "—",
    }
}

fn tui_job_summary(assignments: &[crate::interface::view::JobAssignmentView]) -> String {
    if assignments.is_empty() {
        return "✗ none".to_string();
    }
    let local = assignments
        .iter()
        .filter(|assignment| !assignment.is_remote)
        .count();
    let remote = assignments.len() - local;
    if remote == 0 {
        return format!("✓ {local} local");
    }
    format!("✓ {local} local · {remote} ◀ neighbor")
}

fn tui_flag_chip(flag: &InspectFlag) -> &'static str {
    match flag {
        InspectFlag::GrowthBlockedNoJobs => "✗ no jobs",
        InspectFlag::GoodsSupplyNeighbor => "◀ neighbor goods",
        InspectFlag::GoodsSupplyMissing => "✗ no goods route",
    }
}

/// Picks an emoji icon or its ASCII fallback for the chrome panels.
fn hud_icon(use_emoji: bool, emoji: &'static str, ascii: &'static str) -> &'static str {
    if use_emoji { emoji } else { ascii }
}

/// A coloured trend arrow for a headline stat: ▲ rising (green), ▼ falling (red), → steady (dim).
fn trend_span(direction: Ordering) -> Span<'static> {
    let (arrow, color) = match direction {
        Ordering::Greater => ("▲", Color::Green),
        Ordering::Less => ("▼", Color::Red),
        Ordering::Equal => ("→", Color::DarkGray),
    };
    Span::styled(arrow.to_string(), Style::default().fg(color))
}

/// A compact 3-cell demand meter (Low/Medium/High) for the HUD.
fn demand_bar(level: DemandLevel) -> &'static str {
    match level {
        DemandLevel::Low => "▰▱▱",
        DemandLevel::Medium => "▰▰▱",
        DemandLevel::High => "▰▰▰",
    }
}

/// SimCity-style status HUD: an icon row per city metric (money, people, jobs, power, happiness,
/// pollution, goods) plus RCI demand meters. Reads only `view.status`, so it stays a pure view of
/// the simulation. Emoji degrade to ASCII on bare terminals via `state.use_emoji`.
fn render_city_hud(frame: &mut Frame<'_>, area: Rect, view: &GameView, state: &TuiState) {
    let s = &view.status;
    let e = state.use_emoji;

    let t = &state.hud_trend;
    let lines = vec![
        Line::from(Span::styled(
            state.region_label.clone(),
            Style::default().add_modifier(Modifier::DIM),
        )),
        Line::from(vec![
            Span::raw(format!("{} ${}", hud_icon(e, "💰", "$"), s.money)),
            trend_span(t.money),
            Span::raw(format!(
                "  {} {} pop",
                hud_icon(e, "👥", "pop"),
                s.population
            )),
            trend_span(t.population),
            Span::raw(format!(
                "  {} {} jobs ({} idle)",
                hud_icon(e, "💼", "job"),
                s.jobs,
                s.unemployment
            )),
        ]),
        Line::from(vec![
            Span::raw(format!(
                "{} {}/{}   {} {} happy",
                hud_icon(e, "⚡", "pwr"),
                s.power.total_supplied,
                s.power.total_capacity,
                hud_icon(e, "🙂", "joy"),
                s.happiness
            )),
            trend_span(t.happiness),
            Span::raw(format!(
                "   {} {} pollution",
                hud_icon(e, "🏭", "pol"),
                s.pollution
            )),
        ]),
        Line::from(format!(
            "{} +{} made · {} imported · {} exported",
            hud_icon(e, "📦", "goods"),
            s.goods.city_goods_produced,
            s.goods.goods_imported_from_outside,
            s.goods.goods_exported_outside,
        )),
        Line::from(format!(
            "{} R {} · C {} · I {}",
            hud_icon(e, "📈", "dmd"),
            demand_bar(s.demand.residential),
            demand_bar(s.demand.commercial),
            demand_bar(s.demand.industrial),
        )),
    ];

    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title("City HUD")
                    .title(status_time_title(view))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .wrap(Wrap { trim: true }),
        area,
    );
}

/// Top SimCity-style info bar: Funds on the left, the city name centred, the clock on the right,
/// all on a blue band.
fn render_header_bar(frame: &mut Frame<'_>, area: Rect, view: &GameView, state: &TuiState) {
    let s = &view.status;
    let bar = Style::default()
        .bg(Color::Blue)
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(area);

    frame.render_widget(
        Paragraph::new(Line::from(format!(
            " {} Funds: ${}",
            hud_icon(state.use_emoji, "💰", "$"),
            s.money
        )))
        .style(bar),
        cols[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from("S M A L L   C I T Y"))
            .style(bar)
            .alignment(Alignment::Center),
        cols[1],
    );
    frame.render_widget(
        Paragraph::new(Line::from(format!(
            "{} {} {} ",
            hud_icon(state.use_emoji, "🕑", "Time:"),
            time_spinner(s.turn),
            s.time.label
        )))
        .style(bar)
        .alignment(Alignment::Right),
        cols[2],
    );
}

/// Left build-tool strip: a static icon + hotkey legend that also doubles as the zone colour key.
/// Selection still happens via the number keys; the strip just highlights the active tool.
fn render_tool_strip(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let e = state.use_emoji;
    let build_tools = [
        (BuildingKind::Road, hud_icon(e, "🛣", "Rd"), '1'),
        (BuildingKind::Residential, hud_icon(e, "🏠", "Rs"), '2'),
        (BuildingKind::Commercial, hud_icon(e, "🏪", "Cm"), '3'),
        (BuildingKind::Industrial, hud_icon(e, "🏭", "In"), '4'),
        (BuildingKind::PowerPlant, hud_icon(e, "⚡", "Pw"), '5'),
        (BuildingKind::Park, hud_icon(e, "🌳", "Pk"), '6'),
    ];

    let mut lines: Vec<Line> = build_tools
        .iter()
        .map(|(kind, icon, key)| {
            let selected = state.selected_build == *kind;
            let mut style = building_style(*kind);
            if selected {
                style = style.add_modifier(Modifier::BOLD | Modifier::REVERSED);
            }
            let marker = if selected { "◀" } else { " " };
            Line::from(Span::styled(format!("{icon} {key}{marker}"), style))
        })
        .collect();
    // Bulldoze is an action, not a selectable tool, so it sits apart with its own key.
    lines.push(Line::from(Span::styled(
        format!("{} X", hud_icon(e, "🚜", "Bz")),
        Style::default().fg(Color::Red),
    )));

    // Paint mode is shown in the strip title and a brighter border so "draw" mode is obvious.
    let (title, border) = if state.paint_mode {
        ("Tools ✎", Color::Yellow)
    } else {
        ("Tools", Color::DarkGray)
    };
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border)),
        ),
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
            "Space pause/resume | +/- speed | WASD/Arrows move | 1-6 tools | P paint | ; anim | N next | O overlay | T theme | [ ] region | H help | Q quit",
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
        Line::from("  Space         Pause / resume auto-advance | +/- speed"),
        Line::from("  B Build · Enter zone citizens (else build) · P Paint (draw on move)"),
        Line::from("  R             Replace selected cell with selected tool"),
        Line::from("  U             Upgrade selected cell"),
        Line::from("  X             Bulldoze selected cell"),
        Line::from("  N             Advance 10 min   [ / ] Previous / next region"),
        Line::from(""),
        help_section("Files And UI"),
        Line::from("  S             Save city"),
        Line::from("  L             Load city"),
        Line::from("  H             Close Help"),
        Line::from("  Esc / Q       Quit (confirm + save option) · Ctrl-C immediate"),
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
                "{} | advancing every {}",
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
        RunSpeed::One => "500ms",
        RunSpeed::Two => "250ms",
        RunSpeed::Four => "125ms",
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

    // Each building is one two-column emoji icon that fills the whole tile (SimCity-style). Roads
    // keep box-drawing above; problem tiles fell back to ASCII markers just above.
    building_emoji(kind).to_string()
}

/// SimCity-style building icon for the City (Unicode) theme. Every emoji is two display columns, so
/// it fills one tile exactly and the two-column grid stays aligned (guarded by a test). ASCII themes
/// are the fallback for terminals without emoji — auto-selected by locale, and reachable via the
/// theme-cycle key.
fn building_emoji(kind: BuildingKind) -> &'static str {
    match kind {
        BuildingKind::Residential => "🏠",
        BuildingKind::Commercial => "🏪",
        BuildingKind::Industrial => "🏭",
        BuildingKind::PowerPlant => "⚡",
        BuildingKind::Park => "🌳",
        BuildingKind::Road => "==", // roads render via unicode_road_tile; never reached here
    }
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

    if cell_is_problem(cell) {
        return problem_style();
    }

    let mut style = building_style(kind);
    // Bigger footprint -> bold the ASCII glyph (the emoji theme uses background
    // brightness instead; see `city_cell_style`). fg is respected on letters.
    if cell.footprint_area > 1 {
        style = style.add_modifier(Modifier::BOLD);
    }
    if let Some(modifier) = zone_activity_modifier(cell, kind) {
        style = style.add_modifier(modifier);
    }

    style
}

/// Background brightness multiplier keyed to building size (footprint cell count).
/// 1x1 stays at the base zone tint; a 2-cell building is brighter and a 4-cell one
/// brightest, so a grown building glows in its own zone hue. Tiers match the upgrade
/// levels (L1 -> 1 cell, L2 -> 2, L3 -> 4).
fn size_brightness_factor(footprint_area: u8) -> f32 {
    match footprint_area {
        0 | 1 => 1.0,
        2 | 3 => 1.45,
        _ => 1.9,
    }
}

/// Scales an `Rgb` colour toward white by `factor` (channels clamped at 255). Other
/// colour kinds are returned unchanged (the City theme only uses `Rgb` backgrounds).
fn brighten(color: Color, factor: f32) -> Color {
    match color {
        Color::Rgb(r, g, b) => Color::Rgb(
            scale_channel(r, factor),
            scale_channel(g, factor),
            scale_channel(b, factor),
        ),
        other => other,
    }
}

fn scale_channel(channel: u8, factor: f32) -> u8 {
    (f32::from(channel) * factor).round().clamp(0.0, 255.0) as u8
}

/// A building cell in trouble: not connected to a road, or a powered consumer left unpowered.
fn cell_is_problem(cell: &CellView) -> bool {
    matches!(cell.road_connected, Some(false))
        || (matches!(cell.powered, Some(false)) && cell.power_demand.unwrap_or_default() > 0)
}

/// Muted-earth ground colour. Dark-but-not-black so the map is never a void, while bright zone
/// glyphs stay high-contrast on top (see docs/tui-city-redesign-plan.md §4a).
const GROUND_BG: Color = Color::Rgb(60, 48, 34);

/// Per-zone background tint, a notch brighter than the ground so districts read as raised lots.
fn zone_bg(kind: BuildingKind) -> Color {
    match kind {
        BuildingKind::Residential => Color::Rgb(30, 60, 30), // green district
        BuildingKind::Commercial => Color::Rgb(28, 40, 70),  // blue district
        BuildingKind::Industrial => Color::Rgb(70, 60, 25),  // olive/yellow district
        BuildingKind::Park => Color::Rgb(28, 72, 36),        // forest green
        BuildingKind::PowerPlant => Color::Rgb(25, 60, 65),  // dark cyan
        BuildingKind::Road => GROUND_BG,                     // roads sit on the ground
    }
}

/// City-theme cell style: the foreground/activity from [`cell_base_style`] plus a zoning background.
/// Empty land, roads and problem cells sit on the ground; healthy zones get their district tint.
/// Glyphs are bold for contrast against the coloured ground.
fn city_cell_style(cell: &CellView) -> Style {
    let bg = match cell.building {
        // Size-brightness: scale the zone tint by footprint size so a bigger building
        // glows brighter in its own hue (emoji ignore fg colour, so size rides on bg).
        Some(kind) if !cell_is_problem(cell) && kind != BuildingKind::Road => {
            brighten(zone_bg(kind), size_brightness_factor(cell.footprint_area))
        }
        _ => GROUND_BG,
    };
    cell_base_style(cell).bg(bg).add_modifier(Modifier::BOLD)
}

/// Applies the City-theme animations to one already-rendered tile **without changing its width**:
/// only colours change (the building emoji icon is a single two-column glyph, so there is no
/// char-2 to pulse — the old power-hum / industrial-smoke animation went away with the shade char).
///
/// - day/night: dims the muted-earth/zone background by the hour (clock-driven, always on)
/// - cursor pulse: blinks the cursor highlight (only while animating)
fn animate_city_tile(
    glyph: &mut TileGlyph,
    is_cursor: bool,
    preview: PreviewState,
    hour: u8,
    frame: u64,
    animating: bool,
) {
    // Day/night only tints plain cells (leave the bright cursor/preview highlight at full strength).
    if !is_cursor
        && preview == PreviewState::None
        && let Some(Color::Rgb(r, g, b)) = glyph.style.bg
    {
        let f = day_night_factor(hour);
        glyph.style = glyph.style.bg(Color::Rgb(
            dim_channel(r, f),
            dim_channel(g, f),
            dim_channel(b, f),
        ));
    }

    // Cursor pulse: alternate the bright highlight so the selected lot blinks.
    if animating && is_cursor && frame % 2 == 1 {
        glyph.style = glyph.style.bg(Color::Gray);
    }
}

/// Brightness percentage (0..=100) of the ground by hour of day: full at midday, dim at night.
fn day_night_factor(hour: u8) -> u16 {
    match hour {
        0..=5 => 60,
        6 => 75,
        7 => 88,
        8..=17 => 100,
        18 => 90,
        19 => 78,
        _ => 65,
    }
}

/// Scales one colour channel by a 0..=100 percentage using integer math (determinism-friendly).
fn dim_channel(channel: u8, percent: u16) -> u8 {
    (u16::from(channel) * percent / 100) as u8
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
    use crate::core::city_refs::CityCellRef;
    use crate::core::regional_game::RegionalGame;
    use crate::core::regions::RegionId;
    use crate::interface::view::{InspectFlag, LocalEffectsView, RoadLinks};
    use crossterm::event::KeyModifiers;
    use ratatui::backend::TestBackend;

    fn plain_glyph() -> TileGlyph {
        TileGlyph {
            tile: "R.".to_string(),
            style: Style::default().bg(Color::Blue),
        }
    }

    /// A moving citizen on a cell draws a yellow bold two-column dot in the Normal
    /// overlay, keeping the tile's existing background.
    #[test]
    fn traveler_dot_drawn_in_normal_overlay() {
        let mut glyph = plain_glyph();
        overlay_traveler_dot(&mut glyph, MapOverlayInput::Normal, true, false);
        assert_eq!(glyph.tile, "•·");
        assert_eq!(
            Span::raw(glyph.tile.as_str()).width(),
            2,
            "dot stays two display columns"
        );
        assert!(glyph.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(glyph.style.fg, Some(Color::Yellow));
        assert_eq!(
            glyph.style.bg,
            Some(Color::Blue),
            "keeps the cell background"
        );
    }

    /// The dot is suppressed outside the Normal overlay.
    #[test]
    fn traveler_dot_hidden_on_other_overlays() {
        let base = plain_glyph();
        for overlay in [
            MapOverlayInput::Power,
            MapOverlayInput::Pollution,
            MapOverlayInput::Population,
            MapOverlayInput::LandValue,
            MapOverlayInput::Desirability,
        ] {
            let mut glyph = base.clone();
            overlay_traveler_dot(&mut glyph, overlay, true, false);
            assert_eq!(glyph, base, "no dot on {overlay:?}");
        }
    }

    /// The cursor highlight wins over the dot; an empty cell is untouched.
    #[test]
    fn traveler_dot_loses_to_cursor_and_absent_when_no_traveler() {
        let base = plain_glyph();

        let mut on_cursor = base.clone();
        overlay_traveler_dot(&mut on_cursor, MapOverlayInput::Normal, true, true);
        assert_eq!(on_cursor, base, "cursor cell keeps its glyph");

        let mut no_traveler = base.clone();
        overlay_traveler_dot(&mut no_traveler, MapOverlayInput::Normal, false, false);
        assert_eq!(no_traveler, base, "no traveler → no dot");
    }

    #[test]
    fn building_emoji_icons_are_exactly_two_columns_wide() {
        // The two-column grid alignment depends on every building emoji occupying one full tile.
        for kind in [
            BuildingKind::Residential,
            BuildingKind::Commercial,
            BuildingKind::Industrial,
            BuildingKind::PowerPlant,
            BuildingKind::Park,
        ] {
            assert_eq!(
                Span::raw(building_emoji(kind)).width(),
                2,
                "{kind:?} icon must be two display columns"
            );
        }
    }

    #[test]
    fn level_gauge_uses_each_buildings_own_max() {
        // Power/Park cap at level 2, zoned buildings at 3. The gauge denominator and bar must follow
        // the building's own max, so a maxed power plant reads as full (not a phantom "2/3").
        assert_eq!(BuildingKind::PowerPlant.max_upgrade_level(), 2);
        assert_eq!(BuildingKind::Park.max_upgrade_level(), 2);
        assert_eq!(BuildingKind::Residential.max_upgrade_level(), 3);
        assert_eq!(level_gauge(2, 2), "██");
        assert_eq!(level_gauge(2, 3), "██░");
        assert_eq!(level_gauge(3, 3), "███");
    }

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
        assert_eq!(state.message, "Tile theme: City");

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
            "Simulation running at 1x: advancing every 500ms"
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
        assert_eq!(runtime.next_auto_tick, now + Duration::from_millis(250));
        assert_eq!(
            runtime.apply_action(TuiAction::IncreaseSpeed, now),
            TuiFlow::Continue
        );
        assert_eq!(runtime.state.run_speed, RunSpeed::Four);
        assert_eq!(runtime.next_auto_tick, now + Duration::from_millis(125));
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
    fn unicode_building_tiles_render_their_emoji_icon() {
        let cases = [
            (
                themed_cell(Some(BuildingKind::Residential), 'R', Some(1), None, None, 0),
                "🏠",
            ),
            (
                themed_cell(Some(BuildingKind::Commercial), 'C', Some(2), None, None, 0),
                "🏪",
            ),
            (
                themed_cell(Some(BuildingKind::Industrial), 'I', Some(3), None, None, 0),
                "🏭",
            ),
            (
                themed_cell(Some(BuildingKind::PowerPlant), 'P', Some(2), None, None, 0),
                "⚡",
            ),
            (
                themed_cell(Some(BuildingKind::Park), 'P', Some(2), None, None, 0),
                "🌳",
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
    fn unicode_building_tiles_are_two_display_columns_wide() {
        let cells = [
            residential_cell_with_population(0, 10),
            residential_cell_with_population(10, 10),
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

            // Emoji tiles are a single char but two display columns; what matters is the width.
            assert_eq!(Span::raw(&glyph.tile).width(), 2);
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

        // Cursor highlight is now a bright background (it wins over the preview tint).
        assert_eq!(valid.style.bg, Some(Color::White));
        assert!(valid.style.add_modifier.contains(Modifier::BOLD));
        // Invalid build preview tints the cell red.
        assert_eq!(invalid.style.bg, Some(Color::Red));
        assert!(invalid.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn city_theme_paints_zoning_backgrounds() {
        // Empty land sits on the muted-earth ground instead of the terminal default (the void).
        let empty = themed_cell(None, '.', None, None, None, 0);
        let empty_glyph = TileTheme::Unicode.tile_for_cell(
            &empty,
            MapOverlayInput::Normal,
            false,
            PreviewState::None,
        );
        assert_eq!(empty_glyph.style.bg, Some(GROUND_BG));

        // A healthy commercial lot gets its district tint, not the ground.
        let commercial = themed_cell(Some(BuildingKind::Commercial), 'C', Some(1), None, None, 0);
        let commercial_glyph = TileTheme::Unicode.tile_for_cell(
            &commercial,
            MapOverlayInput::Normal,
            false,
            PreviewState::None,
        );
        assert_eq!(
            commercial_glyph.style.bg,
            Some(zone_bg(BuildingKind::Commercial))
        );

        // The ASCII fallbacks stay background-free for bare terminals.
        let ascii = TileTheme::AsciiDetailed.tile_for_cell(
            &commercial,
            MapOverlayInput::Normal,
            false,
            PreviewState::None,
        );
        assert_eq!(ascii.style.bg, None);
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
        assert_eq!(runtime.state.message, "Pause before stepping manually");
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
    fn enter_opens_roster_on_a_zone_and_builds_on_empty_land() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);
        runtime.game = CityDriver::regional_with_size(4, 3).expect("regional UI driver");
        runtime.state.cursor_x = 0;
        runtime.state.cursor_y = 0;
        runtime.state.selected_build = BuildingKind::Residential;

        // Empty land: Enter builds and does not open the roster.
        runtime.apply_action(TuiAction::EnterCell, now);
        assert!(!runtime.state.citizen_panel);
        assert!(cell_has_roster(&runtime.game.inspect(0, 0)));

        // Now on the placed zone: Enter opens the roster instead of building.
        runtime.apply_action(TuiAction::EnterCell, now);
        assert!(runtime.state.citizen_panel);
        assert_eq!(runtime.state.citizen_selected, 0);
    }

    #[test]
    fn enter_does_not_open_roster_on_a_road() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);
        runtime.game = CityDriver::regional_with_size(4, 3).expect("regional UI driver");
        runtime.state.cursor_x = 0;
        runtime.state.cursor_y = 0;
        runtime.state.selected_build = BuildingKind::Road;
        runtime.apply_action(TuiAction::Build, now);

        // A road with no travelers opens no panel at all; Enter still builds (a
        // second Build on an already-built road is the existing behavior here).
        runtime.apply_action(TuiAction::EnterCell, now);
        assert!(!runtime.state.citizen_panel);
        assert!(!runtime.state.road_traveler_panel);
    }

    /// The `EnterCell` road-traveler branch is exercised at the routing level: given
    /// an `inspect.road_traveler_count > 0` (proven against a real `World` by the
    /// adapter's own `road_traveler_panel_seed`/`road_traveler_count` tests), the
    /// panel opens and its state is populated straight from the facade's seed —
    /// never by reading roster/token data directly.
    #[test]
    fn road_traveler_panel_key_closes_on_esc() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);
        runtime.game = CityDriver::regional_with_size(4, 3).expect("regional UI driver");
        runtime.state.road_traveler_panel = true;
        runtime.state.road_traveler_locals = vec![CitizenDetailView {
            age: 27,
            happiness: 72,
            money: 14,
            unpaid_since_daily_settlement: false,
            relation: CitizenRelation::Unemployed,
        }];

        // A non-close key is still consumed (modal keys never fall through), but
        // leaves the panel open since there is no in-list cursor to move.
        assert!(
            runtime
                .handle_road_traveler_panel_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        );
        assert!(runtime.state.road_traveler_panel);

        // Esc closes the panel; once closed the handler stops consuming keys.
        assert!(
            runtime.handle_road_traveler_panel_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        );
        assert!(!runtime.state.road_traveler_panel);
        assert!(
            !runtime
                .handle_road_traveler_panel_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        );
    }

    #[test]
    fn render_road_traveler_panel_shows_local_rows_and_grouped_visitors() {
        use crate::core::regions::RegionId;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let inspect = InspectView {
            x: 4,
            y: 2,
            in_bounds: true,
            cell: None,
            details: Some(InspectDetailsView::Road),
            local_effects: None,
            flags: Vec::new(),
            explanations: Vec::new(),
            roster: Vec::new(),
            road_traveler_count: 3,
        };
        let locals = vec![CitizenDetailView {
            age: 27,
            happiness: 72,
            money: 14,
            unpaid_since_daily_settlement: false,
            relation: CitizenRelation::Unemployed,
        }];
        let visitors = vec![RoadTravelerEndpointView {
            home_region: RegionId(3),
            work_region: None,
            local_workplace: None,
            count: 2,
        }];

        let mut terminal = Terminal::new(TestBackend::new(100, 20)).expect("test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_road_traveler_panel(frame, area, &inspect, &locals, 0, &visitors);
            })
            .expect("render road traveler panel");
        let text = buffer_text(terminal.backend().buffer());

        assert!(text.contains("Travelers at (4,2)"));
        assert!(text.contains("3 traveler(s)"));
        assert!(text.contains("27")); // local row's age column
        assert!(text.contains("Other travelers:"));
        assert!(text.contains("2× region 3 → no job"));
    }

    #[test]
    fn citizen_panel_key_moves_cursor_clamped_and_closes() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);
        runtime.game = CityDriver::regional_with_size(4, 3).expect("regional UI driver");
        runtime.state.citizen_panel = true;

        // Moving the cursor up is clamped at the top (and stays there with an empty roster).
        assert!(runtime.handle_citizen_panel_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)));
        assert_eq!(runtime.state.citizen_selected, 0);
        // Moving down is clamped against the (empty) live roster, so it cannot pass the last row.
        assert!(runtime.handle_citizen_panel_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
        assert_eq!(runtime.state.citizen_selected, 0);

        // Esc closes the panel; once closed the handler stops consuming keys.
        assert!(runtime.handle_citizen_panel_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(!runtime.state.citizen_panel);
        assert!(!runtime.handle_citizen_panel_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)));
    }

    #[test]
    fn relation_target_resolves_each_relation() {
        // Resident -> workplace (always carries a region).
        assert_eq!(
            relation_target(CitizenRelation::WorksAt {
                cell: CityCellRef::local(RegionId(2), 3, 4),
                salary: 5,
                is_remote: true,
            }),
            Some((Some(RegionId(2)), 3, 4))
        );
        // Local worker -> home in the current region (no switch).
        assert_eq!(
            relation_target(CitizenRelation::LivesAt {
                region: None,
                x: 1,
                y: 0
            }),
            Some((None, 1, 0))
        );
        // Remote commuter -> home in another region (switch to it).
        assert_eq!(
            relation_target(CitizenRelation::LivesAt {
                region: Some(RegionId(1)),
                x: 4,
                y: 11
            }),
            Some((Some(RegionId(1)), 4, 11))
        );
        // Unemployed -> nowhere.
        assert_eq!(relation_target(CitizenRelation::Unemployed), None);
    }

    #[test]
    fn enter_on_empty_roster_closes_without_moving() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);
        runtime.game = CityDriver::regional_with_size(4, 3).expect("regional UI driver");
        runtime.state.citizen_panel = true;
        runtime.state.cursor_x = 2;
        runtime.state.cursor_y = 1;

        // Empty roster: Enter closes the panel and reports no destination, cursor unchanged.
        assert!(
            runtime.handle_citizen_panel_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        );
        assert!(!runtime.state.citizen_panel);
        assert_eq!((runtime.state.cursor_x, runtime.state.cursor_y), (2, 1));
        assert!(runtime.state.message.contains("No location"));
    }

    #[test]
    fn enter_jumps_cursor_to_the_selected_residents_workplace() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);
        runtime.game = CityDriver::regional_with_size(4, 3).expect("regional UI driver");
        // Residential (1,0) and commercial (3,0) on one powered road network.
        for (x, y, kind) in [
            (0, 0, BuildingKind::PowerPlant),
            (0, 1, BuildingKind::Road),
            (1, 1, BuildingKind::Road),
            (2, 1, BuildingKind::Road),
            (3, 1, BuildingKind::Road),
            (1, 0, BuildingKind::Residential),
            (3, 0, BuildingKind::Commercial),
        ] {
            assert!(runtime.game.build(x, y, kind).success, "build {kind:?}");
        }
        // Let a resident move in and take the local commercial job.
        for _ in 0..48 {
            runtime.game.tick();
        }

        // Precondition: the first resident now works at the commercial cell (3,0).
        let roster = runtime.game.inspect(1, 0).roster;
        assert!(
            matches!(
                roster.first().map(|c| c.relation),
                Some(CitizenRelation::WorksAt { cell, .. }) if cell.x == 3 && cell.y == 0
            ),
            "expected an employed resident, got {:?}",
            roster.first().map(|c| c.relation)
        );

        // Open the roster on the residential cell and jump from the first row.
        runtime.state.cursor_x = 1;
        runtime.state.cursor_y = 0;
        runtime.state.citizen_panel = true;
        runtime.state.citizen_selected = 0;

        assert!(
            runtime.handle_citizen_panel_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        );
        assert!(!runtime.state.citizen_panel, "panel should close on jump");
        assert_eq!(
            (runtime.state.cursor_x, runtime.state.cursor_y),
            (3, 0),
            "cursor should land on the workplace"
        );
    }

    #[test]
    fn relation_text_formats_each_relation() {
        let local = CitizenDetailView {
            age: 27,
            happiness: 72,
            money: 14,
            unpaid_since_daily_settlement: false,
            relation: CitizenRelation::WorksAt {
                cell: CityCellRef::local(RegionId(1), 2, 0),
                salary: 3,
                is_remote: false,
            },
        };
        assert_eq!(relation_text(&local), "(2,0) · $3");

        let remote = CitizenDetailView {
            relation: CitizenRelation::WorksAt {
                cell: CityCellRef::local(RegionId(2), 1, 1),
                salary: 4,
                is_remote: true,
            },
            ..local
        };
        assert_eq!(relation_text(&remote), "region 2 (1,1) · $4");

        let lives = CitizenDetailView {
            relation: CitizenRelation::LivesAt {
                region: None,
                x: 1,
                y: 0,
            },
            ..local
        };
        assert_eq!(relation_text(&lives), "(1,0)");

        let remote_resident = CitizenDetailView {
            relation: CitizenRelation::LivesAt {
                region: Some(RegionId(1)),
                x: 4,
                y: 11,
            },
            ..local
        };
        assert_eq!(relation_text(&remote_resident), "region 1 (4,11)");

        let jobless = CitizenDetailView {
            relation: CitizenRelation::Unemployed,
            ..local
        };
        assert_eq!(relation_text(&jobless), "unemployed");
    }

    /// Renders a non-empty roster straight through `render_citizen_panel` (bypassing the
    /// simulation, which doesn't deterministically spawn citizens in a unit test) to confirm the
    /// `Table` draws its column header and the per-row values.
    #[test]
    fn citizen_panel_table_renders_header_and_rows() {
        let inspect = InspectView {
            x: 1,
            y: 0,
            in_bounds: true,
            cell: None,
            details: None,
            local_effects: None,
            flags: Vec::new(),
            explanations: Vec::new(),
            roster: vec![
                CitizenDetailView {
                    age: 27,
                    happiness: 72,
                    money: 14,
                    unpaid_since_daily_settlement: true,
                    relation: CitizenRelation::WorksAt {
                        cell: CityCellRef::local(RegionId(1), 2, 0),
                        salary: 3,
                        is_remote: false,
                    },
                },
                CitizenDetailView {
                    age: 34,
                    happiness: 41,
                    money: 3,
                    unpaid_since_daily_settlement: false,
                    relation: CitizenRelation::Unemployed,
                },
            ],
            road_traveler_count: 0,
        };

        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_citizen_panel(frame, area, &inspect, &[], 1);
            })
            .expect("render citizen panel");
        let text = buffer_text(terminal.backend().buffer());

        // Column header on top; the last column is context-sensitive (this is a
        // residential-style roster — details None is not a workplace — so "Works at").
        assert!(text.contains("Age"));
        assert!(text.contains("Happy"));
        assert!(text.contains("Arrival"));
        assert!(text.contains("Works at"));
        // Per-row, column-aligned values (no verb — the header supplies it).
        assert!(text.contains("(2,0) · $3"));
        assert!(text.contains("not arrived"));
        assert!(text.contains("unemployed"));
        // The selected row (index 1) carries the cursor symbol.
        assert!(text.contains("> #2"));

        // An out-of-range selection (roster shrank while open) clamps to the last row so the
        // cursor stays visible rather than vanishing.
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).expect("test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_citizen_panel(frame, area, &inspect, &[], 99);
            })
            .expect("render citizen panel");
        assert!(buffer_text(terminal.backend().buffer()).contains("> #2"));

        let arrived = InspectView {
            roster: vec![CitizenDetailView {
                unpaid_since_daily_settlement: false,
                relation: CitizenRelation::WorksAt {
                    cell: CityCellRef::local(RegionId(1), 2, 0),
                    salary: 3,
                    is_remote: false,
                },
                ..inspect.roster[0]
            }],
            ..inspect.clone()
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).expect("test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_citizen_panel(frame, area, &arrived, &[], 0);
            })
            .expect("render arrived citizen panel");
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("arrived"));
        assert!(!text.contains("not arrived"));

        // On a workplace (Industrial details) the last column flips to "Lives at".
        let workplace = InspectView {
            details: Some(InspectDetailsView::Industrial {
                powered: true,
                power_demand: 0,
                road_connected: true,
                upgrade_level: 0,
                maintenance_cost: 0,
                goods_production: 0,
                business_cash: 0,
                upgrade_threshold: None,
                recent_profit: 0,
                upgrade_ready: false,
                jobs: 0,
            }),
            ..inspect
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).expect("test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_citizen_panel(frame, area, &workplace, &[], 0);
            })
            .expect("render citizen panel");
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("Lives at"));
        assert!(!text.contains("Arrival"));
    }

    /// A workplace roster mixing a local worker and a remote commuter: both rows
    /// render, the remote one carries its home-region tag, and the footnote counts
    /// each group. Renders straight through `render_citizen_panel`.
    #[test]
    fn citizen_panel_lists_local_and_remote_workers() {
        let workplace = InspectView {
            x: 1,
            y: 2,
            in_bounds: true,
            cell: None,
            details: Some(InspectDetailsView::Commercial {
                powered: true,
                power_demand: 0,
                road_connected: true,
                upgrade_level: 0,
                maintenance_cost: 0,
                sales_tax_per_shopper: 0,
                goods_stored: 0,
                goods_capacity: 0,
                business_cash: 0,
                upgrade_threshold: None,
                recent_profit: 0,
                upgrade_ready: false,
                jobs: 0,
                goods_sold_from_city: 0,
                goods_sold_from_outside: 0,
            }),
            local_effects: None,
            flags: Vec::new(),
            explanations: Vec::new(),
            // One local worker living in this region at (0,1).
            roster: vec![CitizenDetailView {
                age: 40,
                happiness: 55,
                money: 8,
                unpaid_since_daily_settlement: false,
                relation: CitizenRelation::LivesAt {
                    region: None,
                    x: 0,
                    y: 1,
                },
            }],
            road_traveler_count: 0,
        };
        // One remote commuter living in region 1 at (4,11).
        let remote = [CitizenDetailView {
            age: 31,
            happiness: 70,
            money: 9,
            unpaid_since_daily_settlement: false,
            relation: CitizenRelation::LivesAt {
                region: Some(RegionId(1)),
                x: 4,
                y: 11,
            },
        }];

        let mut terminal = Terminal::new(TestBackend::new(100, 20)).expect("test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_citizen_panel(frame, area, &workplace, &remote, 0);
            })
            .expect("render citizen panel");
        let text = buffer_text(terminal.backend().buffer());

        // Both workers appear; the remote one is region-tagged, the local one is not.
        assert!(text.contains("#1"));
        assert!(text.contains("#2"));
        assert!(text.contains("(0,1)"));
        assert!(text.contains("region 1 (4,11)"));
        // Title counts both; footnote breaks down local vs remote.
        assert!(text.contains("2 citizen(s)"));
        assert!(text.contains("1 local · 1 remote"));
    }

    #[test]
    fn citizen_panel_renders_resident_and_workplace_headers() {
        let game = RegionalGame::single_region(4, 3).expect("regional test game");
        assert!(
            game.build(RegionId(1), 1, 0, BuildingKind::Residential)
                .unwrap()
                .success
        );
        assert!(
            game.build(RegionId(1), 2, 0, BuildingKind::Commercial)
                .unwrap()
                .success
        );

        let resident_screen = render_test_screen(
            &game,
            TuiState {
                cursor_x: 1,
                cursor_y: 0,
                citizen_panel: true,
                ..TuiState::default()
            },
        );
        assert!(resident_screen.contains("Residents of"));
        assert!(resident_screen.contains("No citizens yet"));

        let workplace_screen = render_test_screen(
            &game,
            TuiState {
                cursor_x: 2,
                cursor_y: 0,
                citizen_panel: true,
                ..TuiState::default()
            },
        );
        assert!(workplace_screen.contains("Workers at"));
        // Footnote now breaks down local vs remote staff (none here yet).
        assert!(workplace_screen.contains("0 local · 0 remote"));
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
        assert_eq!(runtime.next_auto_tick, now + AUTO_ADVANCE_INTERVAL);
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
        assert_eq!(runtime.next_auto_tick, now + Duration::from_millis(125));
    }

    /// P7d: each auto step is one movement sub-tick; the hourly economy (turn) only
    /// advances once per 6 sub-ticks.
    #[test]
    fn auto_advance_runs_economy_once_per_six_subticks() {
        let mut now = Instant::now();
        let mut runtime = TuiRuntime::new(now);
        runtime.game = CityDriver::regional_with_size(10, 10).expect("regional UI driver");
        runtime.state.is_running = true;
        runtime.next_auto_tick = now;

        for _ in 0..6 {
            runtime.apply_due_auto_tick(now);
            now = runtime.next_auto_tick; // jump to the next scheduled sub-tick
        }
        assert_eq!(
            runtime.game.view().status.turn,
            1,
            "the economy ticks once across 6 movement sub-ticks"
        );

        runtime.apply_due_auto_tick(now); // 7th sub-tick begins the next hour
        assert_eq!(runtime.game.view().status.turn, 2);
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
    fn request_quit_opens_confirm_dialog_without_exiting() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);

        let flow = runtime.apply_action(TuiAction::RequestQuit, now);

        assert_eq!(flow, TuiFlow::Continue);
        assert!(runtime.state.quit_confirm, "Esc/Q opens the confirm modal");
        assert!(!runtime.pending_quit, "it must not exit yet");
    }

    #[test]
    fn quit_confirm_cancel_returns_to_game() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);
        runtime.state.quit_confirm = true;

        let consumed =
            runtime.handle_quit_confirm_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(consumed, "the modal swallows the key");
        assert!(!runtime.state.quit_confirm);
        assert!(!runtime.pending_quit);
    }

    #[test]
    fn quit_confirm_quit_sets_pending_quit() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);
        runtime.state.quit_confirm = true;

        runtime.handle_quit_confirm_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));

        assert!(runtime.pending_quit, "Q in the modal confirms the quit");
    }

    #[test]
    fn quit_confirm_save_opens_save_prompt_flagged_to_quit() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);
        runtime.state.quit_confirm = true;

        runtime.handle_quit_confirm_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));

        assert!(
            !runtime.state.quit_confirm,
            "the modal hands off to the prompt"
        );
        let prompt = runtime.state.prompt.as_ref().expect("save prompt opened");
        assert_eq!(prompt.kind, PromptKind::Save);
        assert!(prompt.then_quit, "save is flagged to quit on success");
        assert!(
            !runtime.pending_quit,
            "not until the save actually succeeds"
        );
    }

    #[test]
    fn save_and_quit_prompt_quits_after_successful_save() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);
        let path = std::env::temp_dir().join("small_city_save_and_quit_test.json");
        runtime.state.prompt = Some(PromptState {
            kind: PromptKind::Save,
            input: path.to_string_lossy().into_owned(),
            then_quit: true,
        });

        runtime
            .handle_prompt_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .expect("save handled");

        assert!(
            runtime.pending_quit,
            "a save-and-quit exits once the write lands"
        );
        assert!(runtime.state.prompt.is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn paused_mode_uses_long_poll_timeout() {
        let now = Instant::now();

        assert_eq!(
            poll_timeout(false, now + AUTO_ADVANCE_INTERVAL, now),
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
        assert_eq!(RunSpeed::One.interval(), Duration::from_millis(500));
        assert_eq!(RunSpeed::Two.interval(), Duration::from_millis(250));
        assert_eq!(RunSpeed::Four.interval(), Duration::from_millis(125));
    }

    #[test]
    fn render_draws_expected_main_panels() {
        let game = RegionalGame::single_region(10, 10).expect("regional test game");
        let output = render_test_screen(&game, TuiState::default());

        for expected in [
            "City Map",
            "(0,0) EMPTY LAND",
            "Buildable ✓",
            "Land",
            "City HUD",
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
            .find(|line| line.contains("City HUD"))
            .expect("city hud panel header");

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
    fn header_bar_and_tool_strip_render() {
        let game = RegionalGame::single_region(10, 10).expect("regional test game");
        let output = render_test_screen(&game, TuiState::default());

        // SimCity-style header bar with Funds and the centred city name.
        assert!(output.contains("Funds: $"));
        assert!(output.contains("S M A L L"));
        // Left tool strip panel.
        assert!(output.contains("Tools"));
    }

    #[test]
    fn city_hud_uses_emoji_or_ascii_per_capability() {
        let game = RegionalGame::single_region(10, 10).expect("regional test game");

        let with_emoji = render_test_screen(
            &game,
            TuiState {
                use_emoji: true,
                ..TuiState::default()
            },
        );
        assert!(
            with_emoji.contains("👥"),
            "emoji HUD shows the people glyph"
        );

        let ascii = render_test_screen(
            &game,
            TuiState {
                use_emoji: false,
                ..TuiState::default()
            },
        );
        assert!(!ascii.contains("👥"), "ascii fallback omits emoji");
        assert!(ascii.contains("pop"), "ascii fallback labels people as pop");
    }

    #[test]
    fn hud_trend_tracks_changes_across_a_turn() {
        let mut state = TuiState::default();

        // First observation just seeds the baseline — no spurious arrows from zero.
        state.update_hud_trend(0, 100, 10, 50);
        assert_eq!(state.hud_trend.money, Ordering::Equal);

        // Same turn: trend stays put even if values are re-read.
        state.update_hud_trend(0, 999, 999, 999);
        assert_eq!(state.hud_trend.money, Ordering::Equal);

        // New turn with money up, population down, happiness flat.
        state.update_hud_trend(1, 140, 8, 50);
        assert_eq!(state.hud_trend.money, Ordering::Greater);
        assert_eq!(state.hud_trend.population, Ordering::Less);
        assert_eq!(state.hud_trend.happiness, Ordering::Equal);
    }

    #[test]
    fn trend_span_colors_by_direction() {
        assert_eq!(trend_span(Ordering::Greater).style.fg, Some(Color::Green));
        assert_eq!(trend_span(Ordering::Less).style.fg, Some(Color::Red));
        assert_eq!(trend_span(Ordering::Equal).style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn successful_build_raises_a_money_flash() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);

        // Default cursor (0,0) is empty land; the default tool is Residential.
        runtime.apply_action(TuiAction::Build, now);

        let flash = runtime.state.build_flash.expect("build raises a flash");
        assert_eq!((flash.x, flash.y), (0, 0));
        assert!(flash.text.contains('$'), "flash shows a money delta");
        assert!(flash.is_active(now));
    }

    #[test]
    fn failed_command_raises_no_flash() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);

        // Bulldozing empty land fails, so there is nothing to celebrate.
        runtime.apply_action(TuiAction::Bulldoze, now);

        assert!(runtime.state.build_flash.is_none());
    }

    #[test]
    fn paint_mode_draws_along_cursor_movement() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);

        // Off by default: moving does not build.
        runtime.apply_action(TuiAction::MoveRight, now);
        assert!(runtime.state.build_flash.is_none());

        // Turn paint mode on, then a move lays the selected tool on the entered cell.
        runtime.apply_action(TuiAction::TogglePaint, now);
        assert!(runtime.state.paint_mode);
        runtime.apply_action(TuiAction::MoveRight, now);
        let flash = runtime
            .state
            .build_flash
            .as_ref()
            .expect("painting builds on the entered cell");
        assert_eq!(
            (flash.x, flash.y),
            (runtime.state.cursor_x, runtime.state.cursor_y)
        );

        // Toggling off stops drawing.
        runtime.apply_action(TuiAction::TogglePaint, now);
        assert!(!runtime.state.paint_mode);
    }

    #[test]
    fn toggle_animation_flips_the_flag() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);
        assert!(runtime.state.animate, "animation defaults on");

        runtime.apply_action(TuiAction::ToggleAnimation, now);
        assert!(!runtime.state.animate);

        // Animation only advances while running, even when enabled.
        runtime.state.animate = true;
        runtime.state.is_running = false;
        assert!(!runtime.animation_active());
        runtime.state.is_running = true;
        assert!(runtime.animation_active());
    }

    #[test]
    fn animation_cadence_scales_with_run_speed() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);
        runtime.state.is_running = true;
        runtime.state.animate = true;

        // At 1x (300ms/frame) 100ms is not enough for a new frame.
        runtime.state.run_speed = RunSpeed::One;
        runtime.last_anim_at = now;
        runtime.state.anim_frame = 0;
        runtime.tick_animation(now + Duration::from_millis(100));
        assert_eq!(runtime.state.anim_frame, 0);

        // At 4x (75ms/frame) the same 100ms advances at least one frame.
        runtime.state.run_speed = RunSpeed::Four;
        runtime.last_anim_at = now;
        runtime.state.anim_frame = 0;
        runtime.tick_animation(now + Duration::from_millis(100));
        assert!(runtime.state.anim_frame >= 1);
        assert!(runtime.dirty);
    }

    #[test]
    fn day_night_factor_dims_at_night() {
        assert_eq!(day_night_factor(12), 100, "full brightness at midday");
        assert!(
            day_night_factor(2) < day_night_factor(12),
            "dimmer at night"
        );
        assert!(day_night_factor(23) < day_night_factor(12));
    }

    #[test]
    fn animation_keeps_tiles_two_columns_wide() {
        // Every animated City tile must stay exactly two display columns (the alignment invariant).
        let cells = [
            themed_cell(Some(BuildingKind::PowerPlant), 'P', Some(2), None, None, 0),
            themed_cell(Some(BuildingKind::Industrial), 'I', Some(2), None, None, 0),
            themed_cell(Some(BuildingKind::Residential), 'R', Some(1), None, None, 0),
            themed_cell(None, '.', None, None, None, 0),
        ];

        for frame in 0..6 {
            for cell in &cells {
                let mut glyph = TileTheme::Unicode.tile_for_cell(
                    cell,
                    MapOverlayInput::Normal,
                    false,
                    PreviewState::None,
                );
                animate_city_tile(&mut glyph, false, PreviewState::None, 2, frame, true);
                assert_eq!(
                    Span::raw(&glyph.tile).width(),
                    2,
                    "animated tile must stay two columns (frame {frame})"
                );
            }
        }
    }

    #[test]
    fn expired_build_flash_is_cleared_and_repaints() {
        let now = Instant::now();
        let mut runtime = TuiRuntime::new(now);
        runtime.state.build_flash = Some(BuildFlash {
            x: 1,
            y: 1,
            text: "+$10".to_string(),
            color: Color::Green,
            expires_at: now,
        });
        runtime.mark_clean();

        runtime.expire_build_flash(now + Duration::from_millis(1));

        assert!(runtime.state.build_flash.is_none());
        assert!(runtime.dirty, "clearing the flash asks for a repaint");
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
    fn tui_inspect_card_uses_unicode_mockup_glyphs() {
        let inspect = InspectView {
            x: 12,
            y: 4,
            in_bounds: true,
            cell: None,
            details: Some(InspectDetailsView::Commercial {
                powered: true,
                power_demand: 2,
                road_connected: true,
                upgrade_level: 2,
                maintenance_cost: 3,
                sales_tax_per_shopper: 1,
                goods_stored: 4,
                goods_capacity: 12,
                business_cash: 30,
                upgrade_threshold: Some(50),
                recent_profit: 7,
                upgrade_ready: false,
                jobs: 2,
                goods_sold_from_city: 6,
                goods_sold_from_outside: 2,
            }),
            local_effects: Some(LocalEffectsView {
                land_value: 6,
                pollution_pressure: 1,
                accessibility: 5,
                desirability: 4,
            }),
            flags: vec![InspectFlag::GoodsSupplyNeighbor],
            explanations: Vec::new(),
            roster: Vec::new(),
            road_traveler_count: 0,
        };

        let (title, body) = tui_inspect_card(&inspect);
        let lines = body.join("\n");

        assert!(title.contains("(12,4) COMMERCIAL"));
        assert!(title.contains("Lvl ██░ 2/3"));
        assert!(body[1].starts_with("Land ▅"));
        assert_eq!(body[2], "───────────────────────────────────────");
        assert!(lines.contains("⚡ on  d2"));
        assert!(lines.contains("🛣 ✓"));
        assert!(lines.contains("👷 2 jobs"));
        assert!(lines.contains("Goods   ▕████░░░░░░░░▏ 4/12"));
        assert!(lines.contains("Source  ▕████████▓▓▏  🏭 6 city-made · 🌍 2 from outside"));
        assert!(lines.contains("⚠ ◀ neighbor goods"));
    }

    #[test]
    fn tui_source_line_colors_city_and_outside_bar_segments() {
        let inspect = InspectView {
            x: 12,
            y: 4,
            in_bounds: true,
            cell: None,
            details: Some(InspectDetailsView::Commercial {
                powered: true,
                power_demand: 2,
                road_connected: true,
                upgrade_level: 2,
                maintenance_cost: 3,
                sales_tax_per_shopper: 1,
                goods_stored: 4,
                goods_capacity: 12,
                business_cash: 30,
                upgrade_threshold: Some(50),
                recent_profit: 7,
                upgrade_ready: false,
                jobs: 2,
                goods_sold_from_city: 6,
                goods_sold_from_outside: 2,
            }),
            local_effects: None,
            flags: Vec::new(),
            explanations: Vec::new(),
            roster: Vec::new(),
            road_traveler_count: 0,
        };
        let (_, body) = tui_inspect_card(&inspect);
        let source = body
            .into_iter()
            .find(|line| line.starts_with("Source  "))
            .expect("formatted source row");
        let line = tui_inspect_line(source);

        assert_eq!(line.spans[1].content.as_ref(), "████████");
        assert_eq!(line.spans[1].style.fg, Some(Color::Green));
        assert_eq!(line.spans[2].content.as_ref(), "▓▓");
        assert_eq!(line.spans[2].style.fg, Some(Color::Yellow));
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
        assert!(output.contains("Space         Pause / resume auto-advance | +/- speed"));
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
        assert!(output.contains("1x | advancing every 500ms"));
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

    #[test]
    fn multi_cell_building_shows_icon_on_anchor_and_fill_elsewhere() {
        let mut anchor = themed_cell(Some(BuildingKind::Residential), 'R', Some(2), None, None, 0);
        anchor.footprint_area = 2; // 2x1 building; themed_cell already set anchor = true
        let mut fill = anchor.clone();
        fill.footprint_anchor = false;

        // Unicode: the icon shows only on the anchor; the continuation cell is a dim fill.
        assert_eq!(
            TileTheme::Unicode.normal_tile(&anchor).tile,
            building_emoji(BuildingKind::Residential)
        );
        assert_eq!(TileTheme::Unicode.normal_tile(&fill).tile, "░░");

        // ASCII detailed: letter+level on the anchor, "::" fill on the continuation cell.
        assert_ne!(TileTheme::AsciiDetailed.normal_tile(&anchor).tile, "::");
        assert_eq!(TileTheme::AsciiDetailed.normal_tile(&fill).tile, "::");

        // A non-anchor cell of a building in trouble keeps its problem marker (the fill
        // must not hide a building-wide power/road warning).
        let mut problem_fill = themed_cell(
            Some(BuildingKind::Residential),
            'R',
            Some(2),
            None,
            Some(false),
            0,
        );
        problem_fill.footprint_area = 2;
        problem_fill.footprint_anchor = false;
        assert_eq!(
            TileTheme::AsciiDetailed.normal_tile(&problem_fill).tile,
            "R!"
        );
    }

    #[test]
    fn size_brightness_factor_rises_with_footprint() {
        assert_eq!(size_brightness_factor(1), 1.0);
        assert!(size_brightness_factor(2) > 1.0);
        assert!(size_brightness_factor(4) > size_brightness_factor(2));
    }

    #[test]
    fn brighten_scales_rgb_and_clamps() {
        assert_eq!(
            brighten(Color::Rgb(100, 100, 100), 1.5),
            Color::Rgb(150, 150, 150)
        );
        assert_eq!(
            brighten(Color::Rgb(200, 200, 200), 2.0),
            Color::Rgb(255, 255, 255) // clamped
        );
        assert_eq!(brighten(Color::Green, 2.0), Color::Green); // non-rgb unchanged
    }

    #[test]
    fn larger_building_gets_a_brighter_zone_background() {
        let small = themed_cell(
            Some(BuildingKind::Residential),
            'R',
            Some(1),
            Some(true),
            Some(true),
            0,
        );
        let mut large = small.clone();
        large.footprint_area = 4;

        let (Some(Color::Rgb(_, small_g, _)), Some(Color::Rgb(_, large_g, _))) =
            (city_cell_style(&small).bg, city_cell_style(&large).bg)
        else {
            panic!("expected rgb zone backgrounds");
        };
        assert!(
            large_g > small_g,
            "a 4-cell building should glow brighter than a 1-cell one ({large_g} vs {small_g})"
        );
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
            footprint_anchor: building.is_some(),
            footprint_area: u8::from(building.is_some()),
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
