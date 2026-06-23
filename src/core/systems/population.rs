//! Population growth system that spawns citizen entities for qualifying residential buildings.

use crate::core::systems::{
    citizens,
    local_effects::{self, DesirabilityLevel},
    road_connectivity,
};
use crate::core::world::World;

pub(crate) fn run(world: &mut World) {
    let mut available_jobs = available_jobs_for_growth(world);
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

        let desirability = world
            .positions
            .get(&entity)
            .map(|position| local_effects::desirability_level(world, position.x, position.y))
            .unwrap_or(DesirabilityLevel::Low);

        let Some(population) = world.populations.get(&entity) else {
            continue;
        };
        let current_population = citizens::citizen_count_for_home(world, entity);
        let growth_happiness =
            citizens::average_happiness_for_home(world, entity).unwrap_or(world.stats.happiness);
        let growth = residential_growth_per_tick(available_jobs, growth_happiness, desirability)
            .min(population.max - current_population)
            .min(available_jobs);
        if growth > 0 {
            citizens::spawn_for_home(world, entity, growth);
            available_jobs -= growth;
        }
    }

    citizens::sync_population_from_citizens(world);
}

pub(crate) fn available_jobs_for_growth(world: &World) -> i32 {
    (world.cached_job_resolution().remaining_slots + world.importable_remote_jobs).max(0)
}

fn residential_growth_per_tick(
    available_jobs: i32,
    happiness: i32,
    desirability: DesirabilityLevel,
) -> i32 {
    if happiness < 40 {
        return 0;
    }

    let demand_growth = if available_jobs >= 3 && happiness >= 50 {
        2
    } else if available_jobs > 0 {
        1
    } else {
        return 0;
    };
    let happiness_bonus = if happiness >= 70 { 1 } else { 0 };

    match desirability {
        DesirabilityLevel::High => demand_growth + 1 + happiness_bonus,
        DesirabilityLevel::Medium => demand_growth + happiness_bonus,
        DesirabilityLevel::Low => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::{available_jobs_for_growth, residential_growth_per_tick};
    use crate::core::city_refs::{CityCellRef, CityEntityRef};
    use crate::core::components::WorkplaceAssignment;
    use crate::core::entity::Entity;
    use crate::core::regions::RegionId;
    use crate::core::systems::{citizens, local_effects::DesirabilityLevel, placement};
    use crate::core::world::World;
    use crate::interface::input::BuildingKind;

    #[test]
    fn residential_growth_rate_follows_demand_thresholds() {
        assert_eq!(
            residential_growth_per_tick(3, 50, DesirabilityLevel::Medium),
            2
        );
        assert_eq!(
            residential_growth_per_tick(1, 40, DesirabilityLevel::Medium),
            1
        );
        assert_eq!(
            residential_growth_per_tick(3, 39, DesirabilityLevel::Medium),
            0
        );
        assert_eq!(
            residential_growth_per_tick(0, 80, DesirabilityLevel::Medium),
            0
        );
    }

    #[test]
    fn residential_growth_rate_uses_desirability() {
        assert_eq!(
            residential_growth_per_tick(3, 50, DesirabilityLevel::High),
            3
        );
        assert_eq!(
            residential_growth_per_tick(3, 50, DesirabilityLevel::Medium),
            2
        );
        assert_eq!(
            residential_growth_per_tick(3, 50, DesirabilityLevel::Low),
            0
        );
    }

    #[test]
    fn residential_growth_rate_uses_happiness_thresholds() {
        assert_eq!(
            residential_growth_per_tick(3, 39, DesirabilityLevel::Medium),
            0
        );
        assert_eq!(
            residential_growth_per_tick(3, 70, DesirabilityLevel::Medium),
            3
        );
    }

    #[test]
    fn available_jobs_for_growth_uses_local_remaining_slots_plus_remote_capacity() {
        let mut world = World::new(3, 2);
        placement::place_building(&mut world, 0, 0, BuildingKind::Residential);
        placement::place_building(&mut world, 1, 0, BuildingKind::Commercial);
        placement::place_building(&mut world, 0, 1, BuildingKind::Road);
        placement::place_building(&mut world, 1, 1, BuildingKind::Road);
        let home = world.grid.get(0, 0).expect("home");
        let workplace = world.grid.get(1, 0).expect("workplace");
        world.power_consumers.get_mut(&workplace).unwrap().powered = true;
        citizens::spawn_for_home(&mut world, home, 1);
        world
            .citizens
            .values_mut()
            .next()
            .unwrap()
            .workplace_assignment = Some(WorkplaceAssignment {
            workplace: CityEntityRef::local(RegionId(2), Entity(9)),
            location: CityCellRef::local(RegionId(2), 0, 0),
            salary: 1,
        });
        world.importable_remote_jobs = 2;

        assert_eq!(available_jobs_for_growth(&world), 4);
    }
}
