use std::io::{self, Write};

use crate::core::game::Game;
use crate::interface::events::CommandResult;
use crate::interface::input::BuildingKind;
use crate::interface::view::{GameView, InspectView};

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
    Quit,
    Help,
}

/// Runs the ASCII terminal UI using only the public Game API and interface view models.
pub fn run() -> io::Result<()> {
    let mut game = Game::default();
    println!("Small City");
    print_help();

    loop {
        render(&game.view());
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
    let Some(cell) = &inspect.cell else {
        println!("({}, {}) is outside the map", inspect.x, inspect.y);
        return;
    };

    if cell.buildable {
        println!("({}, {}) Empty", cell.x, cell.y);
        return;
    }

    print!("({}, {}) {}", cell.x, cell.y, cell.label);
    if let (Some(population), Some(max_population)) = (cell.population, cell.max_population) {
        print!(" population {population}/{max_population}");
    }
    if let Some(powered) = cell.powered {
        print!(" powered {powered}");
    }
    println!();
}

fn print_result(result: &CommandResult) {
    println!("{}", result.message());
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
