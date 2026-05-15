use crate::core::components::{Citizen, CitizenHappiness, Employment, Home};
use crate::core::entity::Entity;
use crate::core::systems::road_connectivity;
use crate::core::world::World;

pub(crate) fn spawn_for_home(world: &mut World, residential: Entity, count: i32) {
    for _ in 0..count.max(0) {
        let citizen = world.spawn();
        world.attach_citizen(citizen, Citizen { age: 0 });
        world.attach_home(citizen, Home { residential });
        world.attach_employment(citizen, Employment { workplace: None });
        world.attach_citizen_happiness(citizen, CitizenHappiness { value: 50 });
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

pub(crate) fn update_happiness(world: &mut World) {
    let mut citizens: Vec<_> = world.citizens.keys().copied().collect();
    citizens.sort_by_key(|citizen| citizen.0);

    for citizen in citizens {
        let happiness = citizen_happiness(world, citizen);
        world.attach_citizen_happiness(citizen, CitizenHappiness { value: happiness });
    }
}

pub(crate) fn citizen_count(world: &World) -> i32 {
    world.citizens.len() as i32
}

pub(crate) fn citizen_count_for_home(world: &World, residential: Entity) -> i32 {
    world
        .homes
        .values()
        .filter(|home| home.residential == residential)
        .count() as i32
}

pub(crate) fn average_happiness_for_home(world: &World, residential: Entity) -> Option<i32> {
    let mut total = 0;
    let mut count = 0;
    for (citizen, home) in &world.homes {
        if home.residential != residential {
            continue;
        }
        let Some(happiness) = world.citizen_happiness.get(citizen) else {
            continue;
        };
        total += happiness.value;
        count += 1;
    }

    if count > 0 { Some(total / count) } else { None }
}

pub(crate) fn average_happiness(world: &World) -> Option<i32> {
    let count = world.citizen_happiness.len() as i32;
    if count == 0 {
        return None;
    }

    let total: i32 = world
        .citizen_happiness
        .values()
        .map(|happiness| happiness.value)
        .sum();
    Some(total / count)
}

fn citizen_happiness(world: &World, citizen: Entity) -> i32 {
    let Some(home) = world.homes.get(&citizen) else {
        return 50;
    };
    let Some(position) = world.positions.get(&home.residential) else {
        return 50;
    };

    let effects = world.local_effects.get(position.x, position.y);
    let powered = world
        .power_consumers
        .get(&home.residential)
        .is_some_and(|consumer| consumer.powered);
    let road_connected = road_connectivity::is_road_connected(world, home.residential);

    let mut happiness = 35 + effects.desirability * 6 + effects.accessibility;
    happiness -= effects.pollution_pressure * 3;
    if !powered {
        happiness -= 15;
    }
    if !road_connected {
        happiness -= 10;
    }

    happiness.clamp(0, 100)
}

#[cfg(test)]
mod tests {
    use super::{average_happiness_for_home, citizen_count_for_home, spawn_for_home};
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
}
