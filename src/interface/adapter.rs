use crate::core::world::World;
use crate::interface::input::BuildingKind;
use crate::interface::view::{
    BuildOptionView, CellView, CityStatusView, GameView, InspectView, MapView,
};

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

pub(crate) fn inspect_world(world: &World, x: usize, y: usize) -> InspectView {
    InspectView {
        x,
        y,
        in_bounds: world.grid.contains(x, y),
        cell: world.grid.contains(x, y).then(|| cell_view(world, x, y)),
    }
}

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
