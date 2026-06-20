//! Bulldoze command handling for removing occupied cells and charging demolition cost.

use crate::core::systems::entity_cleanup;
use crate::core::world::World;
use crate::interface::events::{CommandResult, GameEventView};

const BULLDOZE_COST: i32 = 1;

pub(crate) fn bulldoze(world: &mut World, x: usize, y: usize) -> CommandResult {
    if !world.grid.contains(x, y) {
        return CommandResult::failure(GameEventView::BulldozeFailed {
            reason: "Cannot bulldoze outside the map".to_string(),
        });
    }

    let Some(entity) = world.grid.get(x, y) else {
        return CommandResult::failure(GameEventView::BulldozeFailed {
            reason: "Cell is already empty".to_string(),
        });
    };

    if world.resources.money < BULLDOZE_COST {
        return CommandResult::failure(GameEventView::BulldozeFailed {
            reason: "Not enough money to bulldoze".to_string(),
        });
    }

    world.resources.money -= BULLDOZE_COST;
    entity_cleanup::remove_entity(world, entity, x, y);

    CommandResult::success(GameEventView::BuildingBulldozed { x, y })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::components::{Building, BuildingData, Footprint, Position};
    use crate::interface::input::BuildingKind;

    #[test]
    fn bulldozing_a_body_cell_clears_the_whole_footprint() {
        let mut world = World::new(4, 4);
        world.resources.money = 100;

        // Place a 2x1 commercial building by hand: anchor (1,1), occupying (1,1) and (2,1).
        let entity = world.spawn();
        world.attach_position(entity, Position { x: 1, y: 1 });
        world.attach_building(
            entity,
            Building {
                kind: BuildingKind::Commercial,
                level: 1,
                data: BuildingData::None,
                footprint: Footprint {
                    width: 2,
                    height: 1,
                },
            },
        );
        world.grid.set(1, 1, entity);
        world.grid.set(2, 1, entity);

        // Every footprint cell resolves to the same owner (inspect/cell->owner).
        assert_eq!(world.grid.get(1, 1), Some(entity));
        assert_eq!(world.grid.get(2, 1), Some(entity));

        // Bulldozing the body cell removes the whole building, not just that one cell.
        assert!(bulldoze(&mut world, 2, 1).success);
        assert_eq!(world.grid.get(1, 1), None);
        assert_eq!(world.grid.get(2, 1), None);
        assert!(!world.buildings.contains_key(&entity));
    }

    #[test]
    fn bulldozing_a_corrupt_zero_footprint_still_clears_the_anchor() {
        let mut world = World::new(4, 4);
        world.resources.money = 100;
        let entity = world.spawn();
        world.attach_position(entity, Position { x: 1, y: 1 });
        world.attach_building(
            entity,
            Building {
                kind: BuildingKind::Commercial,
                level: 1,
                data: BuildingData::None,
                footprint: Footprint {
                    width: 0,
                    height: 0,
                },
            },
        );
        world.grid.set(1, 1, entity);

        assert!(bulldoze(&mut world, 1, 1).success);
        assert_eq!(world.grid.get(1, 1), None, "anchor must not be left stale");
    }
}
