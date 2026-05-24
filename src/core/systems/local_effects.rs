//! Local effects system deriving land value, pollution pressure, accessibility, and desirability.

use std::sync::Arc;

use crate::core::region::{RegionBounds, RegionPartition};
use crate::core::region_actor::{
    ActorRuntime, LocalEffectsCell, PhaseRun, RegionId, RegionMessageKind, SimPhase, SimTick,
};
use crate::core::resources::{LocalEffects, LocalEffectsMap};
use crate::core::world::World;
use crate::interface::input::BuildingKind;

const ACTOR_REGION_WIDTH: usize = 5;
const ACTOR_REGION_HEIGHT: usize = 5;
const LOCAL_EFFECTS_PHASE: SimPhase = SimPhase(1);
const PARK_RADIUS: usize = 2;
const INDUSTRIAL_RADIUS: usize = 2;
const COMMERCIAL_RADIUS: usize = 1;
const CITIZEN_RADIUS: usize = 1;

pub(crate) fn run(world: &mut World) {
    world.local_effects = derive_local_effects_with_region_actors(world);
}

fn derive_local_effects_with_region_actors(world: &World) -> LocalEffectsMap {
    let snapshot = LocalEffectsSnapshot::from_world(world);
    let region_order = region_partition_for_snapshot(&snapshot)
        .region_ids()
        .collect::<Vec<_>>();
    derive_local_effects_with_region_order(
        snapshot.clone(),
        region_order,
        SimTick(world.resources.turn as u64 + 1),
    )
    .unwrap_or_else(|| derive_local_effects_direct_from_snapshot(&snapshot))
}

fn derive_local_effects_with_region_order(
    snapshot: LocalEffectsSnapshot,
    region_order: Vec<RegionId>,
    tick: SimTick,
) -> Option<LocalEffectsMap> {
    let partition = region_partition_for_snapshot(&snapshot);
    let mut runtime = ActorRuntime::new_threaded(partition.region_ids());
    let snapshot = Arc::new(snapshot);

    for region in region_order {
        let bounds = partition.bounds(region)?;
        runtime.send(
            tick,
            LOCAL_EFFECTS_PHASE,
            region,
            region,
            RegionMessageKind::LocalEffectsRegionWork(LocalEffectsRegionWork {
                bounds,
                snapshot: Arc::clone(&snapshot),
            }),
        );
    }

    let results = runtime.run_phase(tick, LOCAL_EFFECTS_PHASE);
    if !results
        .values()
        .all(|result| *result == PhaseRun::Completed)
    {
        return None;
    }

    let mut map = LocalEffectsMap::new(snapshot.width, snapshot.height);
    for region in partition.region_ids() {
        let Some(actor) = runtime.actor(region) else {
            continue;
        };
        for cell in &actor.state.read_only.local_effect_cells {
            if cell.x < snapshot.width && cell.y < snapshot.height {
                map.cells[cell.y * snapshot.width + cell.x] = cell.effects;
            }
        }
    }
    Some(map)
}

fn derive_local_effects_direct_from_snapshot(snapshot: &LocalEffectsSnapshot) -> LocalEffectsMap {
    let mut map = LocalEffectsMap::new(snapshot.width, snapshot.height);
    for y in 0..snapshot.height {
        for x in 0..snapshot.width {
            map.cells[y * snapshot.width + x] = derive_cell_effects(snapshot, x, y);
        }
    }
    map
}

pub(crate) fn derive_region_local_effect_cells(
    snapshot: &LocalEffectsSnapshot,
    bounds: RegionBounds,
) -> Vec<LocalEffectsCell> {
    let mut cells = Vec::new();
    for y in bounds.min_y..bounds.max_y.min(snapshot.height) {
        for x in bounds.min_x..bounds.max_x.min(snapshot.width) {
            cells.push(LocalEffectsCell {
                x,
                y,
                effects: derive_cell_effects(snapshot, x, y),
            });
        }
    }
    cells
}

fn derive_cell_effects(snapshot: &LocalEffectsSnapshot, x: usize, y: usize) -> LocalEffects {
    let mut effects = LocalEffects {
        accessibility: adjacent_road_count(snapshot, x, y) as i32 * 3,
        ..LocalEffects::default()
    };

    for building in &snapshot.buildings {
        let distance = manhattan_distance(x, y, building.x, building.y);

        match building.kind {
            BuildingKind::Park if distance <= PARK_RADIUS => {
                let boost = (PARK_RADIUS + 1 - distance) as i32;
                effects.land_value += boost * 2;
            }
            BuildingKind::Industrial if distance <= INDUSTRIAL_RADIUS => {
                let pressure = (INDUSTRIAL_RADIUS + 1 - distance) as i32;
                effects.pollution_pressure += pressure * 2;
                effects.land_value -= pressure;
            }
            BuildingKind::Commercial if distance <= COMMERCIAL_RADIUS => {
                effects.land_value += 1;
            }
            _ => {}
        }
    }

    for citizen in &snapshot.citizens {
        let distance = manhattan_distance(x, y, citizen.home_x, citizen.home_y);
        if distance > CITIZEN_RADIUS {
            continue;
        }

        if citizen.happiness >= 60 {
            effects.land_value += 1;
        } else if citizen.happiness < 40 {
            effects.pollution_pressure += 1;
            effects.land_value -= 1;
        }
    }

    effects.land_value = effects.land_value.clamp(0, 9);
    effects.pollution_pressure = effects.pollution_pressure.clamp(0, 9);
    effects.accessibility = effects.accessibility.clamp(0, 9);
    effects.desirability =
        (effects.land_value + effects.accessibility - effects.pollution_pressure).clamp(0, 9);
    effects
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEffectsRegionWork {
    pub(crate) bounds: RegionBounds,
    pub(crate) snapshot: Arc<LocalEffectsSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEffectsSnapshot {
    width: usize,
    height: usize,
    roads: Vec<bool>,
    buildings: Vec<BuildingEffectSample>,
    citizens: Vec<CitizenEffectSample>,
}

impl LocalEffectsSnapshot {
    fn from_world(world: &World) -> Self {
        let width = world.grid.width();
        let height = world.grid.height();
        let mut roads = vec![false; width * height];
        let mut buildings = Vec::new();
        let mut citizens = Vec::new();

        for y in 0..height {
            for x in 0..width {
                roads[y * width + x] = world
                    .grid
                    .get(x, y)
                    .and_then(|entity| world.buildings.get(&entity))
                    .is_some_and(|building| building.kind == BuildingKind::Road);
            }
        }

        for (entity, building) in &world.buildings {
            let Some(position) = world.positions.get(entity) else {
                continue;
            };
            buildings.push(BuildingEffectSample {
                kind: building.kind,
                x: position.x,
                y: position.y,
            });
        }

        for citizen in world.citizens.values() {
            let Some(position) = world.positions.get(&citizen.home) else {
                continue;
            };
            citizens.push(CitizenEffectSample {
                home_x: position.x,
                home_y: position.y,
                happiness: citizen.happiness,
            });
        }

        buildings.sort_by_key(|sample| (sample.y, sample.x, building_kind_order(sample.kind)));
        citizens.sort_by_key(|sample| (sample.home_y, sample.home_x, sample.happiness));

        Self {
            width,
            height,
            roads,
            buildings,
            citizens,
        }
    }

    fn has_road(&self, x: usize, y: usize) -> bool {
        x < self.width && y < self.height && self.roads[y * self.width + x]
    }
}

pub(crate) fn desirability_level(world: &World, x: usize, y: usize) -> DesirabilityLevel {
    match world.local_effects.get(x, y).desirability {
        0..=3 => DesirabilityLevel::Low,
        4..=8 => DesirabilityLevel::Medium,
        _ => DesirabilityLevel::High,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DesirabilityLevel {
    Low,
    Medium,
    High,
}

fn adjacent_road_count(snapshot: &LocalEffectsSnapshot, x: usize, y: usize) -> usize {
    adjacent_coordinates(x, y)
        .into_iter()
        .flatten()
        .filter(|(x, y)| snapshot.has_road(*x, *y))
        .count()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BuildingEffectSample {
    kind: BuildingKind,
    x: usize,
    y: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CitizenEffectSample {
    home_x: usize,
    home_y: usize,
    happiness: i32,
}

fn region_partition_for_snapshot(snapshot: &LocalEffectsSnapshot) -> RegionPartition {
    RegionPartition::new(
        snapshot.width,
        snapshot.height,
        ACTOR_REGION_WIDTH,
        ACTOR_REGION_HEIGHT,
    )
}

fn building_kind_order(kind: BuildingKind) -> u8 {
    match kind {
        BuildingKind::Road => 0,
        BuildingKind::Residential => 1,
        BuildingKind::Commercial => 2,
        BuildingKind::Industrial => 3,
        BuildingKind::PowerPlant => 4,
        BuildingKind::Park => 5,
    }
}

fn adjacent_coordinates(x: usize, y: usize) -> [Option<(usize, usize)>; 4] {
    [
        x.checked_sub(1).map(|left| (left, y)),
        Some((x.saturating_add(1), y)),
        y.checked_sub(1).map(|up| (x, up)),
        Some((x, y.saturating_add(1))),
    ]
}

fn manhattan_distance(ax: usize, ay: usize, bx: usize, by: usize) -> usize {
    ax.abs_diff(bx) + ay.abs_diff(by)
}

#[cfg(test)]
mod tests {
    use super::{
        DesirabilityLevel, LocalEffectsSnapshot, derive_local_effects_direct_from_snapshot,
        derive_local_effects_with_region_order, desirability_level, region_partition_for_snapshot,
        run,
    };
    use crate::core::components::{Building, BuildingData, Citizen, Position};
    use crate::core::region_actor::SimTick;
    use crate::core::world::World;
    use crate::interface::input::BuildingKind;

    #[test]
    fn parks_raise_nearby_land_value_and_desirability() {
        let mut world = World::new(5, 5);
        let park = world.spawn();
        world.attach_position(park, Position { x: 2, y: 2 });
        world.attach_building(
            park,
            Building {
                kind: BuildingKind::Park,
                level: 1,
                data: BuildingData::None,
            },
        );

        run(&mut world);

        let nearby = world.local_effects.get(2, 2);
        assert!(nearby.land_value > 4);
        assert_eq!(desirability_level(&world, 2, 2), DesirabilityLevel::High);
    }

    #[test]
    fn industrial_raises_pollution_pressure_and_lowers_land_value() {
        let mut world = World::new(5, 5);
        let industrial = world.spawn();
        world.attach_position(industrial, Position { x: 2, y: 2 });
        world.attach_building(
            industrial,
            Building {
                kind: BuildingKind::Industrial,
                level: 1,
                data: BuildingData::None,
            },
        );

        run(&mut world);

        let nearby = world.local_effects.get(2, 1);
        assert!(nearby.pollution_pressure > 0);
        assert!(nearby.land_value < 4);
        assert_eq!(desirability_level(&world, 2, 1), DesirabilityLevel::Low);
    }

    #[test]
    fn actor_local_effects_match_direct_local_effects_result() {
        let mut world = World::new(7, 5);
        attach_building(&mut world, 1, 1, BuildingKind::Industrial);
        attach_building(&mut world, 4, 2, BuildingKind::Park);
        attach_building(&mut world, 5, 3, BuildingKind::Commercial);
        attach_building(&mut world, 2, 2, BuildingKind::Road);

        let snapshot = LocalEffectsSnapshot::from_world(&world);
        let expected = derive_local_effects_direct_from_snapshot(&snapshot);
        run(&mut world);

        assert_eq!(world.local_effects, expected);
    }

    #[test]
    fn actor_local_effects_apply_across_region_boundaries() {
        let mut world = World::new(10, 5);
        // Actor regions are 5 cells wide, so x=4 is in region A and x=5 starts region B.
        attach_building(&mut world, 4, 2, BuildingKind::Park);
        let home = attach_building(&mut world, 4, 4, BuildingKind::Residential);
        attach_citizen(&mut world, home, 70);

        let snapshot = LocalEffectsSnapshot::from_world(&world);
        let expected = derive_local_effects_direct_from_snapshot(&snapshot);
        run(&mut world);

        assert_eq!(world.local_effects, expected);
        assert_eq!(world.local_effects.get(5, 2).land_value, 8);
        assert_eq!(world.local_effects.get(5, 2).desirability, 8);
        assert_eq!(world.local_effects.get(6, 2).land_value, 6);
        assert_eq!(world.local_effects.get(6, 2).desirability, 6);
        assert_eq!(world.local_effects.get(5, 4).land_value, 5);
        assert_eq!(world.local_effects.get(5, 4).desirability, 5);
    }

    #[test]
    fn actor_local_effects_are_stable_with_different_region_order() {
        let mut world = World::new(11, 6);
        attach_building(&mut world, 4, 2, BuildingKind::Park);
        attach_building(&mut world, 5, 2, BuildingKind::Industrial);
        attach_building(&mut world, 7, 3, BuildingKind::Commercial);
        let home = attach_building(&mut world, 4, 4, BuildingKind::Residential);
        attach_citizen(&mut world, home, 30);

        let snapshot = LocalEffectsSnapshot::from_world(&world);
        let forward_order = region_partition_for_snapshot(&snapshot)
            .region_ids()
            .collect::<Vec<_>>();
        let mut reverse_order = forward_order.clone();
        reverse_order.reverse();

        let forward =
            derive_local_effects_with_region_order(snapshot.clone(), forward_order, SimTick(1))
                .expect("forward actor local effects");
        let reverse = derive_local_effects_with_region_order(snapshot, reverse_order, SimTick(1))
            .expect("reverse actor local effects");

        assert_eq!(forward, reverse);
    }

    #[test]
    fn actor_local_effects_handle_empty_and_small_maps() {
        let mut empty = World::new(0, 0);
        run(&mut empty);
        assert_eq!(empty.local_effects.width, 0);
        assert_eq!(empty.local_effects.height, 0);
        assert!(empty.local_effects.cells.is_empty());

        let mut single_cell = World::new(1, 1);
        attach_building(&mut single_cell, 0, 0, BuildingKind::Park);
        let snapshot = LocalEffectsSnapshot::from_world(&single_cell);
        let expected = derive_local_effects_direct_from_snapshot(&snapshot);
        run(&mut single_cell);

        assert_eq!(single_cell.local_effects, expected);
    }

    fn attach_building(
        world: &mut World,
        x: usize,
        y: usize,
        kind: BuildingKind,
    ) -> crate::core::entity::Entity {
        let entity = world.spawn();
        world.attach_position(entity, Position { x, y });
        world.attach_building(
            entity,
            Building {
                kind,
                level: 1,
                data: BuildingData::None,
            },
        );
        entity
    }

    fn attach_citizen(world: &mut World, home: crate::core::entity::Entity, happiness: i32) {
        let citizen = world.spawn();
        world.attach_citizen(
            citizen,
            Citizen {
                age: 0,
                home,
                workplace: None,
                happiness,
                happiness_decay: 0,
                money: 0,
                rent_stress: 0,
            },
        );
    }
}
