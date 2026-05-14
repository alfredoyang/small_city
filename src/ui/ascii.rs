use std::io::{self, Write};

use crate::core::game::Game;
use crate::interface::events::CommandResult;
use crate::interface::input::{BuildingKind, MapOverlayInput};
use crate::interface::view::{GameView, InspectDetailsView, InspectView};

// Terminal-only command shape. The UI converts text into Game API inputs, then drops it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AsciiCommand {
    Build {
        kind: BuildingKind,
        x: usize,
        y: usize,
    },
    Next,
    Inspect {
        x: usize,
        y: usize,
    },
    Status,
    View {
        overlay: MapOverlayInput,
    },
    Save {
        filename: String,
    },
    Load {
        filename: String,
    },
    Quit,
    Help,
}

/// Runs the ASCII terminal UI using only the public Game API and interface view models.
pub fn run() -> io::Result<()> {
    let mut game = Game::default();
    let mut overlay = MapOverlayInput::Normal;
    println!("Small City");
    print_help();

    loop {
        render(&game.view_with_overlay(overlay));
        print!("> ");
        io::stdout().flush()?;

        let mut input = String::new();
        if io::stdin().read_line(&mut input)? == 0 {
            return Ok(());
        }

        match parse_command(&input) {
            Ok(AsciiCommand::Build { kind, x, y }) => print_result(&game.build(x, y, kind)),
            Ok(AsciiCommand::Next) => print_result(&game.tick()),
            Ok(AsciiCommand::Inspect { x, y }) => render_inspect(&game.inspect(x, y)),
            Ok(AsciiCommand::Status) => render_status(&game.view()),
            Ok(AsciiCommand::View {
                overlay: next_overlay,
            }) => {
                overlay = next_overlay;
            }
            Ok(AsciiCommand::Save { filename }) => match game.save_to_file(&filename) {
                Ok(()) => println!("Saved {filename}"),
                Err(error) => println!("{error}"),
            },
            Ok(AsciiCommand::Load { filename }) => match Game::load_from_file(&filename) {
                Ok(loaded_game) => {
                    game = loaded_game;
                    println!("Loaded {filename}");
                }
                Err(error) => println!("{error}"),
            },
            Ok(AsciiCommand::Quit) => return Ok(()),
            Ok(AsciiCommand::Help) => print_help(),
            Err(message) => println!("{message}"),
        }
    }
}

/// Renders the city from GameView only, preserving the UI boundary around ECS internals.
pub fn render(view: &GameView) {
    render_status(view);
    for y in 0..view.map.height {
        for x in 0..view.map.width {
            let index = y * view.map.width + x;
            print!("{}", view.map.cells[index].symbol);
        }
        println!();
    }
}

fn render_status(view: &GameView) {
    let status = &view.status;
    println!(
        "Turn {} | Money {} | Pop {} | Jobs {} | Unemployed {} | Pollution {} | Happiness {}",
        status.turn,
        status.money,
        status.population,
        status.jobs,
        status.unemployment,
        status.pollution,
        status.happiness
    );
}

fn render_inspect(inspect: &InspectView) {
    println!("{}", format_inspect(inspect));
}

/// Formats inspect output from InspectView only, preserving the UI boundary around ECS internals.
pub fn format_inspect(inspect: &InspectView) -> String {
    let Some(details) = &inspect.details else {
        return format!("({}, {}) is outside the map", inspect.x, inspect.y);
    };

    match details {
        InspectDetailsView::Empty { buildable } => {
            format!(
                "({}, {}) Empty | buildable {}",
                inspect.x, inspect.y, buildable
            )
        }
        InspectDetailsView::Road => format!("({}, {}) Road", inspect.x, inspect.y),
        InspectDetailsView::Residential {
            powered,
            population,
            max_population,
        } => format!(
            "({}, {}) Residential | powered {} | population {}/{}",
            inspect.x, inspect.y, powered, population, max_population
        ),
        InspectDetailsView::Commercial { powered, jobs } => format!(
            "({}, {}) Commercial | powered {} | jobs {}",
            inspect.x, inspect.y, powered, jobs
        ),
        InspectDetailsView::Industrial { powered, jobs } => format!(
            "({}, {}) Industrial | powered {} | jobs {}",
            inspect.x, inspect.y, powered, jobs
        ),
        InspectDetailsView::PowerPlant { power_radius } => format!(
            "({}, {}) Power Plant | power radius {}",
            inspect.x, inspect.y, power_radius
        ),
        InspectDetailsView::Park { happiness_effect } => format!(
            "({}, {}) Park | happiness effect +{}",
            inspect.x, inspect.y, happiness_effect
        ),
        InspectDetailsView::Unknown => format!("({}, {}) Unknown", inspect.x, inspect.y),
    }
}

fn print_result(result: &CommandResult) {
    for event in &result.events {
        println!("{}", event.message());
    }
}

fn print_help() {
    println!("Commands:");
    println!("  build road x y");
    println!("  build residential x y");
    println!("  build commercial x y");
    println!("  build industrial x y");
    println!("  build power x y");
    println!("  build park x y");
    println!("  next");
    println!("  inspect x y");
    println!("  status");
    println!("  view normal");
    println!("  view power");
    println!("  view pollution");
    println!("  view population");
    println!("  save filename");
    println!("  load filename");
    println!("  quit");
}

fn parse_command(input: &str) -> Result<AsciiCommand, String> {
    let parts: Vec<_> = input.split_whitespace().collect();
    match parts.as_slice() {
        ["build", kind, x, y] => Ok(AsciiCommand::Build {
            kind: parse_building_kind(kind)?,
            x: parse_coordinate(x)?,
            y: parse_coordinate(y)?,
        }),
        ["next"] => Ok(AsciiCommand::Next),
        ["inspect", x, y] => Ok(AsciiCommand::Inspect {
            x: parse_coordinate(x)?,
            y: parse_coordinate(y)?,
        }),
        ["status"] => Ok(AsciiCommand::Status),
        ["view", overlay] => Ok(AsciiCommand::View {
            overlay: parse_overlay(overlay)?,
        }),
        ["save", filename] => Ok(AsciiCommand::Save {
            filename: filename.to_string(),
        }),
        ["load", filename] => Ok(AsciiCommand::Load {
            filename: filename.to_string(),
        }),
        ["quit"] => Ok(AsciiCommand::Quit),
        ["help"] => Ok(AsciiCommand::Help),
        [] => Ok(AsciiCommand::Help),
        _ => Err("Unknown command".to_string()),
    }
}

fn parse_coordinate(value: &str) -> Result<usize, String> {
    value
        .parse()
        .map_err(|_| format!("Invalid coordinate: {value}"))
}

fn parse_overlay(value: &str) -> Result<MapOverlayInput, String> {
    match value {
        "normal" => Ok(MapOverlayInput::Normal),
        "power" => Ok(MapOverlayInput::Power),
        "pollution" => Ok(MapOverlayInput::Pollution),
        "population" => Ok(MapOverlayInput::Population),
        _ => Err(format!("Unknown view overlay: {value}")),
    }
}

fn parse_building_kind(value: &str) -> Result<BuildingKind, String> {
    match value {
        "road" => Ok(BuildingKind::Road),
        "residential" => Ok(BuildingKind::Residential),
        "commercial" => Ok(BuildingKind::Commercial),
        "industrial" => Ok(BuildingKind::Industrial),
        "power" => Ok(BuildingKind::PowerPlant),
        "park" => Ok(BuildingKind::Park),
        _ => Err(format!("Unknown building kind: {value}")),
    }
}
