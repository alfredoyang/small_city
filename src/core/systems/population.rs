use crate::core::systems::road_connectivity;
use crate::core::world::World;

pub(crate) fn run(world: &mut World) {
    let mut available_jobs = (world.stats.jobs - world.stats.population).max(0);
    let mut residential_entities: Vec<_> = world.populations.keys().copied().collect();
    // Sort by entity id so population growth is deterministic across HashMap iteration orders.
    residential_entities.sort_by_key(|entity| entity.0);

    for entity in residential_entities {
        if available_jobs <= 0 {
            break;
        }

        let powered = world
            .power_consumers
            .get(&entity)
            .map(|consumer| consumer.powered)
            .unwrap_or(false);
        if !powered {
            continue;
        }
        if !road_connectivity::is_road_connected(world, entity) {
            continue;
        }

        let Some(population) = world.populations.get_mut(&entity) else {
            continue;
        };
        let growth = residential_growth_per_tick(available_jobs, world.stats.happiness)
            .min(population.max - population.current)
            .min(available_jobs);
        if growth > 0 {
            population.current += growth;
            available_jobs -= growth;
        }
    }
}

fn residential_growth_per_tick(available_jobs: i32, happiness: i32) -> i32 {
    if available_jobs >= 3 && happiness >= 50 {
        2
    } else if available_jobs > 0 && happiness >= 35 {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::residential_growth_per_tick;

    #[test]
    fn residential_growth_rate_follows_demand_thresholds() {
        assert_eq!(residential_growth_per_tick(3, 50), 2);
        assert_eq!(residential_growth_per_tick(1, 35), 1);
        assert_eq!(residential_growth_per_tick(3, 34), 0);
        assert_eq!(residential_growth_per_tick(0, 80), 0);
    }
}
