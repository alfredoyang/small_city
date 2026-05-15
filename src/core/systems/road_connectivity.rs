use crate::core::entity::Entity;
use crate::core::world::World;
use crate::interface::input::BuildingKind;

pub(crate) fn is_road_connected(world: &World, entity: Entity) -> bool {
    let Some(position) = world.positions.get(&entity) else {
        return false;
    };

    [
        position.x.checked_sub(1).map(|x| (x, position.y)),
        Some((position.x.saturating_add(1), position.y)),
        position.y.checked_sub(1).map(|y| (position.x, y)),
        Some((position.x, position.y.saturating_add(1))),
    ]
    .into_iter()
    .flatten()
    .any(|(x, y)| is_road_at(world, x, y))
}

fn is_road_at(world: &World, x: usize, y: usize) -> bool {
    world
        .grid
        .get(x, y)
        .and_then(|entity| world.buildings.get(&entity))
        .is_some_and(|building| building.kind == BuildingKind::Road)
}
