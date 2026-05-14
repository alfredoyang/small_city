use std::io::{self, Write};

use crate::core::game::Game;
use crate::interface::events::GameEventView;
use crate::interface::input::{UiCommand, parse_command};
use crate::interface::view::{CityStatusView, GameView, InspectView};

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
            Ok(UiCommand::Build { kind, x, y }) => print_event(game.build(x, y, kind).event),
            Ok(UiCommand::Next) => print_event(game.tick().event),
            Ok(UiCommand::Inspect { x, y }) => render_inspect(&game.inspect(x, y)),
            Ok(UiCommand::Status) => render_status(&game.view().status),
            Ok(UiCommand::Quit) => return Ok(()),
            Ok(UiCommand::Help) => print_help(),
            Err(message) => println!("{message}"),
        }
    }
}

pub fn render(view: &GameView) {
    render_status(&view.status);
    for y in 0..view.map.height {
        for x in 0..view.map.width {
            let index = y * view.map.width + x;
            print!("{}", view.map.cells[index].symbol);
        }
        println!();
    }
}

fn render_status(status: &CityStatusView) {
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

fn print_event(event: GameEventView) {
    match event {
        GameEventView::Built { x, y, kind } => {
            println!("Built {} at ({}, {})", kind.label(), x, y);
        }
        GameEventView::BuildFailed { reason } => println!("{reason}"),
        GameEventView::TurnAdvanced { turn } => println!("Advanced to turn {turn}"),
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
    println!("  quit");
}
