use crate::core::components::{
    Building, HappinessEffect, PollutionSource, Population, Position, PowerConsumer, PowerProvider,
};
use crate::core::world::World;
use crate::interface::events::{CommandResult, GameEventView};
use crate::interface::input::BuildingKind;

pub(crate) fn build(world: &mut World, x: usize, y: usize, kind: BuildingKind) -> CommandResult {
    if !world.grid.contains(x, y) {
        return CommandResult::failure(GameEventView::BuildFailed {
            reason: "Cannot build outside the map".to_string(),
        });
    }

    if world.grid.get(x, y).is_some() {
        return CommandResult::failure(GameEventView::BuildFailed {
            reason: "Cell is already occupied".to_string(),
        });
    }

    let cost = kind.cost();
    if world.resources.money < cost {
        return CommandResult::failure(GameEventView::BuildFailed {
            reason: "Not enough money".to_string(),
        });
    }

    let entity = world.spawn();
    world.resources.money -= cost;
    world.grid.set(x, y, entity);
    world.positions.insert(entity, Position { x, y });
    world.buildings.insert(entity, Building { kind });

    match kind {
        BuildingKind::Residential => {
            world
                .populations
                .insert(entity, Population { current: 0, max: 5 });
            world
                .power_consumers
                .insert(entity, PowerConsumer { powered: false });
        }
        BuildingKind::Commercial | BuildingKind::Industrial => {
            world
                .power_consumers
                .insert(entity, PowerConsumer { powered: false });
        }
        BuildingKind::PowerPlant => {
            world
                .power_providers
                .insert(entity, PowerProvider { radius: 3 });
        }
        BuildingKind::Park => {
            world
                .happiness_effects
                .insert(entity, HappinessEffect { amount: 3 });
        }
        BuildingKind::Road => {}
    }

    if kind == BuildingKind::Industrial {
        world
            .pollution_sources
            .insert(entity, PollutionSource { amount: 2 });
    }

    CommandResult::success(GameEventView::Built { x, y, kind })
}
