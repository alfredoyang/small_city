//! Road-network power system with plant capacity, consumer demand, and deterministic allocation.

use std::collections::{HashMap, HashSet};

use crate::core::components::PowerSource;
use crate::core::entity::Entity;
use crate::core::resource_registry::PowerGrant;
use crate::core::resources::PowerStats;
use crate::core::systems::road_connectivity;
use crate::core::world::World;

pub(crate) fn run(world: &mut World) {
    let before = world
        .power_consumers
        .iter()
        .map(|(entity, consumer)| (*entity, consumer.powered, consumer.source))
        .collect::<Vec<_>>();

    // Event-driven plan, P-3: diff-apply instead of clear-then-reapply. A
    // consumer with no local grant this pass KEEPS an existing `Imported`
    // source untouched — imports now survive a raw `power::run` structurally,
    // so the capture/reapply-all wrapper `begin_tick_power_phase_quiet` used
    // to need, and the capture/restore `refresh_derived_state_for_world`
    // used to need, are both gone. The dirty reconcile path
    // (`begin_tick_power_demand_phase`, `regions/mod.rs`) still explicitly
    // clears + filter-restores around this call, because it needs kept
    // imports to become visible to that tick's fresh demand scan before its
    // reservations are released — diff-apply alone would hide them from it.
    // A consumer that gets a fresh local grant always overwrites any prior
    // source, imported or not.
    let resolution = world.cached_power_resolution();
    let local_grants: HashMap<Entity, &PowerGrant> = resolution
        .grants
        .iter()
        .map(|grant| (grant.consumer, grant))
        .collect();

    // `resolution`'s stats only ever reflect LOCAL supply — imports are a
    // cross-region concept the registry has no visibility into. A kept
    // import's demand must be added back in here, or it silently vanishes
    // from total_power_supplied (and inflates shortage) the instant nothing
    // downstream still re-adds it, which is now true on quiet ticks and the
    // paused derived-refresh path (neither has a restore step anymore).
    let mut kept_imported_demand = 0;
    for (entity, consumer) in world.power_consumers.iter_mut() {
        match local_grants.get(entity) {
            Some(grant) if consumer.demand == grant.amount => {
                consumer.powered = true;
                consumer.source = Some(grant.source);
            }
            _ => {
                if matches!(consumer.source, Some(PowerSource::Imported { .. })) {
                    kept_imported_demand += consumer.demand;
                } else {
                    consumer.powered = false;
                    consumer.source = None;
                }
            }
        }
    }

    let total_power_supplied = resolution.total_supplied + kept_imported_demand;
    world.stats.power = PowerStats {
        total_power_capacity: resolution.total_capacity,
        total_power_demand: resolution.total_demand,
        total_power_supplied,
        total_power_shortage: (resolution.total_demand - total_power_supplied).max(0),
    };

    let power_state_changed = before.iter().any(|(entity, powered, source)| {
        world
            .power_consumers
            .get(entity)
            .is_some_and(|consumer| consumer.powered != *powered || consumer.source != *source)
    });
    if power_state_changed {
        world.invalidate_jobs_registry();
    }
}

pub(crate) fn is_powered_road(world: &World, x: usize, y: usize) -> bool {
    let Some(entity) = world.grid.get(x, y) else {
        return false;
    };
    if !road_connectivity::is_road_entity(world, entity) {
        return false;
    }
    road_connectivity::discover_road_networks(world)
        .into_iter()
        .any(|network| {
            network_capacity(world, &network.roads) > 0 && network.roads.contains(&entity)
        })
}

pub(crate) fn is_power_provider_connected(world: &World, entity: Entity) -> bool {
    road_connectivity::adjacent_road_entities(world, entity)
        .next()
        .is_some()
}

fn network_capacity(world: &World, roads: &HashSet<Entity>) -> i32 {
    world
        .power_providers
        .iter()
        .filter(|(entity, _provider)| {
            road_connectivity::adjacent_road_entities(world, **entity)
                .any(|road| roads.contains(&road))
        })
        .map(|(_entity, provider)| provider.capacity)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::components::PowerSource;
    use crate::core::regions::RegionId;
    use crate::core::systems::placement;
    use crate::interface::input::BuildingKind;

    #[test]
    fn powered_consumers_record_local_same_network_source() {
        let mut world = World::new(6, 3);
        placement::place_building(&mut world, 0, 0, BuildingKind::PowerPlant);
        placement::place_building(&mut world, 0, 1, BuildingKind::Road);
        placement::place_building(&mut world, 1, 1, BuildingKind::Road);
        placement::place_building(&mut world, 1, 0, BuildingKind::Residential);

        placement::place_building(&mut world, 5, 0, BuildingKind::PowerPlant);
        placement::place_building(&mut world, 5, 1, BuildingKind::Road);

        run(&mut world);

        let local_provider = world.grid.get(0, 0).expect("local provider");
        let remote_provider = world.grid.get(5, 0).expect("remote provider");
        let consumer = world.grid.get(1, 0).expect("consumer");
        let power = world
            .power_consumers
            .get(&consumer)
            .expect("power consumer");

        assert!(power.powered);
        assert_eq!(power.source, Some(PowerSource::Local(local_provider)));
        assert_ne!(power.source, Some(PowerSource::Local(remote_provider)));
    }

    #[test]
    fn unpowered_consumers_have_no_power_source_after_shortage() {
        let mut world = World::new(7, 4);
        placement::place_building(&mut world, 0, 0, BuildingKind::PowerPlant);
        for x in 0..6 {
            placement::place_building(&mut world, x, 1, BuildingKind::Road);
        }
        for x in 1..5 {
            placement::place_building(&mut world, x, 0, BuildingKind::Industrial);
        }
        placement::place_building(&mut world, 5, 0, BuildingKind::Commercial);

        run(&mut world);

        for x in 1..4 {
            let entity = world.grid.get(x, 0).expect("powered consumer");
            let consumer = world.power_consumers.get(&entity).expect("power consumer");
            assert!(consumer.powered);
            assert!(consumer.source.is_some());
        }
        for x in 4..=5 {
            let entity = world.grid.get(x, 0).expect("unpowered consumer");
            let consumer = world.power_consumers.get(&entity).expect("power consumer");
            assert!(!consumer.powered);
            assert_eq!(consumer.source, None);
        }
    }

    #[test]
    fn multiple_providers_on_one_network_keep_pooled_capacity() {
        let mut world = World::new(8, 3);
        placement::place_building(&mut world, 0, 0, BuildingKind::PowerPlant);
        placement::place_building(&mut world, 5, 0, BuildingKind::PowerPlant);
        for x in 0..=5 {
            placement::place_building(&mut world, x, 1, BuildingKind::Road);
        }
        placement::place_building(&mut world, 1, 0, BuildingKind::Industrial);
        placement::place_building(&mut world, 2, 0, BuildingKind::Industrial);
        placement::place_building(&mut world, 3, 0, BuildingKind::Industrial);

        *consumer_demand_mut(&mut world, 1, 0) = 8;
        *consumer_demand_mut(&mut world, 2, 0) = 8;
        *consumer_demand_mut(&mut world, 3, 0) = 3;

        run(&mut world);

        assert_eq!(world.stats.power.total_power_capacity, 20);
        assert_eq!(world.stats.power.total_power_demand, 19);
        assert_eq!(world.stats.power.total_power_supplied, 19);
        for x in 1..=3 {
            let entity = world.grid.get(x, 0).expect("consumer");
            let consumer = world.power_consumers.get(&entity).expect("power consumer");
            assert!(
                consumer.powered,
                "consumer at x={x} should use remaining pooled network capacity"
            );
            assert!(consumer.source.is_some());
        }
    }

    #[test]
    fn diff_apply_keeps_imported_source_when_no_local_grant_exists() {
        // Event-driven plan, P-3: a consumer with an existing `Imported`
        // source and no local grant available (no power plant anywhere in
        // this world) must survive `power::run` untouched — the whole point
        // of diff-apply is that imports no longer get wiped by a raw local
        // recompute.
        let mut world = World::new(3, 2);
        placement::place_building(&mut world, 0, 0, BuildingKind::Residential);
        placement::place_building(&mut world, 0, 1, BuildingKind::Road);
        let consumer_entity = world.grid.get(0, 0).expect("residential");
        let consumer = world
            .power_consumers
            .get_mut(&consumer_entity)
            .expect("power consumer");
        consumer.powered = true;
        consumer.source = Some(PowerSource::Imported {
            source_region: RegionId(2),
        });

        run(&mut world);

        let consumer = world
            .power_consumers
            .get(&consumer_entity)
            .expect("power consumer");
        assert!(consumer.powered, "kept import must stay powered");
        assert_eq!(
            consumer.source,
            Some(PowerSource::Imported {
                source_region: RegionId(2)
            }),
            "diff-apply must not clear an import with no local grant to replace it"
        );
        assert_eq!(
            world.stats.power.total_power_capacity, 0,
            "capacity reflects only local resolution; no plant exists"
        );
        assert_eq!(
            world.stats.power.total_power_supplied, world.stats.power.total_power_demand,
            "a kept import's demand must count as supplied, not vanish from stats \
             (resolution alone has no visibility into cross-region imports)"
        );
        assert_eq!(
            world.stats.power.total_power_shortage, 0,
            "a fully-covered import must not read as a shortage"
        );
    }

    #[test]
    fn diff_apply_overwrites_imported_source_with_a_fresh_local_grant() {
        // Event-driven plan, P-3: a consumer marked `Imported` that GAINS
        // local coverage (e.g. a plant was just connected) must transition to
        // `Local`, not get stuck showing the stale import.
        let mut world = World::new(3, 2);
        placement::place_building(&mut world, 0, 0, BuildingKind::Residential);
        placement::place_building(&mut world, 0, 1, BuildingKind::Road);
        let consumer_entity = world.grid.get(0, 0).expect("residential");
        let consumer = world
            .power_consumers
            .get_mut(&consumer_entity)
            .expect("power consumer");
        consumer.powered = true;
        consumer.source = Some(PowerSource::Imported {
            source_region: RegionId(2),
        });
        placement::place_building(&mut world, 1, 1, BuildingKind::PowerPlant);

        run(&mut world);

        let consumer = world
            .power_consumers
            .get(&consumer_entity)
            .expect("power consumer");
        assert!(consumer.powered);
        assert!(
            matches!(consumer.source, Some(PowerSource::Local(_))),
            "a fresh local grant must overwrite a stale imported source, got {:?}",
            consumer.source
        );
    }

    fn consumer_demand_mut(world: &mut World, x: usize, y: usize) -> &mut i32 {
        let entity = world.grid.get(x, y).expect("consumer cell");
        &mut world
            .power_consumers
            .get_mut(&entity)
            .expect("power consumer")
            .demand
    }
}
