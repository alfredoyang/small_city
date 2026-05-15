use crate::core::systems::{citizens, power, road_connectivity};
use crate::core::world::World;
use crate::interface::input::{BuildingKind, MapOverlayInput};
use crate::interface::view::{
    BuildOptionView, CellView, CityDemand, CityStatusView, DemandLevel, GameView,
    InspectDetailsView, InspectView, LocalEffectsView, MapView, PowerStatusView,
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
            citizens: citizens::citizen_count(world),
            jobs: world.stats.jobs,
            unemployment: world.stats.unemployment,
            pollution: world.stats.pollution,
            happiness: world.stats.happiness,
            average_citizen_happiness: citizens::average_happiness(world),
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
            maintenance_cost: kind.maintenance_cost(),
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
        local_effects: in_bounds.then(|| local_effects_view(world, x, y)),
        explanations: in_bounds
            .then(|| inspect_explanations(world, x, y))
            .unwrap_or_default(),
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
                upgrade_level: building.level,
                population: population.map(|population| population.current).unwrap_or(0),
                max_population: population.map(|population| population.max).unwrap_or(0),
                citizens: citizens::citizen_count_for_home(world, entity),
                average_happiness: citizens::average_happiness_for_home(world, entity),
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
            upgrade_level: building.level,
            power_capacity: world
                .power_providers
                .get(&entity)
                .map(|provider| provider.capacity)
                .unwrap_or(0),
        },
        BuildingKind::Park => InspectDetailsView::Park {
            road_connected: road_connectivity::is_road_connected(world, entity),
            upgrade_level: building.level,
            happiness_effect: world
                .happiness_effects
                .get(&entity)
                .map(|effect| effect.amount)
                .unwrap_or(0),
        },
    }
}

fn inspect_explanations(world: &World, x: usize, y: usize) -> Vec<String> {
    let Some(entity) = world.grid.get(x, y) else {
        return vec!["Empty cells can be built on if the city has enough money.".to_string()];
    };

    let Some(building) = world.buildings.get(&entity) else {
        return vec!["This cell has an unknown entity type.".to_string()];
    };

    let mut explanations = Vec::new();
    let road_connected =
        building.kind == BuildingKind::Road || road_connectivity::is_road_connected(world, entity);

    match building.kind {
        BuildingKind::Road => {
            if power::is_powered_road(world, x, y) {
                explanations.push("This road is part of a powered road network.".to_string());
            } else {
                explanations.push(
                    "This road network needs an adjacent power plant to carry power.".to_string(),
                );
            }
        }
        BuildingKind::Residential => {
            explain_road_and_power(world, entity, road_connected, &mut explanations);
            let available_jobs = (world.stats.jobs - world.stats.population).max(0);
            if available_jobs == 0 {
                explanations.push(
                    "Population growth is blocked because no jobs are available.".to_string(),
                );
            }
            if let Some(population) = world.populations.get(&entity) {
                if population.current >= population.max {
                    explanations
                        .push("This residential building is at max population.".to_string());
                }
            }
            if world.stats.pollution > 0 {
                explanations.push(format!(
                    "City pollution is reducing happiness by {}.",
                    world.stats.pollution
                ));
            }
        }
        BuildingKind::Commercial => {
            explain_road_and_power(world, entity, road_connected, &mut explanations);
            if road_connected && is_consumer_powered(world, entity) {
                explanations.push("Provides 2 effective jobs and income.".to_string());
            } else {
                explanations.push(
                    "Jobs and income are blocked until road and power requirements are met."
                        .to_string(),
                );
            }
        }
        BuildingKind::Industrial => {
            explain_road_and_power(world, entity, road_connected, &mut explanations);
            if road_connected && is_consumer_powered(world, entity) {
                explanations.push("Provides 3 effective jobs and income.".to_string());
            } else {
                explanations.push(
                    "Jobs and income are blocked until road and power requirements are met."
                        .to_string(),
                );
            }
            if let Some(source) = world.pollution_sources.get(&entity) {
                explanations.push(format!("Local effect: adds {} pollution.", source.amount));
            }
        }
        BuildingKind::PowerPlant => {
            if power::is_power_provider_connected(world, entity) {
                explanations.push("Supplies capacity to adjacent road networks.".to_string());
            } else {
                explanations
                    .push("Power output is blocked because no road is adjacent.".to_string());
            }
            if let Some(provider) = world.power_providers.get(&entity) {
                explanations.push(format!("Provides {} power capacity.", provider.capacity));
            }
        }
        BuildingKind::Park => {
            if !road_connected {
                explanations.push("This park is missing an adjacent road.".to_string());
            }
            if let Some(effect) = world.happiness_effects.get(&entity) {
                explanations.push(format!("Local effect: adds +{} happiness.", effect.amount));
            }
        }
    }

    let level = building.level;
    if level > 0 {
        explanations.push(format!("Upgrade level: {level}."));
    } else if let Some(cost) = building.kind.upgrade_cost() {
        explanations.push(format!("Can be upgraded for {cost}."));
    }

    let effects = world.local_effects.get(x, y);
    explanations.push(format!(
        "Local effects: land value {}, pollution pressure {}, accessibility {}, desirability {}.",
        effects.land_value, effects.pollution_pressure, effects.accessibility, effects.desirability
    ));

    explanations
}

fn explain_road_and_power(
    world: &World,
    entity: crate::core::entity::Entity,
    road_connected: bool,
    explanations: &mut Vec<String>,
) {
    if !road_connected {
        explanations.push("Blocked: no orthogonally adjacent road.".to_string());
        return;
    }

    if is_consumer_powered(world, entity) {
        explanations.push("Connected to a powered road network.".to_string());
    } else if adjacent_powered_road_count(world, entity) > 0
        && world.stats.power.total_power_shortage > 0
    {
        explanations.push("Blocked: connected power network lacks enough capacity.".to_string());
    } else {
        explanations.push("Blocked: adjacent road network is not powered.".to_string());
    }
}

fn is_consumer_powered(world: &World, entity: crate::core::entity::Entity) -> bool {
    world
        .power_consumers
        .get(&entity)
        .is_some_and(|consumer| consumer.powered)
}

fn adjacent_powered_road_count(world: &World, entity: crate::core::entity::Entity) -> usize {
    let Some(position) = world.positions.get(&entity) else {
        return 0;
    };
    adjacent_coordinates(position.x, position.y)
        .into_iter()
        .flatten()
        .filter(|(x, y)| world.grid.contains(*x, *y) && power::is_powered_road(world, *x, *y))
        .count()
}

fn adjacent_coordinates(x: usize, y: usize) -> [Option<(usize, usize)>; 4] {
    [
        x.checked_sub(1).map(|left| (left, y)),
        Some((x.saturating_add(1), y)),
        y.checked_sub(1).map(|up| (x, up)),
        Some((x, y.saturating_add(1))),
    ]
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
            upgrade_level: None,
            local_effects: local_effects_view(world, x, y),
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
    let upgrade_level = building.map_or(0, |building| building_level(world, entity, building));

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
        upgrade_level: (upgrade_level > 0).then_some(upgrade_level),
        local_effects: local_effects_view(world, x, y),
    }
}

fn local_effects_view(world: &World, x: usize, y: usize) -> LocalEffectsView {
    let effects = world.local_effects.get(x, y);
    LocalEffectsView {
        land_value: effects.land_value,
        pollution_pressure: effects.pollution_pressure,
        accessibility: effects.accessibility,
        desirability: effects.desirability,
    }
}

fn building_level(world: &World, entity: crate::core::entity::Entity, kind: BuildingKind) -> u8 {
    world
        .buildings
        .get(&entity)
        .filter(|building| building.kind == kind)
        .map(|building| building.level)
        .unwrap_or(0)
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
        MapOverlayInput::LandValue => digit_symbol(world.local_effects.get(x, y).land_value),
        MapOverlayInput::Desirability => digit_symbol(world.local_effects.get(x, y).desirability),
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
        MapOverlayInput::LandValue => digit_symbol(world.local_effects.get(x, y).land_value),
        MapOverlayInput::Desirability => digit_symbol(world.local_effects.get(x, y).desirability),
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
