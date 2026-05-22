//! Manual performance measurement entry point for comparing current tick and actor costs.
//!
//! Run with `cargo run --example performance_measurements`. The numbers are wall-clock
//! development measurements, not gameplay rules or test expectations.

use std::time::{Duration, Instant};

use small_city::core::game::Game;
use small_city::core::performance::{
    estimate_local_effects_actor_load, measure_synthetic_actor_phase,
};
use small_city::interface::input::BuildingKind;

const MEASUREMENT_TICKS: usize = 24 * 14;

#[derive(Debug, Clone, Copy)]
struct TickMeasurement {
    ticks: usize,
    elapsed: Duration,
}

impl TickMeasurement {
    fn average_tick(self) -> Duration {
        self.elapsed / self.ticks as u32
    }
}

fn main() {
    let small_city = starter_city(20, 15);
    let large_city = starter_city(60, 45);

    report_city("small city", small_city);
    report_city("large city", large_city);
}

fn report_city(label: &str, game: Game) {
    let view = game.view();
    let map_width = view.map.width;
    let map_height = view.map.height;
    let actor_estimate = estimate_local_effects_actor_load(map_width, map_height);
    let actor_phase = measure_synthetic_actor_phase(
        actor_estimate.actor_count,
        actor_estimate.max_actor_queue_len_per_phase,
    );
    let tick_measurement = measure_ticks(game, MEASUREMENT_TICKS);

    println!("{label}");
    println!("  map: {map_width}x{map_height}");
    println!(
        "  tick time: {:?} total, {:?} average over {} ticks",
        tick_measurement.elapsed,
        tick_measurement.average_tick(),
        tick_measurement.ticks
    );
    println!(
        "  local-effects actor estimate: {} actors, {} messages/tick, {} promises/tick, max queue {} messages/phase",
        actor_estimate.actor_count,
        actor_estimate.message_count_per_tick,
        actor_estimate.promise_count_per_tick,
        actor_estimate.max_actor_queue_len_per_phase
    );
    println!(
        "  synthetic actor phase: {:?}, {} messages, {} promises, completed={}",
        actor_phase.phase_duration,
        actor_phase.message_count,
        actor_phase.promise_count,
        actor_phase.completed
    );
}

fn measure_ticks(mut game: Game, ticks: usize) -> TickMeasurement {
    let started = Instant::now();
    for _ in 0..ticks {
        game.tick();
    }
    TickMeasurement {
        ticks,
        elapsed: started.elapsed(),
    }
}

fn starter_city(width: usize, height: usize) -> Game {
    let mut game = Game::new(width, height);
    let y = (height / 2).min(height.saturating_sub(1));
    let max_x = width.saturating_sub(1);

    build_if_in_bounds(&mut game, 0, y, BuildingKind::PowerPlant);
    for x in 0..=max_x.min(10) {
        build_if_in_bounds(&mut game, x, y, BuildingKind::Road);
    }
    build_if_in_bounds(&mut game, 1, y.saturating_sub(1), BuildingKind::Residential);
    build_if_in_bounds(&mut game, 2, y.saturating_sub(1), BuildingKind::Residential);
    build_if_in_bounds(&mut game, 3, y.saturating_sub(1), BuildingKind::Commercial);
    build_if_in_bounds(&mut game, 4, y.saturating_sub(1), BuildingKind::Industrial);
    build_if_in_bounds(&mut game, 5, y.saturating_sub(1), BuildingKind::Park);
    game
}

fn build_if_in_bounds(game: &mut Game, x: usize, y: usize, kind: BuildingKind) {
    let view = game.view();
    if x < view.map.width && y < view.map.height {
        game.build(x, y, kind);
    }
}
