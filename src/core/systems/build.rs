use crate::core::components::{
    Building, HappinessEffect, PollutionSource, Population, Position, PowerConsumer, PowerProvider,
};
use crate::core::world::World;
use crate::interface::events::{CommandResult, GameEventView};
use crate::interface::input::BuildingKind;
use crate::interface::view::BuildPreviewView;

pub(crate) fn build(world: &mut World, x: usize, y: usize, kind: BuildingKind) -> CommandResult {
    if let Err(reason) = validate_build(world, x, y, kind) {
        return CommandResult::failure(GameEventView::BuildFailed {
            reason: reason.to_string(),
        });
    }

    let cost = kind.cost();
    let entity = world.spawn();
    world.resources.money -= cost;
    world.grid.set(x, y, entity);
    world.attach_position(entity, Position { x, y });
    world.attach_building(entity, Building { kind, level: 1 });

    // Attach only the components that make this building participate in later systems.
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
        }
        BuildingKind::PowerPlant => {
            world.attach_power_provider(entity, PowerProvider { capacity: 10 });
        }
        BuildingKind::Park => {
            world.attach_happiness_effect(entity, HappinessEffect { amount: 3 });
        }
        BuildingKind::Road => {}
    }

    // Industrial buildings affect pollution but do not need a separate behavior component.
    if kind == BuildingKind::Industrial {
        world.attach_pollution_source(entity, PollutionSource { amount: 2 });
    }

    CommandResult::success(GameEventView::Built { x, y, kind })
}

pub(crate) fn preview_build(
    world: &World,
    x: usize,
    y: usize,
    kind: BuildingKind,
) -> BuildPreviewView {
    match validate_build(world, x, y, kind) {
        Ok(()) => BuildPreviewView {
            kind,
            label: kind.label().to_string(),
            cost: kind.cost(),
            can_build: true,
            reason: None,
            effects: build_effects(kind),
        },
        Err(reason) => BuildPreviewView {
            kind,
            label: kind.label().to_string(),
            cost: kind.cost(),
            can_build: false,
            reason: Some(reason.to_string()),
            effects: build_effects(kind),
        },
    }
}

fn validate_build(
    world: &World,
    x: usize,
    y: usize,
    kind: BuildingKind,
) -> Result<(), &'static str> {
    if !world.grid.contains(x, y) {
        return Err("Cannot build outside the map");
    }

    if world.grid.get(x, y).is_some() {
        return Err("Cell is already occupied");
    }

    if world.resources.money < kind.cost() {
        return Err("Not enough money");
    }

    Ok(())
}

fn build_effects(kind: BuildingKind) -> Vec<String> {
    match kind {
        BuildingKind::Road => vec!["Connects adjacent buildings to the road network".to_string()],
        BuildingKind::Residential => vec![
            "Adds housing for up to 5 people".to_string(),
            "Needs power, road access, and available jobs to grow".to_string(),
        ],
        BuildingKind::Commercial => vec![
            "Provides 2 effective jobs when powered and road-connected".to_string(),
            "Earns income when powered and road-connected".to_string(),
            "Costs 1 maintenance each turn".to_string(),
        ],
        BuildingKind::Industrial => vec![
            "Provides 3 effective jobs when powered and road-connected".to_string(),
            "Earns income when powered and road-connected".to_string(),
            "Costs 1 maintenance each turn".to_string(),
            "Creates 2 pollution".to_string(),
        ],
        BuildingKind::PowerPlant => {
            vec![
                "Adds 10 power capacity to adjacent road network".to_string(),
                "Costs 1 maintenance each turn".to_string(),
            ]
        }
        BuildingKind::Park => vec![
            "Adds +3 happiness effect".to_string(),
            "Costs 1 maintenance each turn".to_string(),
        ],
    }
}
