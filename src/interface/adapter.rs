//! Adapter that converts private ECS world data into UI-safe view and inspect models.

use crate::core::components::{Citizen, Position, WorkplaceSource};
use crate::core::entity::Entity;
use crate::core::regions::RegionId;
use crate::core::systems::{
    business_growth, citizens, economy, population, power, road_connectivity,
    road_network_analysis, upgrade,
};
use crate::core::world::World;
use crate::interface::input::{BuildingKind, MapOverlayInput};
use crate::interface::view::{
    BuildOptionView, CellView, CitizenDetailView, CitizenRelation, CityDemand, CityStatusView,
    DemandLevel, GameTimeView, GameView, InspectDetailsView, InspectFlag, InspectView,
    JobAssignmentView, LocalEffectsView, MapView, PowerStatusView, RoadLinks,
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
            time: game_time_view(world.resources.time),
            population: world.stats.population,
            citizens: citizens::citizen_count(world),
            jobs: world.stats.jobs,
            unemployment: world.stats.unemployment,
            pollution: world.stats.pollution,
            happiness: world.stats.happiness,
            average_citizen_happiness: citizens::average_happiness(world),
            average_citizen_happiness_target: citizens::average_happiness_target(world),
            average_citizen_money: citizens::average_money(world),
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
            goods: Default::default(),
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

fn game_time_view(time: crate::core::resources::GameTime) -> GameTimeView {
    GameTimeView {
        total_hours: time.total_hours,
        year: time.year(),
        month: time.month(),
        week: time.week_of_month(),
        day: time.day_of_week(),
        hour: time.hour_of_day(),
        label: time.label(),
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
        flags: in_bounds
            .then(|| inspect_flags(world, x, y))
            .unwrap_or_default(),
        explanations: in_bounds
            .then(|| inspect_explanations(world, x, y))
            .unwrap_or_default(),
        roster: in_bounds
            .then(|| citizen_roster(world, x, y))
            .unwrap_or_default(),
    }
}

/// Per-citizen roster for the building at `(x, y)`, kept UI-safe inside the adapter.
///
/// ```text
///   Residential          -> residents (home == building), each: WorksAt | Unemployed
///   Commercial/Industrial -> LOCAL workers (workplace == building), each: LivesAt
///   anything else / empty -> []
/// ```
///
/// Remote workers (imported from another region) are not in this region's world,
/// so a workplace lists local workers only. Order is deterministic: by `Entity` id.
fn citizen_roster(world: &World, x: usize, y: usize) -> Vec<CitizenDetailView> {
    let Some(entity) = world.grid.get(x, y) else {
        return Vec::new();
    };
    let Some(building) = world.buildings.get(&entity) else {
        return Vec::new();
    };

    let mut citizens: Vec<(&Entity, &Citizen)> = match building.kind {
        BuildingKind::Residential => world
            .citizens
            .iter()
            .filter(|(_, citizen)| citizen.home.entity == entity)
            .collect(),
        BuildingKind::Commercial | BuildingKind::Industrial => world
            .citizens
            .iter()
            .filter(|(_, citizen)| {
                matches!(
                    citizen.workplace_assignment.map(|assignment| assignment.source),
                    Some(WorkplaceSource::Local { entity: workplace }) if workplace == entity
                )
            })
            .collect(),
        _ => return Vec::new(),
    };
    citizens.sort_by_key(|(entity, _)| entity.0);

    citizens
        .into_iter()
        .map(|(_, citizen)| CitizenDetailView {
            age: citizen.age,
            happiness: citizen.morale.actual,
            money: citizen.money,
            relation: citizen_relation(world, building.kind, citizen),
        })
        .collect()
}

fn citizen_relation(world: &World, kind: BuildingKind, citizen: &Citizen) -> CitizenRelation {
    match kind {
        BuildingKind::Residential => match citizen.workplace_assignment {
            Some(assignment) => CitizenRelation::WorksAt {
                region: assignment.region,
                x: assignment.position.x,
                y: assignment.position.y,
                salary: assignment.salary,
                is_remote: matches!(assignment.source, WorkplaceSource::Remote { .. }),
            },
            None => CitizenRelation::Unemployed,
        },
        // Workplace roster: locate where this local worker lives. `region: None`
        // means "the inspected region" — the bare World cannot name itself.
        _ => {
            let home = world.positions.get(&citizen.home.entity);
            CitizenRelation::LivesAt {
                region: None,
                x: home.map(|position| position.x).unwrap_or(0),
                y: home.map(|position| position.y).unwrap_or(0),
            }
        }
    }
}

/// Citizens of THIS region who commute to a workplace at `(producer_region, pos)`
/// in another region — the reverse of the local-only workplace roster.
///
/// ```text
///   local roster (citizen_roster):   workers whose source == Local{ this workplace }
///   remote roster (this fn):         OUR residents whose remote assignment targets
///                                    (producer_region, pos) in some other region
/// ```
///
/// The match key `(region, position)` lives on each consumer citizen's remote
/// assignment, so no shared entity id crosses the region boundary. Each worker is
/// tagged `LivesAt { region: Some(home_region) }` (where they live, i.e. this
/// region). Order is deterministic by `Entity` id.
pub(crate) fn remote_workers_for(
    world: &World,
    home_region: RegionId,
    producer_region: RegionId,
    position: Position,
) -> Vec<CitizenDetailView> {
    let mut citizens: Vec<(&Entity, &Citizen)> = world
        .citizens
        .iter()
        .filter(|(_, citizen)| {
            matches!(
                citizen.workplace_assignment,
                Some(assignment)
                    if assignment.region == producer_region
                        && assignment.position == position
                        && matches!(assignment.source, WorkplaceSource::Remote { .. })
            )
        })
        .collect();
    citizens.sort_by_key(|(entity, _)| entity.0);

    citizens
        .into_iter()
        .map(|(_, citizen)| {
            let home = world.positions.get(&citizen.home.entity);
            CitizenDetailView {
                age: citizen.age,
                happiness: citizen.morale.actual,
                money: citizen.money,
                relation: CitizenRelation::LivesAt {
                    region: Some(home_region),
                    x: home.map(|position| position.x).unwrap_or(0),
                    y: home.map(|position| position.y).unwrap_or(0),
                },
            }
        })
        .collect()
}

fn inspect_flags(world: &World, x: usize, y: usize) -> Vec<InspectFlag> {
    let Some(entity) = world.grid.get(x, y) else {
        return Vec::new();
    };
    let Some(building) = world.buildings.get(&entity) else {
        return Vec::new();
    };

    let mut flags = Vec::new();
    match building.kind {
        BuildingKind::Residential => {
            if population::available_jobs_for_growth(world) == 0 {
                flags.push(InspectFlag::GrowthBlockedNoJobs);
            }
        }
        BuildingKind::Commercial => {
            let access = road_network_analysis::access_for(world, entity);
            match goods_supply_route(world, access) {
                GoodsSupplyRoute::Local(_) => {}
                GoodsSupplyRoute::Neighbor => flags.push(InspectFlag::GoodsSupplyNeighbor),
                GoodsSupplyRoute::Missing => flags.push(InspectFlag::GoodsSupplyMissing),
            }
        }
        _ => {}
    }
    flags
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
                maintenance_cost: economy::maintenance_for_building(building.kind, building.level),
                rent_per_citizen: economy::rent_per_citizen(world, entity),
                population: population.map(|population| population.current).unwrap_or(0),
                max_population: population.map(|population| population.max).unwrap_or(0),
                citizens: citizens::citizen_count_for_home(world, entity),
                average_happiness: citizens::average_happiness_for_home(world, entity),
                average_happiness_target: citizens::average_happiness_target_for_home(
                    world, entity,
                ),
                average_money: citizens::average_money_for_home(world, entity),
                job_assignments: job_assignment_views_for_home(world, entity),
            }
        }
        BuildingKind::Commercial => {
            let (goods_sold_from_city, goods_sold_from_outside) =
                economy::recent_commercial_goods_sources(world, entity);
            InspectDetailsView::Commercial {
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
                upgrade_level: building.level,
                maintenance_cost: economy::maintenance_for_building(building.kind, building.level),
                sales_tax_per_shopper: economy::commercial_sales_tax_for_purchase(world, entity),
                goods_stored: economy::commercial_goods_stored(world, entity),
                goods_capacity: economy::commercial_goods_capacity_for_entity(world, entity),
                business_cash: economy::business_cash(world, entity),
                upgrade_threshold: business_growth::reinvestment_threshold(building.kind),
                recent_profit: economy::recent_business_profit(world, entity),
                upgrade_ready: business_growth::can_reinvest(world, entity, building.kind),
                jobs: effective_jobs(world, entity, building.kind),
                goods_sold_from_city,
                goods_sold_from_outside,
            }
        }
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
            upgrade_level: building.level,
            maintenance_cost: economy::maintenance_for_building(building.kind, building.level),
            goods_production: economy::industrial_goods_production(world, entity),
            business_cash: economy::business_cash(world, entity),
            upgrade_threshold: business_growth::reinvestment_threshold(building.kind),
            recent_profit: economy::recent_business_profit(world, entity),
            upgrade_ready: business_growth::can_reinvest(world, entity, building.kind),
            jobs: effective_jobs(world, entity, building.kind),
        },
        BuildingKind::PowerPlant => InspectDetailsView::PowerPlant {
            road_connected: road_connectivity::is_road_connected(world, entity),
            connected_to_road_network: power::is_power_provider_connected(world, entity),
            upgrade_level: building.level,
            maintenance_cost: economy::maintenance_for_building(building.kind, building.level),
            power_capacity: world
                .power_providers
                .get(&entity)
                .map(|provider| provider.capacity)
                .unwrap_or(0),
        },
        BuildingKind::Park => InspectDetailsView::Park {
            road_connected: road_connectivity::is_road_connected(world, entity),
            upgrade_level: building.level,
            maintenance_cost: economy::maintenance_for_building(building.kind, building.level),
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

    // Multi-cell footprint readout, plus a warning when the building wants to grow but is boxed in.
    let footprint = building.footprint;
    if footprint.area() > 1 {
        explanations.push(format!(
            "Footprint: {}x{} ({} cells).",
            footprint.width,
            footprint.height,
            footprint.area()
        ));
    }
    if upgrade::upgrade_blocked_for_space(world, entity) {
        explanations
            .push("No room to grow: clear an adjacent cell to level this building up.".to_string());
    }

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
            // TODO(cross-region display): this commute note is local-only. Cross-region
            // access already affects simulation through regional job exports; teaching
            // this display helper about neighbor regions is a separate UI mission.
            explain_road_access(world, entity, building.kind, &mut explanations);
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
            let rent = economy::rent_per_citizen(world, entity);
            explanations.push(format!("Economy: rent is {rent} per citizen."));
            if let Some(average_happiness) = citizens::average_happiness_for_home(world, entity) {
                if average_happiness < 40 {
                    explanations.push(
                        "Happiness blocker: average resident happiness below 40 blocks growth."
                            .to_string(),
                    );
                } else if average_happiness >= 70 {
                    explanations.push(
                        "Happiness bonus: average resident happiness at least 70 improves growth."
                            .to_string(),
                    );
                }
            }
        }
        BuildingKind::Commercial => {
            explain_road_and_power(world, entity, road_connected, &mut explanations);
            explain_road_access(world, entity, building.kind, &mut explanations);
            if road_connected && is_consumer_powered(world, entity) {
                explanations.push("Provides 2 effective jobs and income.".to_string());
                explanations.push(format!(
                    "Economy: sales tax is {} per shopper.",
                    economy::commercial_sales_tax_for_purchase(world, entity)
                ));
                explanations.push(format!(
                    "Goods: {}/{} city goods stored; goods from outside the city are bought only when storage is empty.",
                    economy::commercial_goods_stored(world, entity),
                    economy::commercial_goods_capacity_for_entity(world, entity)
                ));
                explain_business_reinvestment(world, entity, building.kind, &mut explanations);
            } else {
                explanations.push(
                    "Jobs and income are blocked until road and power requirements are met."
                        .to_string(),
                );
            }
        }
        BuildingKind::Industrial => {
            explain_road_and_power(world, entity, road_connected, &mut explanations);
            explain_road_access(world, entity, building.kind, &mut explanations);
            if road_connected && is_consumer_powered(world, entity) {
                explanations.push("Provides 3 effective jobs and income.".to_string());
                explanations.push(format!(
                    "Goods: produces {} city goods per turn for commercial storage or outside-city export.",
                    economy::industrial_goods_production(world, entity)
                ));
                explain_business_reinvestment(world, entity, building.kind, &mut explanations);
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
    explanations.push(format!(
        "Maintenance: {} per turn.",
        economy::maintenance_for_building(building.kind, building.level)
    ));

    let effects = world.local_effects.get(x, y);
    explanations.push(format!(
        "Local effects: land value {}, pollution pressure {}, accessibility {}, desirability {}.",
        effects.land_value, effects.pollution_pressure, effects.accessibility, effects.desirability
    ));

    explanations
}

fn explain_road_access(
    world: &World,
    entity: crate::core::entity::Entity,
    kind: BuildingKind,
    explanations: &mut Vec<String>,
) {
    let access = road_network_analysis::access_for(world, entity);
    match kind {
        BuildingKind::Residential => {
            explanations.push(format!(
                "Commute: nearest workplace is {}.",
                distance_note(access.commute_distance)
            ));
            explanations.push(format!(
                "Shopping: nearest commercial is {}.",
                distance_note(access.nearest_shop_distance)
            ));
        }
        BuildingKind::Commercial => {
            if let GoodsSupplyRoute::Local(distance) = goods_supply_route(world, access) {
                explanations.push(format!(
                    "Goods: nearest industrial route is {}.",
                    distance_note(Some(distance))
                ));
            }
            explanations.push(format!(
                "Trade: edge access is {}.",
                distance_note(access.import_export_distance)
            ));
        }
        BuildingKind::Industrial => {
            // An industrial does not need a nearby commercial: it sells to commercial storage *or*
            // exports off the map edge, so a missing commercial route is not a problem to flag here.
            // Its goods output is already explained in the per-kind notes above.
            explanations.push(format!(
                "Trade: edge access is {}.",
                distance_note(access.import_export_distance)
            ));
        }
        _ => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GoodsSupplyRoute {
    Local(u32),
    Neighbor,
    Missing,
}

fn goods_supply_route(
    world: &World,
    access: road_network_analysis::RoadAccess,
) -> GoodsSupplyRoute {
    if let Some(distance) = access.goods_route_distance {
        return GoodsSupplyRoute::Local(distance);
    }
    if world
        .cross_region_goods_routes
        .has_supplier_on(access.network_id)
    {
        return GoodsSupplyRoute::Neighbor;
    }
    GoodsSupplyRoute::Missing
}

fn explain_business_reinvestment(
    world: &World,
    entity: crate::core::entity::Entity,
    kind: BuildingKind,
    explanations: &mut Vec<String>,
) {
    let cash = economy::business_cash(world, entity);
    let recent_profit = economy::recent_business_profit(world, entity);
    let threshold = business_growth::reinvestment_threshold(kind).unwrap_or(0);
    explanations.push(format!(
        "Business: cash {cash}/{threshold}; recent profit {recent_profit}."
    ));

    let Some(building) = world.buildings.get(&entity) else {
        return;
    };
    if building.level >= business_growth::MAX_REINVESTMENT_LEVEL {
        explanations.push("Business: already fully upgraded.".to_string());
    } else if business_growth::can_reinvest(world, entity, kind) {
        explanations.push("Business: upgrade ready from reinvestment.".to_string());
    } else if !business_growth::demand_allows_reinvestment(world, kind) {
        explanations.push("Business: blocked by low demand.".to_string());
    } else if cash < threshold {
        explanations.push("Business: needs more retained profit before upgrading.".to_string());
    } else if recent_profit <= 0 {
        explanations.push("Business: blocked by weak recent goods or customer flow.".to_string());
    }
}

fn distance_note(distance: Option<u32>) -> String {
    distance
        .map(|distance| format!("{distance} road tiles away"))
        .unwrap_or_else(|| "unreachable by road".to_string())
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
            road_links: RoadLinks::default(),
            upgrade_level: None,
            job_assignments: Vec::new(),
            local_effects: local_effects_view(world, x, y),
            footprint_anchor: false,
            footprint_area: 0,
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
    // Footprint info for the multi-cell renderer: the grid maps every footprint cell
    // to the same entity, whose single `Position` is the anchor (top-left).
    let footprint_area = world
        .buildings
        .get(&entity)
        .map(|building| building.footprint.area().min(u32::from(u8::MAX)) as u8)
        .unwrap_or(1);
    let footprint_anchor = world
        .positions
        .get(&entity)
        .map(|anchor| anchor.x == x && anchor.y == y)
        .unwrap_or(true);

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
        road_links: road_links(world, x, y, building),
        upgrade_level: (upgrade_level > 0).then_some(upgrade_level),
        job_assignments: job_assignment_views_for_home(world, entity),
        local_effects: local_effects_view(world, x, y),
        footprint_anchor,
        footprint_area,
    }
}

fn job_assignment_views_for_home(
    world: &World,
    home: crate::core::entity::Entity,
) -> Vec<JobAssignmentView> {
    let mut citizens = world
        .citizens
        .iter()
        .filter(|(_, citizen)| citizen.home.entity == home)
        .collect::<Vec<_>>();
    citizens.sort_by_key(|(entity, _)| entity.0);

    citizens
        .into_iter()
        .filter_map(|(_, citizen)| {
            let assignment = citizen.workplace_assignment?;
            Some(JobAssignmentView {
                region: assignment.region,
                x: assignment.position.x,
                y: assignment.position.y,
                salary: assignment.salary,
                is_remote: match assignment.source {
                    WorkplaceSource::Local { .. } => false,
                    WorkplaceSource::Remote { .. } => true,
                },
            })
        })
        .collect()
}

fn road_links(world: &World, x: usize, y: usize, building: Option<BuildingKind>) -> RoadLinks {
    if building != Some(BuildingKind::Road) {
        return RoadLinks::default();
    }

    RoadLinks {
        north: y
            .checked_sub(1)
            .is_some_and(|north| is_road_at(world, x, north)),
        east: is_road_at(world, x.saturating_add(1), y),
        south: is_road_at(world, x, y.saturating_add(1)),
        west: x
            .checked_sub(1)
            .is_some_and(|west| is_road_at(world, west, y)),
    }
}

fn is_road_at(world: &World, x: usize, y: usize) -> bool {
    world
        .grid
        .get(x, y)
        .is_some_and(|entity| road_connectivity::is_road_entity(world, entity))
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
        // Mirror the jobs registry: job capacity scales with footprint area.
        let area = world
            .buildings
            .get(&entity)
            .map(|building| building.footprint.area())
            .unwrap_or(1);
        crate::core::building_stats::capacity_for(kind, area)
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
