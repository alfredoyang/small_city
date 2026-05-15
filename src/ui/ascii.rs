use std::io::{self, IsTerminal, Read, Write};
use std::process::Command;

use crate::core::game::Game;
use crate::interface::input::{BuildingKind, MapOverlayInput};
use crate::interface::view::{DemandLevel, GameView, InspectDetailsView, InspectView};

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

    fn cycle_overlay(&mut self) {
        self.current_overlay = match self.current_overlay {
            MapOverlayInput::Normal => MapOverlayInput::Power,
            MapOverlayInput::Power => MapOverlayInput::Pollution,
            MapOverlayInput::Pollution => MapOverlayInput::Population,
            MapOverlayInput::Population => MapOverlayInput::Normal,
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
        render_screen(&view, &inspect, &state, &message)?;

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
                message = match game.save_to_file(DEFAULT_SAVE_FILE) {
                    Ok(()) => format!("Saved {DEFAULT_SAVE_FILE}"),
                    Err(error) => error.to_string(),
                };
            }
            UiAction::Load => {
                message = match Game::load_from_file(DEFAULT_SAVE_FILE) {
                    Ok(loaded_game) => {
                        game = loaded_game;
                        let loaded_view = game.view_with_overlay(state.current_overlay);
                        state.clamp_cursor(&loaded_view);
                        format!("Loaded {DEFAULT_SAVE_FILE}")
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
    };
    let _ = render_screen(view, &inspect, &state, "");
}

fn render_screen(
    view: &GameView,
    inspect: &InspectView,
    state: &AsciiUiState,
    message: &str,
) -> io::Result<()> {
    let mut stdout = io::stdout();
    write!(stdout, "\x1B[2J\x1B[H")?;
    writeln!(stdout, "Tiny City Builder")?;
    render_status(&mut stdout, view)?;
    writeln!(
        stdout,
        "Mode: Build {} | Cost: {} | Overlay: {}",
        state.selected_build.label(),
        selected_build_cost(view, state.selected_build),
        overlay_label(state.current_overlay)
    )?;
    writeln!(stdout)?;
    render_map(&mut stdout, view, state)?;
    writeln!(stdout)?;
    writeln!(stdout, "Selected: {}", format_inspect(inspect))?;
    if !message.is_empty() {
        writeln!(stdout, "Message: {message}")?;
    }
    writeln!(stdout)?;
    render_controls(&mut stdout)?;
    stdout.flush()
}

fn render_status(stdout: &mut impl Write, view: &GameView) -> io::Result<()> {
    let status = &view.status;
    writeln!(
        stdout,
        "Turn: {} | Money: ${} | Pop: {} | Jobs: {} | Happiness: {} | Pollution: {}",
        status.turn,
        status.money,
        status.population,
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
        "X = Bulldoze | I = Inspect | N = Next turn | V = Change overlay | S = Save | L = Load | Q = Quit"
    )
}

fn selected_build_cost(view: &GameView, selected_build: BuildingKind) -> i32 {
    view.build_options
        .iter()
        .find(|option| option.kind == selected_build)
        .map(|option| option.cost)
        .unwrap_or_else(|| selected_build.cost())
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
            road_connected,
            population,
            max_population,
        } => format!(
            "({}, {}) Residential | Powered: {} | Road: {} | Population: {}/{}",
            inspect.x,
            inspect.y,
            yes_no(*powered),
            yes_no(*road_connected),
            population,
            max_population
        ),
        InspectDetailsView::Commercial {
            powered,
            road_connected,
            jobs,
        } => format!(
            "({}, {}) Commercial | Powered: {} | Road: {} | Jobs: {}",
            inspect.x,
            inspect.y,
            yes_no(*powered),
            yes_no(*road_connected),
            jobs
        ),
        InspectDetailsView::Industrial {
            powered,
            road_connected,
            jobs,
        } => format!(
            "({}, {}) Industrial | Powered: {} | Road: {} | Jobs: {}",
            inspect.x,
            inspect.y,
            yes_no(*powered),
            yes_no(*road_connected),
            jobs
        ),
        InspectDetailsView::PowerPlant {
            road_connected,
            power_radius,
        } => format!(
            "({}, {}) Power Plant | Road: {} | Power Radius: {}",
            inspect.x,
            inspect.y,
            yes_no(*road_connected),
            power_radius
        ),
        InspectDetailsView::Park {
            road_connected,
            happiness_effect,
        } => format!(
            "({}, {}) Park | Road: {} | Happiness: +{}",
            inspect.x,
            inspect.y,
            yes_no(*road_connected),
            happiness_effect
        ),
        InspectDetailsView::Unknown => format!("({}, {}) Unknown", inspect.x, inspect.y),
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "Yes" } else { "No" }
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
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        if let Some(original_state) = &self.original_state {
            let _ = Command::new("stty").arg(original_state).status();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AsciiUiState, UiAction, parse_key_sequence};
    use crate::core::game::Game;
    use crate::interface::input::{BuildingKind, MapOverlayInput};

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
    fn overlay_cycles_in_display_order() {
        let mut state = AsciiUiState::default();

        state.cycle_overlay();
        assert_eq!(state.current_overlay, MapOverlayInput::Power);
        state.cycle_overlay();
        assert_eq!(state.current_overlay, MapOverlayInput::Pollution);
        state.cycle_overlay();
        assert_eq!(state.current_overlay, MapOverlayInput::Population);
        state.cycle_overlay();
        assert_eq!(state.current_overlay, MapOverlayInput::Normal);
    }
}
