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
//! Event-driven plan (docs/20260703-event-driven-architecture.md, P-1..P-6):
//! this local ordering is unchanged — every step above still runs in the same
//! sequence when it runs. What changed is *whether* two of them run at all on
//! a quiet tick: `derived: power` is skipped entirely by
//! `begin_tick_power_phase_quiet` (P-6) when nothing that could affect power
//! changed since the last reconcile, and the *cross-region* remote grant
//! round-trip (not shown in this local-only diagram; see
//! `RegionRuntime::start_tick_power_phase` / `enter_job_phase` /
//! `enter_goods_phase`) is skipped per-resource by the `*_exports_dirty` +
//! discovery-generation gates (P-2/P-4/P-5). A quiet tick's only writes are
//! time, turn, and the DT2 (time-driven) outputs above.
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
/// Regional runtimes split the tick here: local job assignment has run, and the
/// employment ledger reconciles cross-region work, before `finish_tick_after_job_phase`
/// settles the economy.
pub(crate) struct TickJobPhase {
    before: TickSummarySnapshot,
    before_time: GameTime,
    after_time: GameTime,
    is_daily: bool,
    /// Whether the daily employment reconciliation actually ran. Computed AFTER
    /// `population::run` inside `continue_to_job_phase` (not snapshotted before
    /// it), so a citizen spawned by growth THIS tick is never missed -- see that
    /// function's doc comment (caught in review).
    /// Always `false` on an hourly (non-daily) tick.
    jobs_dirty: bool,
}

impl TickJobPhase {
    /// Whether this tick crosses a daily boundary, when jobs and the economy
    /// resolve. Cross-region employment only engages on daily ticks.
    pub(crate) fn is_daily(&self) -> bool {
        self.is_daily
    }

    /// Whether the daily local-match / employment reconciliation ran this tick.
    /// Only meaningful when `is_daily()`; always `false` otherwise.
    pub(crate) fn jobs_dirty(&self) -> bool {
        self.jobs_dirty
    }
}

/// Advances the clock and derived state shared by both power-phase entry
/// points, before either decides whether to run `power::run`.
fn begin_tick_time_advance(world: &mut World, local_region: RegionId) -> TickPowerPhase {
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

    TickPowerPhase {
        before,
        before_time,
        after_time,
    }
}

/// Starts one tick and resolves local power before downstream systems read it.
///
/// Regional runtimes can pause after this phase to request producer-exported
/// power, then call `finish_tick_after_power_phase` once export grants apply.
pub(crate) fn begin_tick_power_phase(world: &mut World, local_region: RegionId) -> TickPowerPhase {
    let phase = begin_tick_time_advance(world, local_region);
    power::run(world);
    phase
}

/// Event-driven plan, P-6: the quiet-tick counterpart of `begin_tick_power_phase`.
/// Skips `power::run` entirely rather than relying on it being a cheap no-op —
/// the caller (`RegionRuntime::start_tick_power_phase`'s quiet branch) has
/// already established that nothing which could affect power changed since
/// the last reconcile, so every consumer's grant is already exactly what a
/// fresh recompute would produce; running it again would only re-write
/// identical values across every consumer, at O(consumers) cost, for nothing.
pub(crate) fn begin_tick_power_phase_quiet(
    world: &mut World,
    local_region: RegionId,
) -> TickPowerPhase {
    begin_tick_time_advance(world, local_region)
}

/// Chains the job phase for the synchronous (single-region) tick path.
///
/// Regional runtimes call `continue_to_job_phase` and `finish_tick_after_job_phase`
/// separately so the goods phase and economy can resolve between them.
#[cfg(test)]
pub(crate) fn finish_tick_after_power_phase(
    world: &mut World,
    local_region: RegionId,
    phase: TickPowerPhase,
) -> CommandResult {
    // Test-only single-region path: no cross-region reconcile gate exists
    // here, so always run the local job rematch (discovery_dirty: true forces
    // jobs_dirty true regardless of the fresh in-function check).
    let job_phase = continue_to_job_phase(world, local_region, phase, true);
    finish_tick_after_job_phase(world, job_phase, &[])
}

/// Runs the post-power systems and, on a daily boundary, local job assignment.
///
/// Local assignment happens here (before the economy settles salaries/taxes) so a
/// citizen left without a reachable local slot becomes a candidate the employment
/// ledger can claim an imported remote workplace for.
///
/// The daily local job assignment is gated on jobs-dirtiness, not unconditional,
/// so a quiet day (nothing jobs-relevant changed locally or remotely) skips it
/// entirely, leaving every citizen's existing assignment -- local AND remote --
/// untouched. This closes a real bug: applying a remote assignment no longer
/// re-dirties the gate (see `World::refresh_jobs_cache_after_grant_applied`), so a
/// settled remote worker is left alone instead of being cleared and re-matched
/// (jobless, unpaid) every single day forever.
///
/// `discovery_dirty` is the caller's own reconcile gate (discovery
/// generation moved) -- snapshotted before this runs, since it cannot
/// change mid-tick. It is OR'd with a FRESH read of `jobs_exports_dirty`
/// taken AFTER `population::run`, not a value snapshotted before it: caught
/// in review, `population::run` can spawn a citizen this same tick (via
/// `attach_citizen`, which itself sets `jobs_exports_dirty`), and a citizen
/// born this tick must not wait a full extra day for its first job
/// attempt. The effective result is returned via `TickJobPhase::jobs_dirty`
/// for the caller to act on (release/request, generation bookkeeping).
pub(crate) fn continue_to_job_phase(
    world: &mut World,
    local_region: RegionId,
    phase: TickPowerPhase,
    discovery_dirty: bool,
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
    // Directory employment ledger plan, P7-d: the daily-employment gate. Fires
    // on a local job-relevant change, on a connectivity change (`discovery_dirty`
    // now means "the component graph moved", not the raw generation), or when a
    // citizen still wants work (a loss clears an assignment without dirtying the
    // gate). `has_unassigned_citizen` is read here, AFTER population growth, so a
    // citizen spawned this tick is noticed the same day.
    let jobs_dirty = is_daily
        && (world.is_jobs_exports_dirty() || discovery_dirty || world.has_unassigned_citizen());
    if jobs_dirty {
        // `assign_local_jobs` matches the region's still-jobless local seekers.
        // Its two guards preserve every remote assignment the ledger owns, and the
        // registry holds employer-contracted seats out of local reach (P7-a).
        economy::assign_local_jobs(world, local_region);
    }

    TickJobPhase {
        before: phase.before,
        before_time: phase.before_time,
        after_time: phase.after_time,
        is_daily,
        jobs_dirty,
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
    finish_tick_after_goods_phase_with_prepared_goods(
        world,
        phase,
        exported_job_slots,
        exported_goods_units,
        None,
    )
}

pub(crate) fn finish_tick_after_goods_phase_with_prepared_goods(
    world: &mut World,
    phase: TickJobPhase,
    exported_job_slots: &[Entity],
    exported_goods_units: u32,
    prepared_goods_flow: Option<economy::PreparedGoodsFlow>,
) -> CommandResult {
    let economy = if phase.is_daily {
        let economy = economy::run_with_prepared_goods_flow(
            world,
            exported_job_slots,
            exported_goods_units,
            prepared_goods_flow,
        );
        // Event-driven plan, P-1: daily settlement writes goods stock through
        // prepared local grants/imports and `consume_local_good`, bypassing
        // the ordinary building mutation chokepoints. Mark explicitly so a
        // goods-only change still republishes this region's availability hints.
        world.mark_hints_dirty();
        // Event-driven plan, P-5: same bypass applies to the goods export
        // dirty flag, but UNLIKE hints (a cheap, idempotence-checked
        // republish), a spurious mark here forces a real cross-region round
        // trip attempt. Marking unconditionally on every daily tick would
        // dirty every region's goods gate forever, regardless of whether it
        // has any commercial/industrial building at all (this call site is
        // gated only on the daily boundary, not on goods activity) — making
        // the whole gate a no-op. So: only when the settlement's own
        // breakdown shows nonzero goods activity, which is exactly what a
        // goods-inactive region (zero commercial/industrial buildings)
        // never has.
        if economy.local_goods_produced != 0
            || economy.local_goods_sold != 0
            || economy.imported_goods_sold != 0
            || economy.exported_goods != 0
        {
            world.mark_goods_exports_dirty();
        }
        economy
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
    // Event-driven plan, P-3: `power::run` diff-applies — a consumer with no
    // fresh local grant keeps its existing `Imported` source structurally, so
    // running this derived pass for a paused config change (build/bulldoze)
    // no longer needs a capture/restore to protect still-valid imports; the
    // next real tick re-derives imports authoritatively through the
    // cross-region flow, same as before.
    power::run(world);
    road_network_analysis::run(world);
    stats::refresh_population_and_jobs(world);
    pollution::run(world);
    local_effects::run(world);
    economy::assign_local_jobs(world, local_region);
    citizens::update_happiness_targets(world);
    happiness::run(world);
}

/// Imported-power grants currently held by consumers. Used by
/// `RegionState::begin_tick_power_demand_phase` (`regions/mod.rs`), which
/// pairs this with `clear_imported_power` and `reapply_imported_power` to
/// protect reads that happen later in the same tick, after the recompute in
/// `begin_tick_power_phase`, while a dirty reconcile's fresh demand batch is
/// still in flight — see `docs/20260703-bug-cross-region-export-starvation-fix.md`.
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

/// Clears previously-held imported power on the given consumers (event-driven
/// plan, P-3). Diff-apply `power::run` keeps an existing `Imported` source by
/// default when no fresh local grant covers a consumer, so a dirty reconcile
/// — about to release every producer reservation and request only what this
/// tick's demand scan finds — must explicitly clear the ones it captured
/// first. Skipping this would leave an import-needing consumer still reading
/// as `powered`, so `pending_power_demands` (which skips anything already
/// powered) would never include it in the fresh batch even though its old
/// reservation is unconditionally released — the starvation fix's round-1
/// desync, reintroduced. `power::run` (called after this) recomputes
/// `world.stats.power` from scratch, so no stats adjustment is needed here.
///
/// Invalidates the jobs registry itself (once, if anything was actually
/// cleared): this clearing happens *before* `power::run`'s own before/after
/// snapshot, so `power::run`'s `power_state_changed` check — which used to
/// catch this transition for free when it was the one doing the clearing —
/// can no longer see it. A consumer that ends up restored by
/// `reapply_imported_power` right after nets out to no observable change (an
/// `apply_power_export_grant` reply invalidates jobs again either way once
/// the round trip resolves), but one that is *not* restored (e.g. it lost its
/// border connection) has a real, lasting transition that nothing else would
/// ever flag.
pub(crate) fn clear_imported_power(world: &mut World, imported: &[(Entity, i32, RegionId)]) {
    if imported.is_empty() {
        return;
    }
    for &(entity, ..) in imported {
        if let Some(consumer) = world.power_consumers.get_mut(&entity) {
            consumer.powered = false;
            consumer.source = None;
        }
    }
    world.invalidate_jobs_registry();
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
    use super::{
        begin_tick_power_phase, begin_tick_power_phase_quiet, continue_to_job_phase,
        finish_tick_after_job_phase, refresh_derived_state_for_world, tick_world,
    };
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
    fn jobs_dirty_is_rechecked_after_population_spawns_a_citizen_same_tick() {
        // Retire-tickstate, P-c (caught in review): `jobs_dirty` is read
        // AFTER `population::run`, not snapshotted before it -- a citizen
        // spawned by growth THIS SAME daily tick must be noticed the same
        // day, not missed because nothing else was dirty when the tick
        // started. Drives `continue_to_job_phase` directly (not `tick_world`,
        // which always passes `discovery_dirty: true` and would hide this).
        let mut world = World::new(5, 3);
        placement::place_building(&mut world, 0, 0, BuildingKind::PowerPlant);
        placement::place_building(&mut world, 1, 0, BuildingKind::Residential);
        placement::place_building(&mut world, 2, 0, BuildingKind::Commercial);
        for x in 0..=2 {
            placement::place_building(&mut world, x, 1, BuildingKind::Road);
        }

        for _ in 0..23 {
            let phase = begin_tick_power_phase(&mut world, RegionId(1));
            let job_phase = continue_to_job_phase(&mut world, RegionId(1), phase, false);
            assert!(!job_phase.is_daily());
            finish_tick_after_job_phase(&mut world, job_phase, &[]);
        }
        assert!(world.citizens.is_empty(), "setup: no growth yet");

        // Genuinely quiet at entry: nothing (placement's own dirtying
        // included) should be why the next assertion passes.
        world.clear_jobs_exports_dirty();
        assert!(!world.is_jobs_exports_dirty(), "setup: quiet at entry");

        let phase = begin_tick_power_phase(&mut world, RegionId(1));
        let job_phase = continue_to_job_phase(&mut world, RegionId(1), phase, false);

        assert!(job_phase.is_daily());
        assert_eq!(world.citizens.len(), 1, "setup: growth happened this tick");
        assert!(
            job_phase.jobs_dirty(),
            "a citizen spawned by population growth THIS tick must be \
             noticed the same day, not missed because jobs_dirty was \
             snapshotted before population::run ran"
        );
        finish_tick_after_job_phase(&mut world, job_phase, &[]);
    }

    #[test]
    fn begin_tick_power_phase_quiet_skips_power_run_entirely() {
        // Event-driven plan, P-6: the quiet variant must not merely make
        // `power::run` cheap (that's P-3's diff-apply) — it must not call it
        // at all. Prove this by corrupting a properly-powered consumer's
        // state directly (bypassing every chokepoint, so nothing marks
        // anything dirty) and confirming the quiet phase leaves the
        // corruption untouched, while the regular (dirty) phase still
        // corrects it on the same world.
        let mut world = World::new(4, 2);
        placement::place_building(&mut world, 0, 0, BuildingKind::PowerPlant);
        placement::place_building(&mut world, 1, 0, BuildingKind::Road);
        placement::place_building(&mut world, 2, 0, BuildingKind::Residential);
        let consumer = world.grid.get(2, 0).expect("residential");
        begin_tick_power_phase(&mut world, RegionId(1));
        assert!(
            world.power_consumers[&consumer].powered,
            "setup: consumer should be locally powered before corruption"
        );

        world.power_consumers.get_mut(&consumer).unwrap().powered = false;
        world.power_consumers.get_mut(&consumer).unwrap().source = None;

        begin_tick_power_phase_quiet(&mut world, RegionId(1));
        assert!(
            !world.power_consumers[&consumer].powered,
            "quiet phase must not run power::run: the corruption must persist"
        );

        begin_tick_power_phase(&mut world, RegionId(1));
        assert!(
            world.power_consumers[&consumer].powered,
            "the regular (dirty) phase must still run power::run and correct the state"
        );
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
