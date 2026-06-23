//! Citizen entity support, including home links, happiness, and aggregate population sync.

use crate::core::city_refs::{CitizenId, CityEntityRef};
use crate::core::components::{Citizen, Morale};
use crate::core::entity::Entity;
use crate::core::systems::{road_connectivity, road_network_analysis};
use crate::core::world::World;

const DAILY_HAPPINESS_DECAY: i32 = 1;

pub(crate) fn spawn_for_home(world: &mut World, residential: Entity, count: i32) {
    for _ in 0..count.max(0) {
        let citizen = world.spawn();
        world.attach_citizen(
            citizen,
            Citizen {
                id: CitizenId {
                    home_region: world.region_id,
                    local: citizen,
                },
                age: 0,
                home: CityEntityRef::local(world.region_id, residential),
                workplace_assignment: None,
                morale: Morale::default(),
                money: 0,
            },
        );
    }
}

pub(crate) fn sync_population_from_citizens(world: &mut World) {
    let counts: Vec<_> = world
        .populations
        .keys()
        .copied()
        .map(|residential| (residential, citizen_count_for_home(world, residential)))
        .collect();

    for (residential, count) in counts {
        if let Some(population) = world.populations.get_mut(&residential) {
            population.current = count.min(population.max);
        }
    }
}

pub(crate) fn apply_daily_happiness_decay(world: &mut World) {
    let mut citizens: Vec<_> = world.citizens.keys().copied().collect();
    citizens.sort_by_key(|citizen| citizen.0);

    for citizen in citizens {
        if let Some(citizen) = world.citizens.get_mut(&citizen) {
            citizen.morale.decay = (citizen.morale.decay + DAILY_HAPPINESS_DECAY).min(100);
        }
    }
}

pub(crate) fn update_happiness(world: &mut World) {
    let mut citizens: Vec<_> = world.citizens.keys().copied().collect();
    citizens.sort_by_key(|citizen| citizen.0);

    for citizen in citizens {
        let happiness = actual_happiness(world, citizen);
        if let Some(citizen) = world.citizens.get_mut(&citizen) {
            citizen.morale.actual = happiness;
        }
    }
}

/// Recomputes the derived, conditions-only happiness target.
///
/// ```text
/// roads/power/jobs/amenities/pollution -> happiness_target
/// daily decay + rent stress             -> actual happiness on tick
/// ```
pub(crate) fn update_happiness_targets(world: &mut World) {
    let mut citizens: Vec<_> = world.citizens.keys().copied().collect();
    citizens.sort_by_key(|citizen| citizen.0);

    for citizen in citizens {
        let target = citizen_happiness_target(world, citizen);
        if let Some(citizen) = world.citizens.get_mut(&citizen) {
            citizen.morale.target = target;
        }
    }
}

pub(crate) fn citizen_count(world: &World) -> i32 {
    world.citizens.len() as i32
}

pub(crate) fn citizen_count_for_home(world: &World, residential: Entity) -> i32 {
    world
        .citizens
        .values()
        .filter(|citizen| citizen.home.entity == residential)
        .count() as i32
}

pub(crate) fn average_happiness_for_home(world: &World, residential: Entity) -> Option<i32> {
    let mut total = 0;
    let mut count = 0;
    for citizen in world.citizens.values() {
        if citizen.home.entity != residential {
            continue;
        }
        total += citizen.morale.actual;
        count += 1;
    }

    if count > 0 { Some(total / count) } else { None }
}

pub(crate) fn average_happiness(world: &World) -> Option<i32> {
    let count = world.citizens.len() as i32;
    if count == 0 {
        return None;
    }

    let total: i32 = world
        .citizens
        .values()
        .map(|citizen| citizen.morale.actual)
        .sum();
    Some(total / count)
}

pub(crate) fn average_happiness_target_for_home(world: &World, residential: Entity) -> Option<i32> {
    let mut total = 0;
    let mut count = 0;
    for citizen in world.citizens.values() {
        if citizen.home.entity != residential {
            continue;
        }
        total += display_happiness(citizen.morale.target);
        count += 1;
    }

    if count > 0 { Some(total / count) } else { None }
}

pub(crate) fn average_happiness_target(world: &World) -> Option<i32> {
    let count = world.citizens.len() as i32;
    if count == 0 {
        return None;
    }

    let total: i32 = world
        .citizens
        .values()
        .map(|citizen| display_happiness(citizen.morale.target))
        .sum();
    Some(total / count)
}

pub(crate) fn average_money_for_home(world: &World, residential: Entity) -> Option<i32> {
    let mut total = 0;
    let mut count = 0;
    for citizen in world.citizens.values() {
        if citizen.home.entity != residential {
            continue;
        }
        total += citizen.money;
        count += 1;
    }

    if count > 0 { Some(total / count) } else { None }
}

pub(crate) fn average_money(world: &World) -> Option<i32> {
    let count = world.citizens.len() as i32;
    if count == 0 {
        return None;
    }

    let total: i32 = world.citizens.values().map(|citizen| citizen.money).sum();
    Some(total / count)
}

fn actual_happiness(world: &World, citizen: Entity) -> i32 {
    let Some(citizen) = world.citizens.get(&citizen) else {
        return 50;
    };

    let mut happiness = citizen.morale.target - citizen.morale.decay;
    if world.positions.contains_key(&citizen.home.entity) {
        happiness -= citizen.morale.rent_stress * 10;
    }

    display_happiness(happiness)
}

fn citizen_happiness_target(world: &World, citizen: Entity) -> i32 {
    let Some(citizen) = world.citizens.get(&citizen) else {
        return 50;
    };
    // Home is always local to this region, so `.entity` is its local building id.
    let home = citizen.home.entity;
    let Some(position) = world.positions.get(&home) else {
        return 50;
    };

    let effects = world.local_effects.get(position.x, position.y);
    let powered = world
        .power_consumers
        .get(&home)
        .is_some_and(|consumer| consumer.powered);
    let road_connected = road_connectivity::is_road_connected(world, home);

    let mut happiness = 35 + effects.desirability * 6 + effects.accessibility;
    happiness -= effects.pollution_pressure * 3;
    if citizen.workplace_assignment.is_none() {
        happiness -= 10;
    }
    let road_access = road_network_analysis::access_for(world, home);
    happiness -= road_network_analysis::commute_penalty(road_access.commute_distance);
    if road_access.nearest_shop_distance.is_none() {
        happiness -= 2;
    }
    if !powered {
        happiness -= 15;
    }
    if !road_connected {
        happiness -= 10;
    }

    happiness
}

fn display_happiness(happiness: i32) -> i32 {
    happiness.clamp(0, 100)
}

#[cfg(test)]
mod tests {
    use super::{
        apply_daily_happiness_decay, average_happiness_for_home, average_happiness_target_for_home,
        citizen_count_for_home, spawn_for_home, update_happiness, update_happiness_targets,
    };
    use crate::core::components::PowerConsumer;
    use crate::core::resources::LocalEffects;
    use crate::core::systems::placement;
    use crate::core::world::World;
    use crate::interface::input::BuildingKind;

    #[test]
    fn spawning_citizens_links_them_to_home() {
        let mut world = World::new(2, 2);
        let residential = world.spawn();

        spawn_for_home(&mut world, residential, 3);

        assert_eq!(world.citizens.len(), 3);
        assert_eq!(citizen_count_for_home(&world, residential), 3);
        assert_eq!(average_happiness_for_home(&world, residential), Some(50));
    }

    #[test]
    fn daily_happiness_decay_lowers_citizens_by_one() {
        let mut world = World::new(2, 2);
        let residential = world.spawn();
        spawn_for_home(&mut world, residential, 1);

        apply_daily_happiness_decay(&mut world);
        update_happiness_targets(&mut world);
        update_happiness(&mut world);

        assert_eq!(average_happiness_for_home(&world, residential), Some(49));
        assert_eq!(
            average_happiness_target_for_home(&world, residential),
            Some(50)
        );
    }

    #[test]
    fn daily_happiness_decay_keeps_citizens_clamped_at_zero() {
        let mut world = World::new(2, 2);
        let residential = world.spawn();
        spawn_for_home(&mut world, residential, 1);

        for _ in 0..150 {
            apply_daily_happiness_decay(&mut world);
            update_happiness_targets(&mut world);
            update_happiness(&mut world);
        }

        assert_eq!(average_happiness_for_home(&world, residential), Some(0));
    }

    #[test]
    fn high_amenity_target_uses_single_final_clamp_for_actual_happiness() {
        let mut world = World::new(1, 1);
        placement::place_building(&mut world, 0, 0, BuildingKind::Residential);
        let residential = world.grid.get(0, 0).expect("residential");
        world.power_consumers.insert(
            residential,
            PowerConsumer {
                demand: 1,
                powered: true,
                source: None,
            },
        );
        world.local_effects.cells[0] = LocalEffects {
            land_value: 4,
            pollution_pressure: 0,
            accessibility: 20,
            desirability: 20,
        };
        spawn_for_home(&mut world, residential, 1);
        let citizen = *world.citizens.keys().next().expect("citizen");
        world
            .citizens
            .get_mut(&citizen)
            .expect("citizen")
            .morale
            .decay = 60;

        update_happiness_targets(&mut world);
        update_happiness(&mut world);

        let raw_target = world.citizens.get(&citizen).expect("citizen").morale.target;
        assert!(raw_target > 100, "fixture must exercise an over-100 target");
        assert_eq!(
            average_happiness_target_for_home(&world, residential),
            Some(100)
        );
        assert_eq!(
            average_happiness_for_home(&world, residential),
            Some(raw_target - 60),
            "actual happiness must clamp once after subtracting decay"
        );
    }
}
