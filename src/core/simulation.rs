//! Shared deterministic simulation helpers for core facades and regional state.
//!
//! This module owns world-level simulation ordering that is shared by the
//! regional `RegionState`. It remains crate-local so UI code cannot receive or
//! manipulate ECS `World` storage directly.
//!
//! DT4 derived -> time dependency graph for one local tick:
//!
//! ```text
//! config/citizens
//!   -> derived: power -> roads -> stats -> pollution -> local effects
//!   -> derived: local job assignment -> morale target
//!   -> time:    clock, daily morale decay, population growth, actual morale
//!   -> time:    remote job grants, economy money/goods, weekly reinvestment
//!   -> derived summary refresh for the tick event and next paused read
//! ```
//!
//! Audit notes:
//! - `population::run` reads derived power/roads/jobs/local effects and writes
//!   citizen/population accumulators only. It creates a time -> derived edge
//!   because newly spawned citizens are counted by the following summary refresh.
//! - `business_growth::run` is a weekly time step: it reads derived demand/stats
//!   and durable business cash, then mutates building levels. The post-step
//!   summary refresh makes that time output visible without running another time
//!   system.
//! - `local_effects::run` intentionally reads `Citizen::morale.actual`, a time
//!   output. This creates a one-step feedback loop:
//!
//! ```text
//! morale.actual -> local_effects -> morale.target -> next morale.actual
//! ```
//!
//!   The tick runs local effects before target/actual happiness and again after,
//!   so target reads the previous applied actual morale; the next refresh observes
//!   the updated actual. Paused derived refreshes are idempotent because actual
//!   morale is frozen while paused.
//! - `Citizen::age` is durable state, but no aging system exists yet.

use crate::core::components::PowerSource;
use crate::core::entity::Entity;
use crate::core::regions::RegionId;
use crate::core::resources::{GameTime, is_new_day, is_new_week};
use crate::core::systems::{
    business_growth, citizens, economy, happiness, local_effects, pollution, population, power,
    road_network_analysis, stats,
};
use crate::core::world::World;
use crate::interface::events::{CommandResult, EconomyBreakdownView, GameEventView, MetricChange};
use crate::interface::view::GameTimeView;

#[cfg(test)]
pub(crate) fn tick_world(world: &mut World) -> CommandResult {
    let phase = begin_tick_power_phase(world, RegionId(1));
    finish_tick_after_power_phase(world, RegionId(1), phase)
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TickPowerPhase {
    before: TickSummarySnapshot,
    before_time: GameTime,
    after_time: GameTime,
}

#[derive(Debug, Clone, Copy)]
/// Paused tick state after local job assignment and before the daily economy.
///
/// Regional runtimes pause here to import (remote) workplace slots for citizens
/// left without a reachable local slot, then call `finish_tick_after_job_phase`
/// once job export grants apply.
pub(crate) struct TickJobPhase {
    before: TickSummarySnapshot,
    before_time: GameTime,
    after_time: GameTime,
    is_daily: bool,
}

impl TickJobPhase {
    /// Whether this tick crosses a daily boundary, when jobs and the economy
    /// resolve. Cross-region job export only engages on daily ticks.
    pub(crate) fn is_daily(&self) -> bool {
        self.is_daily
    }
}

/// Starts one tick and resolves local power before downstream systems read it.
///
/// Regional runtimes can pause after this phase to request producer-exported
/// power, then call `finish_tick_after_power_phase` once export grants apply.
pub(crate) fn begin_tick_power_phase(world: &mut World, local_region: RegionId) -> TickPowerPhase {
    // The world is simulated as `local_region`, so record it (and re-stamp citizen
    // homes, preserving the CW2 invariant `home.region == region_id`). In production
    // this already equals the id RegionState stamped, so the guard skips it every tick;
    // a bare World ticked directly (tests) is stamped once. Keeping region_id consistent
    // with the assignment region is what local/remote classification relies on.
    if world.region_id != local_region {
        world.set_region_id(local_region);
    }
    // DT1 derived-before-time: bring the derived pass current before the time
    // pass reads it. A paused config change (build/bulldoze) only marks the world
    // dirty; this is where that change is applied for the running step, matching
    // the timing of the old eager per-command refresh.
    ensure_derived_state(world, local_region);
    let before = TickSummarySnapshot::from_world(world);
    let before_time = world.resources.time;
    world.resources.time.advance_hours(1);
    let after_time = world.resources.time;
    power::run(world);

    TickPowerPhase {
        before,
        before_time,
        after_time,
    }
}

/// Chains the job phase for the synchronous (single-region) tick path.
///
/// Regional runtimes call `continue_to_job_phase` and `finish_tick_after_job_phase`
/// separately so they can pause between them for cross-region job exports.
#[cfg(test)]
pub(crate) fn finish_tick_after_power_phase(
    world: &mut World,
    local_region: RegionId,
    phase: TickPowerPhase,
) -> CommandResult {
    let job_phase = continue_to_job_phase(world, local_region, phase);
    finish_tick_after_job_phase(world, job_phase, &[])
}

/// Runs the post-power systems and, on a daily boundary, local job assignment.
///
/// Local assignment happens here (before the economy settles salaries/taxes) so a
/// citizen left without a reachable local slot becomes a candidate for an imported
/// remote workplace during the cross-region job export phase.
pub(crate) fn continue_to_job_phase(
    world: &mut World,
    local_region: RegionId,
    phase: TickPowerPhase,
) -> TickJobPhase {
    let is_daily = is_new_day(phase.before_time, phase.after_time);
    stats::run(world);
    local_effects::run(world);
    if is_daily {
        citizens::apply_daily_happiness_decay(world);
    }
    if is_daily {
        population::run(world);
    }
    citizens::update_happiness_targets(world);
    citizens::update_happiness(world);
    local_effects::run(world);
    if is_daily {
        economy::assign_local_jobs_for_daily_tick(world, local_region);
    }

    TickJobPhase {
        before: phase.before,
        before_time: phase.before_time,
        after_time: phase.after_time,
        is_daily,
    }
}

/// Settles the daily economy (using `exported_job_slots` for producer-owned tax),
/// then finishes the tick: weekly growth, stats refresh, pollution, happiness.
pub(crate) fn finish_tick_after_job_phase(
    world: &mut World,
    phase: TickJobPhase,
    exported_job_slots: &[Entity],
) -> CommandResult {
    finish_tick_after_goods_phase(world, phase, exported_job_slots, 0)
}

/// Finishes the tick after producer-owned job and goods exports resolve.
pub(crate) fn finish_tick_after_goods_phase(
    world: &mut World,
    phase: TickJobPhase,
    exported_job_slots: &[Entity],
    exported_goods_units: u32,
) -> CommandResult {
    let economy = if phase.is_daily {
        economy::run_with_goods_exports(world, exported_job_slots, exported_goods_units)
    } else {
        economy::EconomyBreakdown::default()
    };
    let business_upgrades = if is_new_week(phase.before_time, phase.after_time) {
        business_growth::run(world).upgrades
    } else {
        Vec::new()
    };
    if !business_upgrades.is_empty() {
        world.invalidate_jobs_registry();
    }
    stats::refresh_population_and_jobs(world);
    pollution::run(world);
    happiness::run(world);
    // P7c: movement no longer runs on the hourly tick. It is driven separately by
    // `RegionState::step_travel` (the 10-minute sub-tick), 6× per game hour via the
    // runner's `advance`, so crossings/turns cost realistic time. Travel state is
    // transient/display-only, so decoupling it from the economy tick is free.
    world.resources.turn += 1;
    let after = TickSummarySnapshot::from_world(world);

    let tick_summary = GameEventView::TickSummary {
        turn: world.resources.turn,
        time: game_time_view(world.resources.time),
        population: metric_change(phase.before.population, after.population),
        money: metric_change(phase.before.money, after.money),
        happiness: metric_change(phase.before.happiness, after.happiness),
        pollution: metric_change(phase.before.pollution, after.pollution),
        unemployment: metric_change(phase.before.unemployment, after.unemployment),
        powered_buildings: metric_change(phase.before.powered_buildings, after.powered_buildings),
        economy: EconomyBreakdownView {
            salaries_paid: economy.salaries_paid,
            workplace_tax: economy.workplace_tax,
            rent_income: economy.rent_income,
            commercial_sales_tax: economy.commercial_sales_tax,
            shoppers_served: economy.shoppers_served,
            local_goods_produced: economy.local_goods_produced,
            local_goods_stored: economy.local_goods_stored,
            local_goods_sold: economy.local_goods_sold,
            imported_goods_sold: economy.imported_goods_sold,
            exported_goods: economy.exported_goods,
            manufacturing_tax: economy.manufacturing_tax,
            export_tax: economy.export_tax,
            rent_failures: economy.rent_failures,
            maintenance_cost: economy.maintenance_cost,
            net: economy.net,
        },
    };
    let mut events = vec![tick_summary];
    events.extend(business_upgrades.into_iter().map(|upgrade| {
        GameEventView::BusinessAutoUpgraded {
            x: upgrade.x,
            y: upgrade.y,
            kind: upgrade.kind,
            level: upgrade.level,
        }
    }));

    CommandResult::success_events(events)
}

/// Recomputes the derived pass only when a config change has marked it dirty (DT1).
///
/// This is the lazy entry point: config commands (build/bulldoze/replace/upgrade)
/// now only mark the world dirty instead of eagerly recomputing, so the derived
/// pass runs once at the next `&mut` boundary that reads it -- a tick (via
/// `begin_tick_power_phase`) or a view/inspect read -- and not once per command.
///
/// ```text
///   build (paused) --> world.derived_dirty = true        (no recompute yet)
///        |
///        v   next tick OR view/inspect read
///   ensure_derived_state: if dirty { run_derived_pass; clear }
/// ```
pub(crate) fn ensure_derived_state(world: &mut World, local_region: RegionId) {
    if world.is_derived_dirty() {
        refresh_derived_state_for_world(world, local_region);
        world.clear_derived_dirty();
    }
}

/// The derived pass: the instantaneous state that is a pure function of the
/// current config (buildings/roads/citizens), recomputed on change, not on time.
///
/// DT1 covers the already-derived systems: power, road analysis, stats, pollution,
/// and local effects. DT2 keeps actual citizen happiness in the time pass while
/// exposing the conditions-only `happiness_target` from this derived pass.
pub(crate) fn refresh_derived_state_for_world(world: &mut World, local_region: RegionId) {
    // Record the simulated region, re-stamping homes only if it actually changed
    // (no-op in production; keeps a bare World consistent without per-call cost).
    if world.region_id != local_region {
        world.set_region_id(local_region);
    }
    // Cross-region imported power is reserved in the runtime ledger and only
    // (re)applied during a tick's export phase. `power::run` clears every consumer
    // and re-applies only *local* grants, so running this derived pass for a paused
    // config change (build/bulldoze) would drop still-valid imports and make
    // imported-powered buildings flash unpowered until the next tick. Capture the
    // imports first, then restore the ones local power did not cover. The next real
    // tick re-derives imports authoritatively through the cross-region flow.
    let imported = imported_power_grants(world);
    power::run(world);
    reapply_imported_power(world, &imported);
    road_network_analysis::run(world);
    stats::refresh_population_and_jobs(world);
    pollution::run(world);
    local_effects::run(world);
    economy::assign_local_jobs(world, local_region);
    citizens::update_happiness_targets(world);
    happiness::run(world);
}

/// Imported-power grants currently held by consumers, captured before a local
/// power recompute so a paused derived pass can restore them (see
/// `refresh_derived_state_for_world`). Also used by
/// `RegionState::begin_tick_power_demand_phase` (`regions/mod.rs`) to protect
/// reads that happen later in the same tick, after the raw recompute in
/// `begin_tick_power_phase` — see
/// `docs/20260703-bug-cross-region-export-starvation-fix.md`.
pub(crate) fn imported_power_grants(world: &World) -> Vec<(Entity, i32, RegionId)> {
    world
        .power_consumers
        .iter()
        .filter_map(|(entity, consumer)| match consumer.source {
            Some(PowerSource::Imported { source_region }) if consumer.powered => {
                Some((*entity, consumer.demand, source_region))
            }
            _ => None,
        })
        .collect()
}

/// Re-applies previously-held imported power to consumers that local resolution
/// left unpowered (mirrors `RegionState::apply_power_export_grant`). Keeps the
/// reservation reflected in the consumer flag and the supplied/shortage stats.
pub(crate) fn reapply_imported_power(world: &mut World, imported: &[(Entity, i32, RegionId)]) {
    for &(entity, demand, source_region) in imported {
        let Some(consumer) = world.power_consumers.get_mut(&entity) else {
            continue;
        };
        if consumer.powered || consumer.demand != demand {
            continue;
        }
        consumer.powered = true;
        consumer.source = Some(PowerSource::Imported { source_region });
        world.stats.power.total_power_supplied += demand;
    }
    world.stats.power.total_power_shortage =
        (world.stats.power.total_power_demand - world.stats.power.total_power_supplied).max(0);
}

#[derive(Debug, Clone, Copy)]
struct TickSummarySnapshot {
    population: i32,
    money: i32,
    happiness: i32,
    pollution: i32,
    unemployment: i32,
    powered_buildings: i32,
}

impl TickSummarySnapshot {
    fn from_world(world: &World) -> Self {
        Self {
            population: world.stats.population,
            money: world.resources.money,
            happiness: world.stats.happiness,
            pollution: world.stats.pollution,
            unemployment: world.stats.unemployment,
            powered_buildings: world
                .power_consumers
                .values()
                .filter(|consumer| consumer.powered)
                .count() as i32,
        }
    }
}

fn metric_change<T>(before: T, after: T) -> MetricChange<T> {
    MetricChange { before, after }
}

fn game_time_view(time: GameTime) -> GameTimeView {
    GameTimeView {
        total_hours: time.total_hours,
        year: time.year(),
        month: time.month(),
        week: time.week_of_month(),
        day: time.day_of_week(),
        hour: time.hour_of_day(),
        label: time.label(),
    }
}

#[cfg(test)]
mod tests {
    use super::{refresh_derived_state_for_world, tick_world};
    use crate::core::components::WorkplaceAssignment;
    use crate::core::regions::RegionId;
    use crate::core::resources::{CityStats, LocalEffectsMap};
    use crate::core::systems::citizens;
    use crate::core::systems::placement;
    use crate::core::world::World;
    use crate::interface::input::BuildingKind;

    #[test]
    fn citizen_happiness_decay_happens_on_daily_boundary_not_hourly() {
        let (mut world, residential) = world_with_one_citizen();

        for _ in 0..23 {
            assert!(tick_world(&mut world).success);
        }
        assert_eq!(citizen_happiness_decay(&world), 0);
        assert_eq!(
            citizens::average_happiness_for_home(&world, residential),
            Some(50)
        );

        assert!(tick_world(&mut world).success);

        let average_happiness =
            citizens::average_happiness_for_home(&world, residential).expect("happiness");
        assert_eq!(citizen_happiness_decay(&world), 1);
        assert!(average_happiness < 50);
    }

    #[test]
    fn population_growth_happens_on_daily_boundary_not_hourly() {
        let mut world = World::new(5, 3);
        placement::place_building(&mut world, 0, 0, BuildingKind::PowerPlant);
        placement::place_building(&mut world, 1, 0, BuildingKind::Residential);
        placement::place_building(&mut world, 2, 0, BuildingKind::Commercial);
        for x in 0..=2 {
            placement::place_building(&mut world, x, 1, BuildingKind::Road);
        }

        for _ in 0..23 {
            assert!(tick_world(&mut world).success);
        }
        assert_eq!(world.stats.population, 0);

        assert!(tick_world(&mut world).success);
        assert_eq!(world.stats.population, 1);
    }

    #[test]
    fn paused_derived_refresh_does_not_advance_money_population_or_age() {
        let mut world = World::new(3, 3);
        placement::place_building(&mut world, 0, 0, BuildingKind::Residential);
        let residential = world.grid.get(0, 0).expect("residential");
        citizens::spawn_for_home(&mut world, residential, 1);
        let citizen = *world.citizens.keys().next().expect("citizen");
        world.citizens.get_mut(&citizen).expect("citizen").age = 12;
        refresh_derived_state_for_world(&mut world, RegionId(1));

        placement::place_building(&mut world, 1, 0, BuildingKind::Park);
        let money_after_command = world.resources.money;
        let population_before_refresh = world.stats.population;
        let turn_before_refresh = world.resources.turn;
        let time_before_refresh = world.resources.time;
        let age_before_refresh = world.citizens.get(&citizen).expect("citizen").age;

        refresh_derived_state_for_world(&mut world, RegionId(1));

        assert_eq!(world.resources.money, money_after_command);
        assert_eq!(world.stats.population, population_before_refresh);
        assert_eq!(world.resources.turn, turn_before_refresh);
        assert_eq!(world.resources.time, time_before_refresh);
        assert_eq!(
            world.citizens.get(&citizen).expect("citizen").age,
            age_before_refresh
        );
    }

    #[test]
    fn paused_derived_refresh_is_idempotent_for_applied_derived_state() {
        let mut world = World::new(4, 3);
        placement::place_building(&mut world, 0, 0, BuildingKind::PowerPlant);
        placement::place_building(&mut world, 1, 0, BuildingKind::Residential);
        placement::place_building(&mut world, 2, 0, BuildingKind::Commercial);
        for x in 0..=2 {
            placement::place_building(&mut world, x, 1, BuildingKind::Road);
        }
        let residential = world.grid.get(1, 0).expect("residential");
        citizens::spawn_for_home(&mut world, residential, 1);

        refresh_derived_state_for_world(&mut world, RegionId(1));
        let once = DerivedSnapshot::from_world(&world);
        refresh_derived_state_for_world(&mut world, RegionId(1));
        let twice = DerivedSnapshot::from_world(&world);

        assert_eq!(twice, once);
    }

    fn world_with_one_citizen() -> (World, crate::core::entity::Entity) {
        let mut world = World::new(1, 1);
        let residential = world.spawn();
        citizens::spawn_for_home(&mut world, residential, 1);
        refresh_derived_state_for_world(&mut world, RegionId(1));
        (world, residential)
    }

    fn citizen_happiness_decay(world: &World) -> i32 {
        world
            .citizens
            .values()
            .next()
            .expect("citizen")
            .morale
            .decay
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct DerivedSnapshot {
        stats: CityStats,
        local_effects: LocalEffectsMap,
        morale_targets: Vec<(u64, i32)>,
        assignments: Vec<(u64, Option<WorkplaceAssignment>)>,
    }

    impl DerivedSnapshot {
        fn from_world(world: &World) -> Self {
            let mut morale_targets = world
                .citizens
                .iter()
                .map(|(entity, citizen)| (entity.0, citizen.morale.target))
                .collect::<Vec<_>>();
            morale_targets.sort_by_key(|(entity, _)| *entity);

            let mut assignments = world
                .citizens
                .iter()
                .map(|(entity, citizen)| (entity.0, citizen.workplace_assignment))
                .collect::<Vec<_>>();
            assignments.sort_by_key(|(entity, _)| *entity);

            Self {
                stats: world.stats.clone(),
                local_effects: world.local_effects.clone(),
                morale_targets,
                assignments,
            }
        }
    }
}
