//! Development-only measurement helpers for tick timing and actor-runtime overhead.
//!
//! These helpers do not change simulation rules. They provide deterministic count
//! estimates plus wall-clock measurements that can be run manually before moving
//! more systems onto the actor runtime.

use std::time::{Duration, Instant};

use crate::core::region_actor::{
    ActorRuntime, PhaseRun, RegionId, RegionMessageKind, SimPhase, SimTick,
};

/// Region size used by the current actor-backed local-effects system.
pub const LOCAL_EFFECTS_ACTOR_REGION_WIDTH: usize = 5;
pub const LOCAL_EFFECTS_ACTOR_REGION_HEIGHT: usize = 5;

/// Number of actor local-effects passes currently performed inside one game tick.
pub const LOCAL_EFFECTS_PASSES_PER_TICK: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalEffectsActorEstimate {
    pub actor_count: usize,
    pub message_count_per_tick: usize,
    pub promise_count_per_tick: usize,
    pub max_actor_queue_len_per_phase: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActorPhaseMeasurement {
    pub actor_count: usize,
    pub message_count: usize,
    pub promise_count: usize,
    pub max_actor_queue_len: usize,
    pub phase_duration: Duration,
    pub completed: bool,
}

/// Estimates local-effects actor traffic from public map dimensions.
pub fn estimate_local_effects_actor_load(width: usize, height: usize) -> LocalEffectsActorEstimate {
    LocalEffectsActorEstimate {
        actor_count: region_count(width, height),
        message_count_per_tick: width
            .saturating_mul(height)
            .saturating_mul(LOCAL_EFFECTS_PASSES_PER_TICK),
        promise_count_per_tick: 0,
        max_actor_queue_len_per_phase: max_region_cell_count(width, height),
    }
}

/// Measures a synthetic actor phase that uses the same runtime path as real actor systems.
pub fn measure_synthetic_actor_phase(
    actor_count: usize,
    messages_per_actor: usize,
) -> ActorPhaseMeasurement {
    let actor_ids: Vec<_> = (0..actor_count)
        .map(|id| RegionId(id.try_into().expect("actor count should fit u32")))
        .collect();
    let mut runtime = ActorRuntime::new_threaded(actor_ids.iter().copied());
    let tick = SimTick(1);
    let phase = SimPhase(1);

    for actor_id in &actor_ids {
        for _ in 0..messages_per_actor {
            runtime.send(
                tick,
                phase,
                *actor_id,
                *actor_id,
                RegionMessageKind::AddCounter(1),
            );
        }
    }

    let started = Instant::now();
    let results = runtime.run_phase(tick, phase);
    let phase_duration = started.elapsed();
    let completed = results
        .values()
        .all(|status| *status == PhaseRun::Completed);

    ActorPhaseMeasurement {
        actor_count,
        message_count: actor_count.saturating_mul(messages_per_actor),
        promise_count: 0,
        max_actor_queue_len: messages_per_actor,
        phase_duration,
        completed,
    }
}

fn region_count(width: usize, height: usize) -> usize {
    if width == 0 || height == 0 {
        return 0;
    }
    width.div_ceil(LOCAL_EFFECTS_ACTOR_REGION_WIDTH)
        * height.div_ceil(LOCAL_EFFECTS_ACTOR_REGION_HEIGHT)
}

fn max_region_cell_count(width: usize, height: usize) -> usize {
    if width == 0 || height == 0 {
        return 0;
    }
    let mut max_cells = 0;
    for region_y in (0..height).step_by(LOCAL_EFFECTS_ACTOR_REGION_HEIGHT) {
        for region_x in (0..width).step_by(LOCAL_EFFECTS_ACTOR_REGION_WIDTH) {
            let region_width = (width - region_x).min(LOCAL_EFFECTS_ACTOR_REGION_WIDTH);
            let region_height = (height - region_y).min(LOCAL_EFFECTS_ACTOR_REGION_HEIGHT);
            max_cells = max_cells.max(region_width * region_height);
        }
    }
    max_cells
}

#[cfg(test)]
mod tests {
    use super::{
        LOCAL_EFFECTS_ACTOR_REGION_HEIGHT, LOCAL_EFFECTS_ACTOR_REGION_WIDTH,
        estimate_local_effects_actor_load, measure_synthetic_actor_phase,
    };

    #[test]
    fn local_effects_actor_load_estimate_counts_two_cell_passes_per_tick() {
        let estimate = estimate_local_effects_actor_load(20, 15);

        assert_eq!(estimate.actor_count, 12);
        assert_eq!(estimate.message_count_per_tick, 20 * 15 * 2);
        assert_eq!(estimate.promise_count_per_tick, 0);
        assert_eq!(
            estimate.max_actor_queue_len_per_phase,
            LOCAL_EFFECTS_ACTOR_REGION_WIDTH * LOCAL_EFFECTS_ACTOR_REGION_HEIGHT
        );
    }

    #[test]
    fn local_effects_actor_load_estimate_handles_partial_edge_regions() {
        let estimate = estimate_local_effects_actor_load(6, 6);

        assert_eq!(estimate.actor_count, 4);
        assert_eq!(estimate.message_count_per_tick, 6 * 6 * 2);
        assert_eq!(
            estimate.max_actor_queue_len_per_phase,
            LOCAL_EFFECTS_ACTOR_REGION_WIDTH * LOCAL_EFFECTS_ACTOR_REGION_HEIGHT
        );
    }

    #[test]
    fn local_effects_actor_load_estimate_handles_empty_maps() {
        let estimate = estimate_local_effects_actor_load(0, 6);

        assert_eq!(estimate.actor_count, 0);
        assert_eq!(estimate.message_count_per_tick, 0);
        assert_eq!(estimate.promise_count_per_tick, 0);
        assert_eq!(estimate.max_actor_queue_len_per_phase, 0);
    }

    #[test]
    fn synthetic_actor_phase_reports_deterministic_message_counts() {
        let measurement = measure_synthetic_actor_phase(4, 3);

        assert!(measurement.completed);
        assert_eq!(measurement.actor_count, 4);
        assert_eq!(measurement.message_count, 12);
        assert_eq!(measurement.promise_count, 0);
        assert_eq!(measurement.max_actor_queue_len, 3);
    }
}
