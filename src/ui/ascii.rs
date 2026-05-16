//! Cursor-based ASCII terminal UI that renders only from GameView and InspectView data.

use std::io::{self, IsTerminal, Read, Write};
use std::process::Command;

use crate::core::game::Game;
use crate::interface::input::{BuildingKind, MapOverlayInput};
use crate::interface::view::{
    BuildPreviewView, DemandLevel, GameView, InspectDetailsView, InspectView,
};

const DEFAULT_SAVE_FILE: &str = "city1";

#[derive(Debug, Clone, PartialEq, Eq)]
struct AsciiUiState {
    cursor_x: usize,
    cursor_y: usize,
    selected_build: BuildingKind,
    current_overlay: MapOverlayInput,
}

impl Default for AsciiUiState {
    fn default() -> Self {
        Self {
            cursor_x: 0,
            cursor_y: 0,
            selected_build: BuildingKind::Residential,
            current_overlay: MapOverlayInput::Normal,
        }
    }
}

impl AsciiUiState {
    fn move_cursor(&mut self, dx: isize, dy: isize, view: &GameView) {
        let max_x = view.map.width.saturating_sub(1);
        let max_y = view.map.height.saturating_sub(1);
        self.cursor_x = self.cursor_x.saturating_add_signed(dx).min(max_x);
        self.cursor_y = self.cursor_y.saturating_add_signed(dy).min(max_y);
    }

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
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiAction {
    MoveUp,
    MoveDown,
    MoveLeft,
    MoveRight,
    SelectBuild(BuildingKind),
    Build,
    Replace,
    Upgrade,
    Bulldoze,
    Inspect,
    NextTurn,
    CycleOverlay,
    Save,
    Load,
    Quit,
    Noop,
}

/// Runs the ASCII terminal UI using only the public Game API and interface view models.
pub fn run() -> io::Result<()> {
    let _raw_terminal = RawTerminal::enter()?;
    let mut game = Game::default();
    let mut state = AsciiUiState::default();
    let mut message = String::from("Tiny City Builder");

    loop {
        let view = game.view_with_overlay(state.current_overlay);
        state.clamp_cursor(&view);
        let inspect = game.inspect(state.cursor_x, state.cursor_y);
        let preview = game.preview_build(state.cursor_x, state.cursor_y, state.selected_build);
        render_screen(&view, &inspect, &preview, &state, &message)?;

        match read_action()? {
            UiAction::MoveUp => state.move_cursor(0, -1, &view),
            UiAction::MoveDown => state.move_cursor(0, 1, &view),
            UiAction::MoveLeft => state.move_cursor(-1, 0, &view),
            UiAction::MoveRight => state.move_cursor(1, 0, &view),
            UiAction::SelectBuild(kind) => {
                state.selected_build = kind;
                message = format!("Selected {}", kind.label());
            }
            UiAction::Build => {
                message = game
                    .build(state.cursor_x, state.cursor_y, state.selected_build)
                    .message();
            }
            UiAction::Replace => {
                message = game
                    .replace(state.cursor_x, state.cursor_y, state.selected_build)
                    .message();
            }
            UiAction::Upgrade => {
                message = game.upgrade(state.cursor_x, state.cursor_y).message();
            }
            UiAction::Bulldoze => {
                message = game.bulldoze(state.cursor_x, state.cursor_y).message();
            }
            UiAction::Inspect => {
                message = format_inspect(&game.inspect(state.cursor_x, state.cursor_y));
            }
            UiAction::NextTurn => {
                message = game.tick().message();
            }
            UiAction::CycleOverlay => {
                state.cycle_overlay();
                message = format!("Overlay: {}", overlay_label(state.current_overlay));
            }
            UiAction::Save => {
                let filename = prompt_filename("Save filename", DEFAULT_SAVE_FILE)?;
                message = match game.save_to_file(&filename) {
                    Ok(()) => format!("Saved {filename}"),
                    Err(error) => error.to_string(),
                };
            }
            UiAction::Load => {
                let filename = prompt_filename("Load filename", DEFAULT_SAVE_FILE)?;
                message = match Game::load_from_file(&filename) {
                    Ok(loaded_game) => {
                        game = loaded_game;
                        let loaded_view = game.view_with_overlay(state.current_overlay);
                        state.reset_cursor();
                        state.clamp_cursor(&loaded_view);
                        format!("Loaded {filename}")
                    }
                    Err(error) => error.to_string(),
                };
            }
            UiAction::Quit => return Ok(()),
            UiAction::Noop => {}
        }
    }
}

/// Renders the city from GameView only, preserving the UI boundary around ECS internals.
pub fn render(view: &GameView) {
    let state = AsciiUiState::default();
    let inspect = InspectView {
        x: 0,
        y: 0,
        in_bounds: false,
        cell: None,
        details: None,
        local_effects: None,
        explanations: Vec::new(),
    };
    let preview = BuildPreviewView {
        kind: state.selected_build,
        label: state.selected_build.label().to_string(),
        cost: state.selected_build.cost(),
        can_build: false,
        reason: Some("No game preview available".to_string()),
        effects: Vec::new(),
    };
    let _ = render_screen(view, &inspect, &preview, &state, "");
}

fn render_screen(
    view: &GameView,
    inspect: &InspectView,
    preview: &BuildPreviewView,
    state: &AsciiUiState,
    message: &str,
) -> io::Result<()> {
    let mut stdout = io::stdout();
    write!(stdout, "\x1B[2J\x1B[H")?;
    writeln!(stdout, "Tiny City Builder")?;
    render_status(&mut stdout, view)?;
    writeln!(
        stdout,
        "Mode: Build {} | Cost: {} | Upkeep: {} | Overlay: {}",
        state.selected_build.label(),
        selected_build_cost(view, state.selected_build),
        selected_build_maintenance_cost(view, state.selected_build),
        overlay_label(state.current_overlay)
    )?;
    render_overlay_legend(&mut stdout, state.current_overlay)?;
    render_demand_notes(&mut stdout, view)?;
    writeln!(stdout)?;
    render_map(&mut stdout, view, state)?;
    writeln!(stdout)?;
    writeln!(stdout, "Selected: {}", format_inspect(inspect))?;
    render_local_effects(&mut stdout, inspect)?;
    render_inspect_explanations(&mut stdout, inspect)?;
    render_build_preview(&mut stdout, preview)?;
    if !message.is_empty() {
        writeln!(stdout, "Message: {message}")?;
    }
    writeln!(stdout)?;
    render_controls(&mut stdout)?;
    stdout.flush()
}

fn render_build_preview(stdout: &mut impl Write, preview: &BuildPreviewView) -> io::Result<()> {
    let result = if preview.can_build { "Yes" } else { "No" };
    writeln!(
        stdout,
        "Build Preview: {} | Cost: {} | Can build: {}",
        preview.label, preview.cost, result
    )?;
    if let Some(reason) = &preview.reason {
        writeln!(stdout, "Reason: {reason}")?;
    }
    if !preview.effects.is_empty() {
        writeln!(stdout, "Effects: {}", preview.effects.join("; "))?;
    }
    Ok(())
}

fn render_inspect_explanations(stdout: &mut impl Write, inspect: &InspectView) -> io::Result<()> {
    if inspect.explanations.is_empty() {
        return Ok(());
    }
    writeln!(stdout, "Inspect Notes: {}", inspect.explanations.join("; "))
}

fn render_local_effects(stdout: &mut impl Write, inspect: &InspectView) -> io::Result<()> {
    let Some(effects) = inspect.local_effects else {
        return Ok(());
    };

    writeln!(
        stdout,
        "Local: Land {} | Pollution Pressure {} | Access {} | Desirability {}",
        effects.land_value, effects.pollution_pressure, effects.accessibility, effects.desirability
    )
}

fn render_status(stdout: &mut impl Write, view: &GameView) -> io::Result<()> {
    let status = &view.status;
    writeln!(
        stdout,
        "Turn: {} | Money: ${} | Pop: {} | Citizens: {} | Jobs: {} | Happiness: {} | Pollution: {}",
        status.turn,
        status.money,
        status.population,
        status.citizens,
        status.jobs,
        status.happiness,
        status.pollution
    )?;
    writeln!(
        stdout,
        "Demand: R {} | C {} | I {}",
        demand_label(status.demand.residential),
        demand_label(status.demand.commercial),
        demand_label(status.demand.industrial)
    )?;
    writeln!(
        stdout,
        "Power: {}/{} supplied | Demand: {} | Shortage: {}",
        status.power.total_supplied,
        status.power.total_capacity,
        status.power.total_demand,
        status.power.total_shortage
    )
}

fn render_map(stdout: &mut impl Write, view: &GameView, state: &AsciiUiState) -> io::Result<()> {
    write!(stdout, "   ")?;
    for x in 0..view.map.width {
        write!(stdout, "{x:^3}")?;
    }
    writeln!(stdout)?;

    write!(stdout, "  +")?;
    for _ in 0..view.map.width {
        write!(stdout, "---")?;
    }
    writeln!(stdout, "+")?;

    for y in 0..view.map.height {
        write!(stdout, "{y:>2}|")?;
        for x in 0..view.map.width {
            let index = y * view.map.width + x;
            let symbol = view.map.cells[index].symbol;
            if x == state.cursor_x && y == state.cursor_y {
                write!(stdout, "[{symbol}]")?;
            } else {
                write!(stdout, " {symbol} ")?;
            }
        }
        writeln!(stdout, " |")?;
    }

    write!(stdout, "  +")?;
    for _ in 0..view.map.width {
        write!(stdout, "---")?;
    }
    writeln!(stdout, "+")
}

fn render_controls(stdout: &mut impl Write) -> io::Result<()> {
    writeln!(stdout, "Controls:")?;
    writeln!(stdout, "WASD / Arrow Keys = Move cursor")?;
    writeln!(
        stdout,
        "1 Road | 2 Residential | 3 Commercial | 4 Industrial | 5 Power | 6 Park"
    )?;
    writeln!(stdout, "B / Enter = Build selected type")?;
    writeln!(
        stdout,
        "R = Replace with selected type | U = Upgrade | X = Bulldoze"
    )?;
    writeln!(
        stdout,
        "I = Inspect | N = Next turn | V = Change overlay | S = Save | L = Load | Q = Quit"
    )?;
    writeln!(
        stdout,
        "Save/Load prompts for filename; Enter uses {DEFAULT_SAVE_FILE}"
    )
}

fn render_overlay_legend(stdout: &mut impl Write, overlay: MapOverlayInput) -> io::Result<()> {
    writeln!(stdout, "Overlay Legend: {}", overlay_legend(overlay))
}

fn render_demand_notes(stdout: &mut impl Write, view: &GameView) -> io::Result<()> {
    let demand = view.status.demand;
    writeln!(
        stdout,
        "Demand Notes: R {} | C {} | I {}",
        demand_note(BuildingKind::Residential, demand.residential),
        demand_note(BuildingKind::Commercial, demand.commercial),
        demand_note(BuildingKind::Industrial, demand.industrial)
    )
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

fn demand_label(level: DemandLevel) -> &'static str {
    match level {
        DemandLevel::Low => "Low",
        DemandLevel::Medium => "Medium",
        DemandLevel::High => "High",
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

fn read_action() -> io::Result<UiAction> {
    let mut stdin = io::stdin();
    let mut first = [0_u8; 1];
    stdin.read_exact(&mut first)?;

    if first[0] == b'\x1B' {
        let mut rest = [0_u8; 2];
        stdin.read_exact(&mut rest)?;
        return Ok(parse_key_sequence(&[first[0], rest[0], rest[1]]));
    }

    Ok(parse_key_sequence(&first))
}

fn parse_key_sequence(bytes: &[u8]) -> UiAction {
    match bytes {
        [b'w'] | [b'W'] | [b'\x1B', b'[', b'A'] => UiAction::MoveUp,
        [b's'] | [b'\x1B', b'[', b'B'] => UiAction::MoveDown,
        [b'a'] | [b'A'] | [b'\x1B', b'[', b'D'] => UiAction::MoveLeft,
        [b'd'] | [b'D'] | [b'\x1B', b'[', b'C'] => UiAction::MoveRight,
        [b'1'] => UiAction::SelectBuild(BuildingKind::Road),
        [b'2'] => UiAction::SelectBuild(BuildingKind::Residential),
        [b'3'] => UiAction::SelectBuild(BuildingKind::Commercial),
        [b'4'] => UiAction::SelectBuild(BuildingKind::Industrial),
        [b'5'] => UiAction::SelectBuild(BuildingKind::PowerPlant),
        [b'6'] => UiAction::SelectBuild(BuildingKind::Park),
        [b'b'] | [b'B'] | [b'\r'] | [b'\n'] => UiAction::Build,
        [b'r'] | [b'R'] => UiAction::Replace,
        [b'u'] | [b'U'] => UiAction::Upgrade,
        [b'x'] | [b'X'] => UiAction::Bulldoze,
        [b'i'] | [b'I'] => UiAction::Inspect,
        [b'n'] | [b'N'] => UiAction::NextTurn,
        [b'v'] | [b'V'] => UiAction::CycleOverlay,
        [b'S'] => UiAction::Save,
        [b'l'] | [b'L'] => UiAction::Load,
        [b'q'] | [b'Q'] => UiAction::Quit,
        _ => UiAction::Noop,
    }
}

/// Formats inspect output from InspectView only, preserving the UI boundary around ECS internals.
pub fn format_inspect(inspect: &InspectView) -> String {
    let Some(details) = &inspect.details else {
        return format!("({}, {}) is outside the map", inspect.x, inspect.y);
    };

    match details {
        InspectDetailsView::Empty { buildable } => {
            let buildable = if *buildable { "Yes" } else { "No" };
            format!(
                "({}, {}) Empty Land | Buildable: {}",
                inspect.x, inspect.y, buildable
            )
        }
        InspectDetailsView::Road => format!("({}, {}) Road", inspect.x, inspect.y),
        InspectDetailsView::Residential {
            powered,
            power_demand,
            road_connected,
            upgrade_level,
            population,
            max_population,
            citizens,
            average_happiness,
            average_money,
        } => format!(
            "({}, {}) Residential | Powered: {} | Demand: {} | Road: {} | Level: {} | Population: {}/{} | Citizens: {} | Avg Happiness: {} | Avg Money: {}",
            inspect.x,
            inspect.y,
            yes_no(*powered),
            power_demand,
            yes_no(*road_connected),
            upgrade_level,
            population,
            max_population,
            citizens,
            optional_number(*average_happiness),
            optional_number(*average_money)
        ),
        InspectDetailsView::Commercial {
            powered,
            power_demand,
            road_connected,
            jobs,
        } => format!(
            "({}, {}) Commercial | Powered: {} | Demand: {} | Road: {} | Jobs: {}",
            inspect.x,
            inspect.y,
            yes_no(*powered),
            power_demand,
            yes_no(*road_connected),
            jobs
        ),
        InspectDetailsView::Industrial {
            powered,
            power_demand,
            road_connected,
            jobs,
        } => format!(
            "({}, {}) Industrial | Powered: {} | Demand: {} | Road: {} | Jobs: {}",
            inspect.x,
            inspect.y,
            yes_no(*powered),
            power_demand,
            yes_no(*road_connected),
            jobs
        ),
        InspectDetailsView::PowerPlant {
            road_connected,
            connected_to_road_network,
            upgrade_level,
            power_capacity,
        } => format!(
            "({}, {}) Power Plant | Road: {} | Network: {} | Level: {} | Capacity: {}",
            inspect.x,
            inspect.y,
            yes_no(*road_connected),
            yes_no(*connected_to_road_network),
            upgrade_level,
            power_capacity
        ),
        InspectDetailsView::Park {
            road_connected,
            upgrade_level,
            happiness_effect,
        } => format!(
            "({}, {}) Park | Road: {} | Level: {} | Happiness: +{}",
            inspect.x,
            inspect.y,
            yes_no(*road_connected),
            upgrade_level,
            happiness_effect
        ),
        InspectDetailsView::Unknown => format!("({}, {}) Unknown", inspect.x, inspect.y),
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "Yes" } else { "No" }
}

fn optional_number(value: Option<i32>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "None".to_string())
}

fn prompt_filename(label: &str, default: &str) -> io::Result<String> {
    RawTerminal::temporarily_restore(|| {
        let mut stdout = io::stdout();
        write!(stdout, "\n{label} [{default}]: ")?;
        stdout.flush()?;

        let mut input = String::new();
        if io::stdin().read_line(&mut input)? == 0 {
            return Ok(default.to_string());
        }

        let filename = input.trim();
        if filename.is_empty() {
            Ok(default.to_string())
        } else {
            Ok(filename.to_string())
        }
    })
}

struct RawTerminal {
    original_state: Option<String>,
}

impl RawTerminal {
    fn enter() -> io::Result<Self> {
        if !io::stdin().is_terminal() {
            return Ok(Self {
                original_state: None,
            });
        }

        let output = Command::new("stty").arg("-g").output()?;
        let original_state = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Command::new("stty").args(["cbreak", "-echo"]).status()?;

        Ok(Self {
            original_state: Some(original_state),
        })
    }

    fn temporarily_restore<T>(operation: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        if !io::stdin().is_terminal() {
            return operation();
        }

        Command::new("stty").arg("sane").status()?;
        let _restore = KeyModeRestore;
        operation()
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        if let Some(original_state) = &self.original_state {
            let _ = Command::new("stty").arg(original_state).status();
        }
    }
}

struct KeyModeRestore;

impl Drop for KeyModeRestore {
    fn drop(&mut self) {
        let _ = Command::new("stty").args(["cbreak", "-echo"]).status();
    }
}

#[cfg(test)]
mod tests {
    use super::{AsciiUiState, UiAction, demand_note, overlay_legend, parse_key_sequence};
    use crate::core::game::Game;
    use crate::interface::input::{BuildingKind, MapOverlayInput};
    use crate::interface::view::DemandLevel;

    #[test]
    fn parses_single_key_build_selection_and_actions() {
        assert_eq!(
            parse_key_sequence(b"1"),
            UiAction::SelectBuild(BuildingKind::Road)
        );
        assert_eq!(
            parse_key_sequence(b"5"),
            UiAction::SelectBuild(BuildingKind::PowerPlant)
        );
        assert_eq!(parse_key_sequence(b"b"), UiAction::Build);
        assert_eq!(parse_key_sequence(b"\n"), UiAction::Build);
        assert_eq!(parse_key_sequence(b"r"), UiAction::Replace);
        assert_eq!(parse_key_sequence(b"u"), UiAction::Upgrade);
        assert_eq!(parse_key_sequence(b"x"), UiAction::Bulldoze);
        assert_eq!(parse_key_sequence(b"n"), UiAction::NextTurn);
        assert_eq!(parse_key_sequence(b"v"), UiAction::CycleOverlay);
        assert_eq!(parse_key_sequence(b"S"), UiAction::Save);
        assert_eq!(parse_key_sequence(b"q"), UiAction::Quit);
    }

    #[test]
    fn parses_wasd_and_arrow_movement() {
        assert_eq!(parse_key_sequence(b"w"), UiAction::MoveUp);
        assert_eq!(parse_key_sequence(b"a"), UiAction::MoveLeft);
        assert_eq!(parse_key_sequence(b"s"), UiAction::MoveDown);
        assert_eq!(parse_key_sequence(b"d"), UiAction::MoveRight);
        assert_eq!(parse_key_sequence(b"\x1B[A"), UiAction::MoveUp);
        assert_eq!(parse_key_sequence(b"\x1B[B"), UiAction::MoveDown);
        assert_eq!(parse_key_sequence(b"\x1B[C"), UiAction::MoveRight);
        assert_eq!(parse_key_sequence(b"\x1B[D"), UiAction::MoveLeft);
    }

    #[test]
    fn cursor_movement_is_clamped_to_map_bounds() {
        let game = Game::new(3, 2);
        let view = game.view();
        let mut state = AsciiUiState::default();

        state.move_cursor(-1, -1, &view);
        assert_eq!((state.cursor_x, state.cursor_y), (0, 0));

        state.move_cursor(10, 10, &view);
        assert_eq!((state.cursor_x, state.cursor_y), (2, 1));
    }

    #[test]
    fn cursor_can_reset_after_loading_game() {
        let game = Game::new(3, 2);
        let view = game.view();
        let mut state = AsciiUiState::default();
        state.move_cursor(10, 10, &view);

        state.reset_cursor();

        assert_eq!((state.cursor_x, state.cursor_y), (0, 0));
    }

    #[test]
    fn overlay_cycles_in_display_order() {
        let mut state = AsciiUiState::default();

        state.cycle_overlay();
        assert_eq!(state.current_overlay, MapOverlayInput::Power);
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
    fn overlay_legend_explains_active_overlay_symbols() {
        assert!(overlay_legend(MapOverlayInput::Power).contains("* powered road"));
        assert!(overlay_legend(MapOverlayInput::Power).contains("+ powered building"));
        assert!(overlay_legend(MapOverlayInput::Pollution).contains("0-9 pollution"));
        assert!(overlay_legend(MapOverlayInput::LandValue).contains("land value"));
        assert!(overlay_legend(MapOverlayInput::Desirability).contains("desirability"));
    }

    #[test]
    fn demand_notes_explain_zone_demand_levels() {
        assert!(demand_note(BuildingKind::Residential, DemandLevel::High).contains("jobs"));
        assert!(demand_note(BuildingKind::Commercial, DemandLevel::Low).contains("population"));
        assert!(demand_note(BuildingKind::Industrial, DemandLevel::High).contains("jobs"));
    }
}
