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
    world.attach_position(entity, Position { x, y });
    world.attach_building(entity, Building { kind, level: 1 });

    match kind {
        BuildingKind::Residential => {
            world.attach_population(entity, Population { current: 0, max: 5 });
            world.attach_power_consumer(
                entity,
                PowerConsumer {
                    powered: false,
                    demand: 1,
                },
            );
        }
        BuildingKind::Commercial => {
            world.attach_power_consumer(
                entity,
                PowerConsumer {
                    powered: false,
                    demand: 2,
                },
            );
        }
        BuildingKind::Industrial => {
            world.attach_power_consumer(
                entity,
                PowerConsumer {
                    powered: false,
                    demand: 3,
                },
            );
            world.attach_pollution_source(entity, PollutionSource { amount: 2 });
        }
        BuildingKind::PowerPlant => {
            world.attach_power_provider(entity, PowerProvider { capacity: 10 });
        }
        BuildingKind::Park => {
            world.attach_happiness_effect(entity, HappinessEffect { amount: 3 });
        }
        BuildingKind::Road => {}
    }
}
