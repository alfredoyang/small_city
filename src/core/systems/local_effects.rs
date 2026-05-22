//! Local effects system deriving land value, pollution pressure, accessibility, and desirability.

use crate::core::region::{GridPos, RegionPartition};
use crate::core::region_actor::{
    ActorRuntime, LocalEffectsCell, PhaseRun, RegionMessageKind, SimPhase, SimTick,
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
    let width = world.grid.width();
    let height = world.grid.height();
    let partition = RegionPartition::new(width, height, ACTOR_REGION_WIDTH, ACTOR_REGION_HEIGHT);
    let mut runtime = ActorRuntime::new_threaded(partition.region_ids());
    let tick = SimTick(world.resources.turn as u64 + 1);

    for y in 0..height {
        for x in 0..width {
            let Some(region) = partition.region_for_cell(GridPos { x, y }) else {
                continue;
            };
            runtime.send(
                tick,
                LOCAL_EFFECTS_PHASE,
                region,
                region,
                RegionMessageKind::LocalEffectsCellSample(LocalEffectsCell {
                    x,
                    y,
                    effects: derive_cell_effects(world, x, y),
                }),
            );
        }
    }

    let results = runtime.run_phase(tick, LOCAL_EFFECTS_PHASE);
    if !results
        .values()
        .all(|result| *result == PhaseRun::Completed)
    {
        return derive_local_effects_direct(world);
    }

    let mut map = LocalEffectsMap::new(width, height);
    for region in partition.region_ids() {
        let Some(actor) = runtime.actor(region) else {
            continue;
        };
        for cell in &actor.state.read_only.local_effect_cells {
            if cell.x < width && cell.y < height {
                map.cells[cell.y * width + cell.x] = cell.effects;
            }
        }
    }
    map
}

fn derive_local_effects_direct(world: &World) -> LocalEffectsMap {
    let width = world.grid.width();
    let height = world.grid.height();
    let mut map = LocalEffectsMap::new(width, height);
    for y in 0..height {
        for x in 0..width {
            map.cells[y * width + x] = derive_cell_effects(world, x, y);
        }
    }
    map
}

fn derive_cell_effects(world: &World, x: usize, y: usize) -> LocalEffects {
    let mut effects = LocalEffects {
        accessibility: adjacent_road_count(world, x, y) as i32 * 3,
        ..LocalEffects::default()
    };

    for (entity, building) in &world.buildings {
        let Some(position) = world.positions.get(entity) else {
            continue;
        };
        let distance = manhattan_distance(x, y, position.x, position.y);

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

    for citizen in world.citizens.values() {
        let Some(position) = world.positions.get(&citizen.home) else {
            continue;
        };
        let distance = manhattan_distance(x, y, position.x, position.y);
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

fn adjacent_road_count(world: &World, x: usize, y: usize) -> usize {
    adjacent_coordinates(x, y)
        .into_iter()
        .flatten()
        .filter(|(x, y)| {
            world
                .grid
                .get(*x, *y)
                .and_then(|entity| world.buildings.get(&entity))
                .is_some_and(|building| building.kind == BuildingKind::Road)
        })
        .count()
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
    use super::{DesirabilityLevel, derive_local_effects_direct, desirability_level, run};
    use crate::core::components::{Building, BuildingData, Position};
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

        let expected = derive_local_effects_direct(&world);
        run(&mut world);

        assert_eq!(world.local_effects, expected);
    }

    fn attach_building(world: &mut World, x: usize, y: usize, kind: BuildingKind) {
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
    }
}
