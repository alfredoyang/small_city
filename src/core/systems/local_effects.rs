//! Local effects system deriving land value, pollution pressure, accessibility, and desirability.

use crate::core::resources::{LocalEffects, LocalEffectsMap};
use crate::core::world::World;
use crate::interface::input::BuildingKind;

const PARK_RADIUS: usize = 2;
const INDUSTRIAL_RADIUS: usize = 2;
const COMMERCIAL_RADIUS: usize = 1;
const CITIZEN_RADIUS: usize = 1;

pub(crate) fn run(world: &mut World) {
    let width = world.grid.width();
    let height = world.grid.height();
    let mut map = LocalEffectsMap::new(width, height);

    for y in 0..height {
        for x in 0..width {
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

                if citizen.morale.actual >= 60 {
                    effects.land_value += 1;
                } else if citizen.morale.actual < 40 {
                    effects.pollution_pressure += 1;
                    effects.land_value -= 1;
                }
            }

            effects.land_value = effects.land_value.clamp(0, 9);
            effects.pollution_pressure = effects.pollution_pressure.clamp(0, 9);
            effects.accessibility = effects.accessibility.clamp(0, 9);
            effects.desirability = (effects.land_value + effects.accessibility
                - effects.pollution_pressure)
                .clamp(0, 9);

            map.cells[y * width + x] = effects;
        }
    }

    world.local_effects = map;
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
    use super::{DesirabilityLevel, desirability_level, run};
    use crate::core::components::{Building, BuildingData, Footprint, Position};
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
                footprint: Footprint::single(),
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
                footprint: Footprint::single(),
            },
        );

        run(&mut world);

        let nearby = world.local_effects.get(2, 1);
        assert!(nearby.pollution_pressure > 0);
        assert!(nearby.land_value < 4);
        assert_eq!(desirability_level(&world, 2, 1), DesirabilityLevel::Low);
    }
}
