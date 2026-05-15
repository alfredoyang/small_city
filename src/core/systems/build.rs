use crate::core::systems::placement;
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

    placement::place_building(world, x, y, kind);

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
