use crate::core::components::{
    Building, HappinessEffect, PollutionSource, Population, Position, PowerConsumer, PowerProvider,
};
use crate::core::systems::entity_cleanup;
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
    place_replacement(world, x, y, kind);
    CommandResult::success(GameEventView::BuildingReplaced { x, y, kind })
}

fn place_replacement(world: &mut World, x: usize, y: usize, kind: BuildingKind) {
    let entity = world.spawn();
    world.resources.money -= kind.cost();
    world.grid.set(x, y, entity);
    world.positions.insert(entity, Position { x, y });
    world.buildings.insert(entity, Building { kind, level: 1 });

    match kind {
        BuildingKind::Residential => {
            world
                .populations
                .insert(entity, Population { current: 0, max: 5 });
            world.power_consumers.insert(
                entity,
                PowerConsumer {
                    powered: false,
                    demand: 1,
                },
            );
        }
        BuildingKind::Commercial => {
            world.power_consumers.insert(
                entity,
                PowerConsumer {
                    powered: false,
                    demand: 2,
                },
            );
        }
        BuildingKind::Industrial => {
            world.power_consumers.insert(
                entity,
                PowerConsumer {
                    powered: false,
                    demand: 3,
                },
            );
            world
                .pollution_sources
                .insert(entity, PollutionSource { amount: 2 });
        }
        BuildingKind::PowerPlant => {
            world
                .power_providers
                .insert(entity, PowerProvider { capacity: 10 });
        }
        BuildingKind::Park => {
            world
                .happiness_effects
                .insert(entity, HappinessEffect { amount: 3 });
        }
        BuildingKind::Road => {}
    }
}
