//! Citizen entity support, including home links, happiness, and aggregate population sync.

use crate::core::components::Citizen;
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
                age: 0,
                home: residential,
                workplace: None,
                remote_workplace: None,
                happiness: 50,
                happiness_decay: 0,
                money: 0,
                rent_stress: 0,
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
            citizen.happiness_decay = (citizen.happiness_decay + DAILY_HAPPINESS_DECAY).min(100);
        }
    }
}

pub(crate) fn update_happiness(world: &mut World) {
    let mut citizens: Vec<_> = world.citizens.keys().copied().collect();
    citizens.sort_by_key(|citizen| citizen.0);

    for citizen in citizens {
        let happiness = citizen_happiness(world, citizen);
        if let Some(citizen) = world.citizens.get_mut(&citizen) {
            citizen.happiness = happiness;
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
        .filter(|citizen| citizen.home == residential)
        .count() as i32
}

pub(crate) fn average_happiness_for_home(world: &World, residential: Entity) -> Option<i32> {
    let mut total = 0;
    let mut count = 0;
    for citizen in world.citizens.values() {
        if citizen.home != residential {
            continue;
        }
        total += citizen.happiness;
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
        .map(|citizen| citizen.happiness)
        .sum();
    Some(total / count)
}

pub(crate) fn average_money_for_home(world: &World, residential: Entity) -> Option<i32> {
    let mut total = 0;
    let mut count = 0;
    for citizen in world.citizens.values() {
        if citizen.home != residential {
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

fn citizen_happiness(world: &World, citizen: Entity) -> i32 {
    let Some(citizen) = world.citizens.get(&citizen) else {
        return 50;
    };
    let Some(position) = world.positions.get(&citizen.home) else {
        return (50 - citizen.happiness_decay).clamp(0, 100);
    };

    let effects = world.local_effects.get(position.x, position.y);
    let powered = world
        .power_consumers
        .get(&citizen.home)
        .is_some_and(|consumer| consumer.powered);
    let road_connected = road_connectivity::is_road_connected(world, citizen.home);

    let mut happiness = 35 + effects.desirability * 6 + effects.accessibility;
    happiness -= effects.pollution_pressure * 3;
    if citizen.workplace.is_none() {
        happiness -= 10;
    }
    let road_access = road_network_analysis::access_for(world, citizen.home);
    happiness -= road_network_analysis::commute_penalty(road_access.commute_distance);
    if road_access.nearest_shop_distance.is_none() {
        happiness -= 2;
    }
    happiness -= citizen.rent_stress * 10;
    if !powered {
        happiness -= 15;
    }
    if !road_connected {
        happiness -= 10;
    }
    happiness -= citizen.happiness_decay;

    happiness.clamp(0, 100)
}

#[cfg(test)]
mod tests {
    use super::{
        apply_daily_happiness_decay, average_happiness_for_home, citizen_count_for_home,
        spawn_for_home, update_happiness,
    };
    use crate::core::world::World;

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
        update_happiness(&mut world);

        assert_eq!(average_happiness_for_home(&world, residential), Some(49));
    }

    #[test]
    fn daily_happiness_decay_keeps_citizens_clamped_at_zero() {
        let mut world = World::new(2, 2);
        let residential = world.spawn();
        spawn_for_home(&mut world, residential, 1);

        for _ in 0..150 {
            apply_daily_happiness_decay(&mut world);
            update_happiness(&mut world);
        }

        assert_eq!(average_happiness_for_home(&world, residential), Some(0));
    }
}
