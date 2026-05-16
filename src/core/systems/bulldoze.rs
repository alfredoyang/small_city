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
