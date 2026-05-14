use crate::core::world::World;
use crate::interface::input::BuildingKind;
use crate::interface::view::{
    BuildOptionView, CellView, CityStatusView, GameView, InspectDetailsView, InspectView, MapView,
};

/// Converts the private ECS World into the only render model the UI may consume.
pub(crate) fn view_world(world: &World) -> GameView {
    let mut cells = Vec::with_capacity(world.grid.width() * world.grid.height());
    for y in 0..world.grid.height() {
        for x in 0..world.grid.width() {
            cells.push(cell_view(world, x, y));
        }
    }

    GameView {
        map: MapView {
            width: world.grid.width(),
            height: world.grid.height(),
            cells,
        },
        status: CityStatusView {
            money: world.resources.money,
            turn: world.resources.turn,
            population: world.stats.population,
            jobs: world.stats.jobs,
            unemployment: world.stats.unemployment,
            pollution: world.stats.pollution,
            happiness: world.stats.happiness,
        },
        build_options: [
            BuildingKind::Road,
            BuildingKind::Residential,
            BuildingKind::Commercial,
            BuildingKind::Industrial,
            BuildingKind::PowerPlant,
            BuildingKind::Park,
        ]
        .into_iter()
        .map(|kind| BuildOptionView {
            kind,
            label: kind.label().to_string(),
            cost: kind.cost(),
        })
        .collect(),
    }
}

/// Converts a map coordinate lookup into a UI-safe inspection result.
pub(crate) fn inspect_world(world: &World, x: usize, y: usize) -> InspectView {
    let in_bounds = world.grid.contains(x, y);
    InspectView {
        x,
        y,
        in_bounds,
        cell: in_bounds.then(|| cell_view(world, x, y)),
        details: in_bounds.then(|| inspect_details(world, x, y)),
    }
}

/// Builds type-specific inspect data while keeping ECS details inside the adapter.
fn inspect_details(world: &World, x: usize, y: usize) -> InspectDetailsView {
    let Some(entity) = world.grid.get(x, y) else {
        return InspectDetailsView::Empty { buildable: true };
    };

    let Some(building) = world.buildings.get(&entity) else {
        return InspectDetailsView::Unknown;
    };

    match building.kind {
        BuildingKind::Road => InspectDetailsView::Road,
        BuildingKind::Residential => {
            let population = world.populations.get(&entity);
            InspectDetailsView::Residential {
                powered: world
                    .power_consumers
                    .get(&entity)
                    .map(|consumer| consumer.powered)
                    .unwrap_or(false),
                population: population.map(|population| population.current).unwrap_or(0),
                max_population: population.map(|population| population.max).unwrap_or(0),
            }
        }
        BuildingKind::Commercial => InspectDetailsView::Commercial {
            powered: world
                .power_consumers
                .get(&entity)
                .map(|consumer| consumer.powered)
                .unwrap_or(false),
            jobs: building.kind.jobs(),
        },
        BuildingKind::Industrial => InspectDetailsView::Industrial {
            powered: world
                .power_consumers
                .get(&entity)
                .map(|consumer| consumer.powered)
                .unwrap_or(false),
            jobs: building.kind.jobs(),
        },
        BuildingKind::PowerPlant => InspectDetailsView::PowerPlant {
            power_radius: world
                .power_providers
                .get(&entity)
                .map(|provider| provider.radius)
                .unwrap_or(0),
        },
        BuildingKind::Park => InspectDetailsView::Park {
            happiness_effect: world
                .happiness_effects
                .get(&entity)
                .map(|effect| effect.amount)
                .unwrap_or(0),
        },
    }
}

/// Builds a cell view from ECS storage while keeping all World access inside the adapter.
fn cell_view(world: &World, x: usize, y: usize) -> CellView {
    let Some(entity) = world.grid.get(x, y) else {
        return CellView {
            x,
            y,
            symbol: '.',
            building: None,
            label: "Empty".to_string(),
            buildable: true,
            population: None,
            max_population: None,
            powered: None,
        };
    };

    let building = world.buildings.get(&entity).map(|building| building.kind);
    let population = world.populations.get(&entity);
    let powered = world
        .power_consumers
        .get(&entity)
        .map(|consumer| consumer.powered);

    CellView {
        x,
        y,
        symbol: building.map_or('?', BuildingKind::symbol),
        building,
        label: building.map_or("Unknown", BuildingKind::label).to_string(),
        buildable: false,
        population: population.map(|population| population.current),
        max_population: population.map(|population| population.max),
        powered,
    }
}
