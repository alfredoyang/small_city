//! Shared deterministic simulation helpers for core facades and regional state.
//!
//! This module owns world-level simulation ordering that is shared by the
//! regional `RegionState`. It remains crate-local so UI code cannot receive or
//! manipulate ECS `World` storage directly.

use crate::core::resources::{GameTime, is_new_day, is_new_week};
use crate::core::systems::{
    business_growth, citizens, economy, happiness, local_effects, pollution, population, power,
    road_network_analysis, stats,
};
use crate::core::world::World;
use crate::interface::events::{CommandResult, EconomyBreakdownView, GameEventView, MetricChange};
use crate::interface::view::GameTimeView;

pub(crate) fn tick_world(world: &mut World) -> CommandResult {
    let before = TickSummarySnapshot::from_world(world);
    let before_time = world.resources.time;
    world.resources.time.advance_hours(1);
    let after_time = world.resources.time;
    power::run(world);
    stats::run(world);
    local_effects::run(world);
    if is_new_day(before_time, after_time) {
        citizens::apply_daily_happiness_decay(world);
    }
    if is_new_day(before_time, after_time) {
        population::run(world);
    }
    citizens::update_happiness(world);
    local_effects::run(world);
    let economy = if is_new_day(before_time, after_time) {
        economy::run(world)
    } else {
        economy::EconomyBreakdown::default()
    };
    let business_upgrades = if is_new_week(before_time, after_time) {
        business_growth::run(world).upgrades
    } else {
        Vec::new()
    };
    stats::refresh_population_and_jobs(world);
    pollution::run(world);
    happiness::run(world);
    world.resources.turn += 1;
    let after = TickSummarySnapshot::from_world(world);

    let tick_summary = GameEventView::TickSummary {
        turn: world.resources.turn,
        time: game_time_view(world.resources.time),
        population: metric_change(before.population, after.population),
        money: metric_change(before.money, after.money),
        happiness: metric_change(before.happiness, after.happiness),
        pollution: metric_change(before.pollution, after.pollution),
        unemployment: metric_change(before.unemployment, after.unemployment),
        powered_buildings: metric_change(before.powered_buildings, after.powered_buildings),
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

pub(crate) fn refresh_derived_state_for_world(world: &mut World) {
    power::run(world);
    road_network_analysis::run(world);
    stats::refresh_population_and_jobs(world);
    pollution::run(world);
    citizens::update_happiness(world);
    happiness::run(world);
    local_effects::run(world);
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

    fn world_with_one_citizen() -> (World, crate::core::entity::Entity) {
        let mut world = World::new(1, 1);
        let residential = world.spawn();
        citizens::spawn_for_home(&mut world, residential, 1);
        refresh_derived_state_for_world(&mut world);
        (world, residential)
    }

    fn citizen_happiness_decay(world: &World) -> i32 {
        world
            .citizens
            .values()
            .next()
            .expect("citizen")
            .happiness_decay
    }
}
