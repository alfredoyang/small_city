use crate::core::systems::{power, road_connectivity};
use crate::core::world::World;
use crate::interface::input::{BuildingKind, MapOverlayInput};
use crate::interface::view::{
    BuildOptionView, CellView, CityDemand, CityStatusView, DemandLevel, GameView,
    InspectDetailsView, InspectView, MapView, PowerStatusView,
};

/// Converts the private ECS World into the only render model the UI may consume.
pub(crate) fn view_world(world: &World) -> GameView {
    view_world_with_overlay(world, MapOverlayInput::Normal)
}

/// Converts the private ECS World into a render model using the requested map overlay.
pub(crate) fn view_world_with_overlay(world: &World, overlay: MapOverlayInput) -> GameView {
    let mut cells = Vec::with_capacity(world.grid.width() * world.grid.height());
    for y in 0..world.grid.height() {
        for x in 0..world.grid.width() {
            cells.push(cell_view_with_overlay(world, x, y, overlay));
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
            demand: calculate_demand(
                world.stats.population,
                world.stats.jobs,
                world.stats.unemployment,
                world.stats.pollution,
                world.stats.happiness,
            ),
            power: PowerStatusView {
                total_capacity: world.stats.power.total_power_capacity,
                total_demand: world.stats.power.total_power_demand,
                total_supplied: world.stats.power.total_power_supplied,
                total_shortage: world.stats.power.total_power_shortage,
            },
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

pub(crate) fn calculate_demand(
    population: i32,
    jobs: i32,
    unemployment: i32,
    pollution: i32,
    happiness: i32,
) -> CityDemand {
    let available_jobs = jobs - population;

    CityDemand {
        residential: if available_jobs >= 3 && happiness >= 50 {
            DemandLevel::High
        } else if available_jobs > 0 && happiness >= 35 {
            DemandLevel::Medium
        } else {
            DemandLevel::Low
        },
        commercial: if population > jobs + 3 {
            DemandLevel::High
        } else if population > 0 && population >= jobs {
            DemandLevel::Medium
        } else {
            DemandLevel::Low
        },
        industrial: if unemployment >= 3 && pollution <= 4 {
            DemandLevel::High
        } else if unemployment > 0 && pollution <= 8 {
            DemandLevel::Medium
        } else {
            DemandLevel::Low
        },
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
            let consumer = world.power_consumers.get(&entity);
            InspectDetailsView::Residential {
                powered: consumer.map(|consumer| consumer.powered).unwrap_or(false),
                power_demand: consumer.map(|consumer| consumer.demand).unwrap_or(0),
                road_connected: road_connectivity::is_road_connected(world, entity),
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
            power_demand: world
                .power_consumers
                .get(&entity)
                .map(|consumer| consumer.demand)
                .unwrap_or(0),
            road_connected: road_connectivity::is_road_connected(world, entity),
            jobs: effective_jobs(world, entity, building.kind),
        },
        BuildingKind::Industrial => InspectDetailsView::Industrial {
            powered: world
                .power_consumers
                .get(&entity)
                .map(|consumer| consumer.powered)
                .unwrap_or(false),
            power_demand: world
                .power_consumers
                .get(&entity)
                .map(|consumer| consumer.demand)
                .unwrap_or(0),
            road_connected: road_connectivity::is_road_connected(world, entity),
            jobs: effective_jobs(world, entity, building.kind),
        },
        BuildingKind::PowerPlant => InspectDetailsView::PowerPlant {
            road_connected: road_connectivity::is_road_connected(world, entity),
            connected_to_road_network: power::is_power_provider_connected(world, entity),
            power_capacity: world
                .power_providers
                .get(&entity)
                .map(|provider| provider.capacity)
                .unwrap_or(0),
        },
        BuildingKind::Park => InspectDetailsView::Park {
            road_connected: road_connectivity::is_road_connected(world, entity),
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
    cell_view_with_overlay(world, x, y, MapOverlayInput::Normal)
}

fn cell_view_with_overlay(world: &World, x: usize, y: usize, overlay: MapOverlayInput) -> CellView {
    let Some(entity) = world.grid.get(x, y) else {
        return CellView {
            x,
            y,
            symbol: empty_symbol(world, x, y, overlay),
            building: None,
            label: "Empty".to_string(),
            buildable: true,
            population: None,
            max_population: None,
            powered: None,
            power_demand: None,
            road_connected: None,
        };
    };

    let building = world.buildings.get(&entity).map(|building| building.kind);
    let population = world.populations.get(&entity);
    let powered = world
        .power_consumers
        .get(&entity)
        .map(|consumer| consumer.powered);
    let power_demand = world
        .power_consumers
        .get(&entity)
        .map(|consumer| consumer.demand);
    let normal_symbol = building.map_or('?', BuildingKind::symbol);

    CellView {
        x,
        y,
        symbol: overlay_symbol(world, entity, x, y, normal_symbol, overlay),
        building,
        label: building.map_or("Unknown", BuildingKind::label).to_string(),
        buildable: false,
        population: population.map(|population| population.current),
        max_population: population.map(|population| population.max),
        powered,
        power_demand,
        road_connected: building.and_then(|kind| {
            (kind != BuildingKind::Road)
                .then(|| road_connectivity::is_road_connected(world, entity))
        }),
    }
}

fn effective_jobs(world: &World, entity: crate::core::entity::Entity, kind: BuildingKind) -> i32 {
    let powered = world
        .power_consumers
        .get(&entity)
        .is_some_and(|consumer| consumer.powered);
    if powered && road_connectivity::is_road_connected(world, entity) {
        kind.jobs()
    } else {
        0
    }
}

fn overlay_symbol(
    world: &World,
    entity: crate::core::entity::Entity,
    x: usize,
    y: usize,
    normal_symbol: char,
    overlay: MapOverlayInput,
) -> char {
    match overlay {
        MapOverlayInput::Normal => normal_symbol,
        MapOverlayInput::Power => {
            if world.power_providers.contains_key(&entity) {
                'P'
            } else if normal_symbol == '=' {
                if power::is_powered_road(world, x, y) {
                    '*'
                } else {
                    '='
                }
            } else {
                world
                    .power_consumers
                    .get(&entity)
                    .map(|consumer| if consumer.powered { '+' } else { '-' })
                    .unwrap_or('.')
            }
        }
        MapOverlayInput::Pollution => world
            .pollution_sources
            .get(&entity)
            .map(|source| digit_symbol(source.amount))
            .unwrap_or('.'),
        MapOverlayInput::Population => world
            .populations
            .get(&entity)
            .map(|population| digit_symbol(population.current))
            .unwrap_or('.'),
    }
}

fn empty_symbol(world: &World, x: usize, y: usize, overlay: MapOverlayInput) -> char {
    match overlay {
        MapOverlayInput::Power => {
            if power::is_powered_road(world, x, y) {
                '*'
            } else {
                '.'
            }
        }
        _ => '.',
    }
}

fn digit_symbol(value: i32) -> char {
    char::from_digit(value.clamp(0, 9) as u32, 10).unwrap_or('0')
}

#[cfg(test)]
mod tests {
    use super::calculate_demand;
    use crate::interface::view::{CityDemand, DemandLevel};

    #[test]
    fn demand_is_low_without_population_or_available_jobs() {
        assert_eq!(
            calculate_demand(0, 0, 0, 0, 50),
            CityDemand {
                residential: DemandLevel::Low,
                commercial: DemandLevel::Low,
                industrial: DemandLevel::Low,
            }
        );
    }

    #[test]
    fn residential_demand_rises_when_jobs_and_happiness_are_available() {
        assert_eq!(
            calculate_demand(1, 3, 0, 0, 45).residential,
            DemandLevel::Medium
        );
        assert_eq!(
            calculate_demand(1, 4, 0, 0, 55).residential,
            DemandLevel::High
        );
    }

    #[test]
    fn commercial_demand_rises_when_population_exceeds_jobs() {
        assert_eq!(
            calculate_demand(2, 2, 0, 0, 50).commercial,
            DemandLevel::Medium
        );
        assert_eq!(
            calculate_demand(7, 3, 4, 0, 50).commercial,
            DemandLevel::High
        );
    }

    #[test]
    fn industrial_demand_rises_with_unemployment_but_drops_when_pollution_is_high() {
        assert_eq!(
            calculate_demand(2, 1, 1, 2, 50).industrial,
            DemandLevel::Medium
        );
        assert_eq!(
            calculate_demand(6, 2, 4, 2, 50).industrial,
            DemandLevel::High
        );
        assert_eq!(
            calculate_demand(6, 2, 4, 9, 50).industrial,
            DemandLevel::Low
        );
    }
}
