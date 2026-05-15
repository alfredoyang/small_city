use crate::core::systems::{entity_cleanup, placement};
use crate::core::world::World;
use crate::interface::events::{CommandResult, GameEventView};
use crate::interface::input::BuildingKind;

pub(crate) fn replace(world: &mut World, x: usize, y: usize, kind: BuildingKind) -> CommandResult {
    if !world.grid.contains(x, y) {
        return CommandResult::failure(GameEventView::ReplaceFailed {
            reason: "Cannot replace outside the map".to_string(),
        });
    }

    let Some(existing_entity) = world.grid.get(x, y) else {
        return CommandResult::failure(GameEventView::ReplaceFailed {
            reason: "Cannot replace an empty cell".to_string(),
        });
    };

    if world
        .buildings
        .get(&existing_entity)
        .is_some_and(|building| building.kind == kind)
    {
        return CommandResult::failure(GameEventView::ReplaceFailed {
            reason: "Cell already has that building type".to_string(),
        });
    }

    if world.resources.money < kind.cost() {
        return CommandResult::failure(GameEventView::ReplaceFailed {
            reason: "Not enough money to replace".to_string(),
        });
    }

    entity_cleanup::remove_entity(world, existing_entity, x, y);
    placement::place_building(world, x, y, kind);
    CommandResult::success(GameEventView::BuildingReplaced { x, y, kind })
}
