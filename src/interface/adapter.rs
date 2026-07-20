//! Adapter that converts private ECS world data into UI-safe view and inspect models.

use crate::core::city_refs::CityCellRef;
use crate::core::components::{Citizen, Position, TravelToken};
use crate::core::entity::Entity;
use crate::core::regions::RegionId;
use crate::core::systems::{
    business_growth, citizens, economy, population, power, road_connectivity,
    road_network_analysis, upgrade,
};
use crate::core::world::World;
use crate::interface::input::{BuildingKind, MapOverlayInput};
use crate::interface::view::{
    BuildOptionView, CellView, CitizenDetailView, CitizenRelation, CitizenTravelView, CityDemand,
    CityStatusView, DemandLevel, GameTimeView, GameView, InspectDetailsView, InspectFlag,
    InspectView, JobAssignmentView, LocalEffectsView, MapView, PowerStatusView, RoadLinks,
    RoadTravelerEndpointView, RoadTravelerPanelSeedView,
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
        travelers: traveler_views(world),
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

/// P4 (R-a): the map cells currently holding a moving citizen, deduped and
/// sorted for a deterministic, presentation-agnostic render list. Only
/// `Travelling` tokens have a `current_cell`; idle `AtWork` tokens (inside a
/// building) and absent tokens (idle at home, or away in another region) all
/// contribute nothing. Multiple citizens sharing a cell collapse to one
/// marker. No entity id or path leaks out.
///
/// The unified `world.tokens` map holds both local tokens and foreign
/// visiting tokens (the neighbour's body is in this region — the `home.region`
/// is elsewhere). All tokens with a road cell render a dot; idle and
/// home-region-away tokens don't have a road cell and contribute nothing.
fn traveler_views(world: &World) -> Vec<CitizenTravelView> {
    let self_region = world.region_id;
    let mut cells: Vec<(usize, usize)> = world
        .tokens
        .iter()
        .filter(|(id, token)| {
            // A local token: the citizen must still be alive (the stepper
            // prunes stale entries at the end of each sub-tick, but a paused
            // frame may see a not-yet-pruned entry).
            world.citizens.contains_key(id)
                // A foreign token: the home is elsewhere, the citizen is not
                // in this region's `world.citizens`. Always include.
                || token.home.region != self_region
        })
        .filter_map(|(_, token)| token.state.current_cell)
        .filter_map(|cell| world.positions.get(&cell))
        .map(|position| (position.x, position.y))
        .collect();
    cells.sort_unstable();
    cells.dedup();
    cells
        .into_iter()
        .map(|(x, y)| CitizenTravelView { x, y })
        .collect()
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
        flags: if in_bounds {
            inspect_flags(world, x, y)
        } else {
            Default::default()
        },
        explanations: if in_bounds {
            inspect_explanations(world, x, y)
        } else {
            Default::default()
        },
        roster: if in_bounds {
            citizen_roster(world, x, y)
        } else {
            Default::default()
        },
        road_traveler_count: if in_bounds {
            road_traveler_count(world, x, y)
        } else {
            Default::default()
        },
    }
}

/// Count of travel tokens currently standing on the road cell at `(x, y)`.
/// Zero for a non-road cell or an out-of-bounds coordinate. Hover-only summary;
/// mirrors the same local-token-alive-or-visitor filter as [`traveler_views`] so
/// the count always matches what the map dot shows.
fn road_traveler_count(world: &World, x: usize, y: usize) -> usize {
    let Some(entity) = world.grid.get(x, y) else {
        return 0;
    };
    if !road_connectivity::is_road_entity(world, entity) {
        return 0;
    }

    let self_region = world.region_id;
    world
        .tokens
        .iter()
        .filter(|(id, token)| world.citizens.contains_key(id) || token.home.region != self_region)
        .filter(|(_, token)| token.state.current_cell == Some(entity))
        .count()
}

/// Enter-panel detail for the travelers standing on the road cell at `(x, y)`.
/// Local travelers (home is this region) get full `CitizenDetailView` rows, same
/// perspective as a residential roster. Visitors (home elsewhere) get endpoint
/// summary rows built only from `token.home`/`token.work` — no cross-region query.
/// Empty for a non-road cell or an out-of-bounds coordinate. Order: local rows by
/// `Entity` id, then visitor rows grouped by endpoint (see `RoadTravelerEndpointView`).
pub(crate) fn road_traveler_panel_seed(
    world: &World,
    x: usize,
    y: usize,
) -> RoadTravelerPanelSeedView {
    let Some(entity) = world.grid.get(x, y) else {
        return RoadTravelerPanelSeedView::default();
    };
    if !road_connectivity::is_road_entity(world, entity) {
        return RoadTravelerPanelSeedView::default();
    }

    let mut tokens: Vec<(&Entity, &TravelToken)> = world
        .tokens
        .iter()
        .filter(|(_, token)| token.state.current_cell == Some(entity))
        .collect();
    tokens.sort_by_key(|(id, _)| id.0);

    let mut seed = RoadTravelerPanelSeedView::default();
    let mut visitor_keys: Vec<(RegionId, Option<RegionId>, Option<CityCellRef>)> = Vec::new();
    for (id, token) in tokens {
        if token.home.region == world.region_id {
            // Stale local token whose citizen was already removed: skip, like
            // road_traveler_count's alive check.
            if let Some(citizen) = world.citizens.get(id) {
                seed.local_details.push(CitizenDetailView {
                    age: citizen.age,
                    happiness: citizen.morale.actual,
                    money: citizen.money,
                    unpaid_since_daily_settlement: citizen.workplace_assignment.is_some()
                        && !citizen.attended_since_daily_settlement,
                    relation: citizen_relation(world, BuildingKind::Residential, citizen),
                });
            }
            continue;
        }

        let local_workplace = token
            .work
            .filter(|workplace| workplace.region == world.region_id)
            .and_then(|workplace| world.positions.get(&workplace.building))
            .map(|position| CityCellRef::local(world.region_id, position.x, position.y));
        visitor_keys.push((
            token.home.region,
            token.work.map(|workplace| workplace.region),
            local_workplace,
        ));
    }

    // Group visitors sharing the exact same endpoint into one row with a count,
    // so collapsing duplicates never silently loses how many travelers a row
    // represents (unlike a plain `sort` + `dedup`).
    visitor_keys.sort();
    for (home_region, work_region, local_workplace) in visitor_keys {
        match seed.visitor_endpoints.last_mut() {
            Some(last)
                if last.home_region == home_region
                    && last.work_region == work_region
                    && last.local_workplace == local_workplace =>
            {
                last.count += 1;
            }
            _ => seed.visitor_endpoints.push(RoadTravelerEndpointView {
                home_region,
                work_region,
                local_workplace,
                count: 1,
            }),
        }
    }
    seed
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
            .filter(|(_, citizen)| citizen.home == entity)
            .collect(),
        BuildingKind::Commercial | BuildingKind::Industrial => world
            .citizens
            .iter()
            .filter(|(_, citizen)| {
                // Local workers of this building: a workplace local to this region
                // (`as_local` resolves) whose entity is this cell's building.
                citizen.workplace_assignment.is_some_and(|assignment| {
                    assignment.workplace.as_local(world.region_id) == Some(entity)
                })
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
            unpaid_since_daily_settlement: citizen.workplace_assignment.is_some()
                && !citizen.attended_since_daily_settlement,
            relation: citizen_relation(world, building.kind, citizen),
        })
        .collect()
}

fn citizen_relation(world: &World, kind: BuildingKind, citizen: &Citizen) -> CitizenRelation {
    match kind {
        BuildingKind::Residential => match citizen.workplace_assignment {
            Some(assignment) => CitizenRelation::WorksAt {
                cell: assignment.location,
                salary: assignment.salary,
                is_remote: assignment.workplace.region() != world.region_id,
            },
            None => CitizenRelation::Unemployed,
        },
        // Workplace roster: locate where this local worker lives. `region: None`
        // means "the inspected region" — the bare World cannot name itself.
        _ => {
            let home = world.positions.get(&citizen.home);
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
            citizen.workplace_assignment.is_some_and(|assignment| {
                // A remote job (workplace in another region) at the producer's cell.
                assignment.workplace.region() == producer_region
                    && assignment.workplace.region() != world.region_id
                    && assignment.location.x == position.x
                    && assignment.location.y == position.y
            })
        })
        .collect();
    citizens.sort_by_key(|(entity, _)| entity.0);

    citizens
        .into_iter()
        .map(|(_, citizen)| {
            let home = world.positions.get(&citizen.home);
            CitizenDetailView {
                age: citizen.age,
                happiness: citizen.morale.actual,
                money: citizen.money,
                unpaid_since_daily_settlement: citizen.workplace_assignment.is_some()
                    && !citizen.attended_since_daily_settlement,
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
                unpaid_citizens: unpaid_citizens_for_home(world, entity),
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

fn unpaid_citizens_for_home(world: &World, home: Entity) -> i32 {
    world
        .citizens
        .values()
        .filter(|citizen| {
            citizen.home == home
                && citizen.workplace_assignment.is_some()
                && !citizen.attended_since_daily_settlement
        })
        .count() as i32
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
            // access already affects simulation through remote employment; teaching
            // this display helper about neighbor regions is a separate UI mission.
            explain_road_access(world, entity, building.kind, &mut explanations);
            if let Some(population) = world.populations.get(&entity)
                && population.current >= population.max
            {
                explanations.push("This residential building is at max population.".to_string());
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
        .filter(|(_, citizen)| citizen.home == home)
        .collect::<Vec<_>>();
    citizens.sort_by_key(|(entity, _)| entity.0);

    citizens
        .into_iter()
        .filter_map(|(_, citizen)| {
            let assignment = citizen.workplace_assignment?;
            Some(JobAssignmentView {
                cell: assignment.location,
                salary: assignment.salary,
                is_remote: assignment.workplace.region() != world.region_id,
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
    use super::{calculate_demand, traveler_views, view_world};
    use crate::core::city_refs::CityCellRef;
    use crate::core::components::{
        Citizen, Morale, PlaceRef, TravelState, TravelStatus, TravelToken,
    };
    use crate::core::entity::Entity;
    use crate::core::systems::placement::place_building;
    use crate::core::world::World;
    use crate::interface::input::BuildingKind;
    use crate::interface::view::{
        CitizenDetailView, CitizenRelation, CitizenTravelView, CityDemand, DemandLevel,
        RoadTravelerEndpointView,
    };

    /// Inserts a citizen and its travel state. `cell = None` ⇒ idle (no token).
    fn add_citizen(world: &mut World, local: u32, cell: Option<Entity>) -> Entity {
        let id = Entity::new(world.region_id, local);
        world.citizens.insert(
            id,
            Citizen {
                id,
                age: 1,
                home: id,
                workplace_assignment: None,
                morale: Morale::default(),
                money: 0,
                arrival_action: crate::core::components::CitizenArrivalAction::ReturnHome,
                work_trip_generation: 0,
                attended_since_daily_settlement: false,
            },
        );
        if let Some(cell) = cell {
            world.tokens.insert(
                id,
                TravelToken {
                    state: TravelState {
                        status: TravelStatus::Traveling,
                        current_cell: Some(cell),
                        destination: None,
                        building: None,
                        dwell: 0,
                        prev_cell: None,
                    },
                    home: PlaceRef {
                        region: world.region_id,
                        building: id,
                    },
                    work: None,
                    trip_gen: 0,
                },
            );
        }
        id
    }

    /// No moving citizens (or empty roads) → no markers.
    #[test]
    fn traveler_views_empty_when_nobody_is_moving() {
        let mut world = World::new(3, 1);
        place_building(&mut world, 0, 0, BuildingKind::Road);
        add_citizen(&mut world, 1, None); // idle citizen contributes nothing
        assert!(traveler_views(&world).is_empty());
    }

    /// Each moving citizen's cell is reported, deduped (shared cell → one marker)
    /// and sorted; idle citizens are excluded.
    #[test]
    fn traveler_views_reports_moving_cells_deduped_and_sorted() {
        let mut world = World::new(3, 1);
        place_building(&mut world, 0, 0, BuildingKind::Road);
        place_building(&mut world, 1, 0, BuildingKind::Road);
        let r0 = world.grid.get(0, 0).expect("r0");
        let r1 = world.grid.get(1, 0).expect("r1");

        // Two citizens on r0 (collapse to one marker), one on r1, one idle.
        add_citizen(&mut world, 1, Some(r0));
        add_citizen(&mut world, 2, Some(r0));
        add_citizen(&mut world, 3, Some(r1));
        add_citizen(&mut world, 4, None);

        assert_eq!(
            traveler_views(&world),
            vec![
                CitizenTravelView { x: 0, y: 0 },
                CitizenTravelView { x: 1, y: 0 },
            ]
        );
    }

    /// A token whose citizen was removed (not yet pruned) is skipped, so a
    /// paused frame never shows a stale dot. The `world.tokens.retain` in the
    /// stepper prunes this, but the adapter is more conservative and filters
    /// directly.
    #[test]
    fn traveler_views_excludes_removed_citizen() {
        let mut world = World::new(2, 1);
        place_building(&mut world, 0, 0, BuildingKind::Road);
        let r0 = world.grid.get(0, 0).expect("r0");
        let id = add_citizen(&mut world, 1, Some(r0));
        assert_eq!(traveler_views(&world).len(), 1);

        // Remove the citizen but leave the (not-yet-pruned) token.
        world.citizens.remove(&id);
        assert!(world.tokens.contains_key(&id));
        assert!(
            traveler_views(&world).is_empty(),
            "stale dot must not render"
        );
    }

    /// Foreign tokens (visiting from a neighbour) render dots too, and dedupe
    /// against a local traveller sharing the same cell. The unified `world.tokens`
    /// map holds both.
    #[test]
    fn traveler_views_includes_visiting_tokens() {
        use crate::core::regions::RegionId;

        let mut world = World::new(2, 1);
        place_building(&mut world, 0, 0, BuildingKind::Road);
        place_building(&mut world, 1, 0, BuildingKind::Road);
        let r0 = world.grid.get(0, 0).expect("r0");
        let r1 = world.grid.get(1, 0).expect("r1");

        // A local traveller on r0 and a foreign token on r0 collapse to one
        // marker; a second foreign token on r1 adds another.
        add_citizen(&mut world, 1, Some(r0));
        let foreign_on = |cell| TravelToken {
            state: TravelState {
                status: TravelStatus::Traveling,
                current_cell: Some(cell),
                destination: None,
                building: None,
                dwell: 0,
                prev_cell: None,
            },
            home: PlaceRef {
                region: RegionId(3),
                building: Entity::new(RegionId(3), 0),
            },
            work: None,
            trip_gen: 1,
        };
        let _ = add_citizen(&mut world, 2, Some(r0)); // local on r0 too
        let _ = add_foreign_token(&mut world, Entity::new(RegionId(3), 1), foreign_on(r0));
        let _ = add_foreign_token(&mut world, Entity::new(RegionId(3), 2), foreign_on(r1));

        assert_eq!(
            traveler_views(&world),
            vec![
                CitizenTravelView { x: 0, y: 0 },
                CitizenTravelView { x: 1, y: 0 },
            ]
        );
    }

    fn add_foreign_token(world: &mut World, _citizen: Entity, token: TravelToken) -> Entity {
        // Use a unique key. For a foreign token, the citizen key in
        // `world.tokens` doesn't have to be a real citizen in this region —
        // it's just the unique map key.
        let key = Entity::new(world.region_id, 90);
        world.tokens.insert(key, token);
        key
    }

    /// A road with no tokens on it reports zero travelers.
    #[test]
    fn road_traveler_count_is_zero_with_no_tokens() {
        let mut world = World::new(2, 1);
        place_building(&mut world, 0, 0, BuildingKind::Road);
        add_citizen(&mut world, 1, None); // idle citizen contributes nothing

        assert_eq!(super::inspect_world(&world, 0, 0).road_traveler_count, 0);
    }

    /// A non-road cell always reports zero, even with tokens standing on
    /// neighbouring road cells.
    #[test]
    fn road_traveler_count_is_zero_for_non_road_cell() {
        let mut world = World::new(2, 1);
        place_building(&mut world, 0, 0, BuildingKind::Road);
        let r0 = world.grid.get(0, 0).expect("r0");
        add_citizen(&mut world, 1, Some(r0));

        assert_eq!(super::inspect_world(&world, 1, 0).road_traveler_count, 0);
    }

    /// Local travelers and a visiting foreign token on the same road cell all
    /// count, matching how many dots `traveler_views` would draw there.
    #[test]
    fn road_traveler_count_includes_local_and_visitor_tokens() {
        use crate::core::regions::RegionId;

        let mut world = World::new(2, 1);
        place_building(&mut world, 0, 0, BuildingKind::Road);
        let r0 = world.grid.get(0, 0).expect("r0");

        add_citizen(&mut world, 1, Some(r0));
        add_citizen(&mut world, 2, Some(r0));
        world.tokens.insert(
            Entity::new(RegionId(3), 1),
            TravelToken {
                state: TravelState {
                    status: TravelStatus::Traveling,
                    current_cell: Some(r0),
                    destination: None,
                    building: None,
                    dwell: 0,
                    prev_cell: None,
                },
                home: PlaceRef {
                    region: RegionId(3),
                    building: Entity::new(RegionId(3), 0),
                },
                work: None,
                trip_gen: 1,
            },
        );

        assert_eq!(super::inspect_world(&world, 0, 0).road_traveler_count, 3);
    }

    /// A removed local citizen's not-yet-pruned token is excluded from the
    /// count, mirroring `traveler_views`'s stale-dot guard; a foreign token
    /// on the same cell is still counted.
    #[test]
    fn road_traveler_count_excludes_removed_local_citizen_but_counts_foreign_token() {
        use crate::core::regions::RegionId;

        let mut world = World::new(2, 1);
        place_building(&mut world, 0, 0, BuildingKind::Road);
        let r0 = world.grid.get(0, 0).expect("r0");

        let id = add_citizen(&mut world, 1, Some(r0));
        world.tokens.insert(
            Entity::new(RegionId(3), 1),
            TravelToken {
                state: TravelState {
                    status: TravelStatus::Traveling,
                    current_cell: Some(r0),
                    destination: None,
                    building: None,
                    dwell: 0,
                    prev_cell: None,
                },
                home: PlaceRef {
                    region: RegionId(3),
                    building: Entity::new(RegionId(3), 0),
                },
                work: None,
                trip_gen: 1,
            },
        );
        assert_eq!(super::inspect_world(&world, 0, 0).road_traveler_count, 2);

        // Remove the citizen but leave the (not-yet-pruned) token.
        world.citizens.remove(&id);
        assert!(world.tokens.contains_key(&id));

        assert_eq!(
            super::inspect_world(&world, 0, 0).road_traveler_count,
            1,
            "stale local token must not count, foreign token still does"
        );
    }

    /// A local traveler (home is this region) gets a full `CitizenDetailView`
    /// row, same shape as a residential roster.
    #[test]
    fn road_traveler_panel_seed_returns_local_citizen_detail_rows() {
        let mut world = World::new(2, 1);
        place_building(&mut world, 0, 0, BuildingKind::Road);
        let r0 = world.grid.get(0, 0).expect("r0");
        add_citizen(&mut world, 1, Some(r0));

        let seed = super::road_traveler_panel_seed(&world, 0, 0);

        assert_eq!(
            seed.local_details,
            vec![CitizenDetailView {
                age: 1,
                happiness: 50,
                money: 0,
                unpaid_since_daily_settlement: false,
                relation: CitizenRelation::Unemployed,
            }]
        );
        assert!(seed.visitor_endpoints.is_empty());
    }

    /// A visitor's endpoint row is built only from `token.home`/`token.work` —
    /// no remote-region query. A workplace outside this region has no resolvable
    /// local coordinates.
    #[test]
    fn road_traveler_panel_seed_visitor_endpoint_uses_token_home_and_work() {
        use crate::core::regions::RegionId;

        let mut world = World::new(2, 1);
        place_building(&mut world, 0, 0, BuildingKind::Road);
        let r0 = world.grid.get(0, 0).expect("r0");
        world.tokens.insert(
            Entity::new(RegionId(3), 1),
            TravelToken {
                state: TravelState {
                    status: TravelStatus::Traveling,
                    current_cell: Some(r0),
                    destination: None,
                    building: None,
                    dwell: 0,
                    prev_cell: None,
                },
                home: PlaceRef {
                    region: RegionId(3),
                    building: Entity::new(RegionId(3), 0),
                },
                work: Some(PlaceRef {
                    region: RegionId(5),
                    building: Entity::new(RegionId(5), 9),
                }),
                trip_gen: 1,
            },
        );

        let seed = super::road_traveler_panel_seed(&world, 0, 0);

        assert!(seed.local_details.is_empty());
        assert_eq!(
            seed.visitor_endpoints,
            vec![RoadTravelerEndpointView {
                home_region: RegionId(3),
                work_region: Some(RegionId(5)),
                local_workplace: None,
                count: 1,
            }]
        );
    }

    /// A visitor whose workplace is in the inspected region resolves local
    /// coordinates from `world.positions`.
    #[test]
    fn road_traveler_panel_seed_includes_local_workplace_when_resolvable() {
        use crate::core::regions::RegionId;

        let mut world = World::new(2, 1);
        place_building(&mut world, 0, 0, BuildingKind::Road);
        place_building(&mut world, 1, 0, BuildingKind::Commercial);
        let r0 = world.grid.get(0, 0).expect("r0");
        let workplace = world.grid.get(1, 0).expect("workplace");
        let region_id = world.region_id;

        world.tokens.insert(
            Entity::new(RegionId(3), 1),
            TravelToken {
                state: TravelState {
                    status: TravelStatus::Traveling,
                    current_cell: Some(r0),
                    destination: None,
                    building: None,
                    dwell: 0,
                    prev_cell: None,
                },
                home: PlaceRef {
                    region: RegionId(3),
                    building: Entity::new(RegionId(3), 0),
                },
                work: Some(PlaceRef {
                    region: region_id,
                    building: workplace,
                }),
                trip_gen: 1,
            },
        );

        let seed = super::road_traveler_panel_seed(&world, 0, 0);

        assert_eq!(
            seed.visitor_endpoints,
            vec![RoadTravelerEndpointView {
                home_region: RegionId(3),
                work_region: Some(region_id),
                local_workplace: Some(CityCellRef::local(region_id, 1, 0)),
                count: 1,
            }]
        );
    }

    /// A jobless transit visitor (no workplace at all) still gets an endpoint
    /// row showing its home region, never a fake `CitizenDetailView` row.
    #[test]
    fn road_traveler_panel_seed_transit_visitor_has_no_fake_local_row() {
        use crate::core::regions::RegionId;

        let mut world = World::new(2, 1);
        place_building(&mut world, 0, 0, BuildingKind::Road);
        let r0 = world.grid.get(0, 0).expect("r0");
        world.tokens.insert(
            Entity::new(RegionId(3), 1),
            TravelToken {
                state: TravelState {
                    status: TravelStatus::Traveling,
                    current_cell: Some(r0),
                    destination: None,
                    building: None,
                    dwell: 0,
                    prev_cell: None,
                },
                home: PlaceRef {
                    region: RegionId(3),
                    building: Entity::new(RegionId(3), 0),
                },
                work: None,
                trip_gen: 1,
            },
        );

        let seed = super::road_traveler_panel_seed(&world, 0, 0);

        assert!(seed.local_details.is_empty(), "no fake local row");
        assert_eq!(
            seed.visitor_endpoints,
            vec![RoadTravelerEndpointView {
                home_region: RegionId(3),
                work_region: None,
                local_workplace: None,
                count: 1,
            }]
        );
    }

    /// Two visitors sharing the exact same endpoint (home region, work region,
    /// local workplace) group into one row with `count: 2`, instead of a plain
    /// dedup silently collapsing them into a single, uncounted row. A third
    /// visitor with a different home region stays a separate row.
    #[test]
    fn road_traveler_panel_seed_groups_visitors_sharing_an_endpoint_with_a_count() {
        use crate::core::regions::RegionId;

        let mut world = World::new(2, 1);
        place_building(&mut world, 0, 0, BuildingKind::Road);
        let r0 = world.grid.get(0, 0).expect("r0");
        let transit_from = |local: u32, home_region: RegionId| TravelToken {
            state: TravelState {
                status: TravelStatus::Traveling,
                current_cell: Some(r0),
                destination: None,
                building: None,
                dwell: 0,
                prev_cell: None,
            },
            home: PlaceRef {
                region: home_region,
                building: Entity::new(home_region, local),
            },
            work: None,
            trip_gen: 1,
        };
        world
            .tokens
            .insert(Entity::new(RegionId(3), 1), transit_from(0, RegionId(3)));
        world
            .tokens
            .insert(Entity::new(RegionId(3), 2), transit_from(1, RegionId(3)));
        world
            .tokens
            .insert(Entity::new(RegionId(4), 1), transit_from(0, RegionId(4)));

        let seed = super::road_traveler_panel_seed(&world, 0, 0);

        assert_eq!(
            seed.visitor_endpoints,
            vec![
                RoadTravelerEndpointView {
                    home_region: RegionId(3),
                    work_region: None,
                    local_workplace: None,
                    count: 2,
                },
                RoadTravelerEndpointView {
                    home_region: RegionId(4),
                    work_region: None,
                    local_workplace: None,
                    count: 1,
                },
            ]
        );
    }

    /// The marker list reaches the public `view_world` render model.
    #[test]
    fn view_world_carries_traveler_markers() {
        let mut world = World::new(2, 1);
        place_building(&mut world, 0, 0, BuildingKind::Road);
        let r0 = world.grid.get(0, 0).expect("r0");
        add_citizen(&mut world, 1, Some(r0));

        let view = view_world(&world);
        assert_eq!(view.travelers, vec![CitizenTravelView { x: 0, y: 0 }]);
    }

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
