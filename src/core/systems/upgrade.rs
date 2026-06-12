//! Upgrade command handling for supported building types and their level-based effects.

use crate::core::world::World;
use crate::interface::events::{CommandResult, GameEventView};
use crate::interface::input::BuildingKind;

// v0.2 supports one upgrade step: base buildings start at level 1 and cap at level 2.
pub(crate) const MAX_UPGRADE_LEVEL: u8 = 2;

pub(crate) fn upgrade(world: &mut World, x: usize, y: usize) -> CommandResult {
    if !world.grid.contains(x, y) {
        return CommandResult::failure(GameEventView::UpgradeFailed {
            reason: "Cannot upgrade outside the map".to_string(),
        });
    }

    let Some(entity) = world.grid.get(x, y) else {
        return CommandResult::failure(GameEventView::UpgradeFailed {
            reason: "Cannot upgrade an empty cell".to_string(),
        });
    };

    let Some(building) = world.buildings.get(&entity).copied() else {
        return CommandResult::failure(GameEventView::UpgradeFailed {
            reason: "Cannot upgrade unknown building".to_string(),
        });
    };

    if building.level >= MAX_UPGRADE_LEVEL {
        return CommandResult::failure(GameEventView::UpgradeFailed {
            reason: "Building is already fully upgraded".to_string(),
        });
    }

    let next_level = building.level + 1;
    let Some(cost) = building.kind.upgrade_cost_for_level(next_level) else {
        return CommandResult::failure(GameEventView::UpgradeFailed {
            reason: format!("{} cannot be upgraded", building.kind.label()),
        });
    };

    if world.resources.money < cost {
        return CommandResult::failure(GameEventView::UpgradeFailed {
            reason: "Not enough money to upgrade".to_string(),
        });
    }

    world.resources.money -= cost;
    if let Some(building) = world.buildings.get_mut(&entity) {
        building.level = next_level;
    }
    apply_upgrade_effect(world, entity, building.kind);

    CommandResult::success(GameEventView::BuildingUpgraded {
        x,
        y,
        kind: building.kind,
        level: next_level,
    })
}

pub(crate) fn apply_upgrade_effect(
    world: &mut World,
    entity: crate::core::entity::Entity,
    kind: BuildingKind,
) {
    match kind {
        BuildingKind::Residential => {
            if let Some(population) = world.populations.get_mut(&entity) {
                population.max = 8;
            }
        }
        BuildingKind::PowerPlant => {
            if let Some(provider) = world.power_providers.get_mut(&entity) {
                provider.capacity = 15;
            }
        }
        BuildingKind::Park => {
            if let Some(effect) = world.happiness_effects.get_mut(&entity) {
                effect.amount = 5;
            }
        }
        BuildingKind::Industrial => {
            if let Some(source) = world.pollution_sources.get_mut(&entity) {
                source.amount = 3;
            }
        }
        BuildingKind::Road | BuildingKind::Commercial => {}
    }
    match kind {
        BuildingKind::PowerPlant => world.invalidate_resource_registry(),
        BuildingKind::Residential | BuildingKind::Commercial | BuildingKind::Industrial => {
            world.invalidate_jobs_registry();
        }
        BuildingKind::Road | BuildingKind::Park => {}
    }
}
