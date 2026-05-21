//! Shared building placement logic used by build and replace command systems.

use crate::core::components::{
    Building, BuildingData, BusinessFinance, HappinessEffect, PollutionSource, Population,
    Position, PowerConsumer, PowerProvider,
};
use crate::core::world::World;
use crate::interface::input::BuildingKind;

pub(crate) fn place_building(world: &mut World, x: usize, y: usize, kind: BuildingKind) {
    let entity = world.spawn();
    world.resources.money -= kind.cost();
    world.grid.set(x, y, entity);
    world.attach_position(entity, Position { x, y });
    world.attach_building(
        entity,
        Building {
            kind,
            level: 1,
            data: building_data_for_kind(kind),
        },
    );

    attach_building_components(world, entity, kind);
}

fn attach_building_components(
    world: &mut World,
    entity: crate::core::entity::Entity,
    kind: BuildingKind,
) {
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

fn building_data_for_kind(kind: BuildingKind) -> BuildingData {
    match kind {
        BuildingKind::Commercial => BuildingData::Commercial {
            local_goods_stored: 0,
            business: BusinessFinance::default(),
        },
        BuildingKind::Industrial => BuildingData::Industrial {
            business: BusinessFinance::default(),
        },
        BuildingKind::Road
        | BuildingKind::Residential
        | BuildingKind::PowerPlant
        | BuildingKind::Park => BuildingData::None,
    }
}
