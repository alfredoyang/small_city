//! Economy system for citizen salary, rent, shopping, taxes, and building maintenance.
//!
//! The city budget is intentionally simple: each tick, citizens earn salary from
//! productive workplaces, pay rent to the city, optionally shop at commercial
//! buildings, and buildings charge maintenance. The system returns a structured
//! `EconomyBreakdown` so UI layers can explain the money change without reading
//! ECS internals.

use crate::core::components::BuildingData;
use crate::core::systems::{road_connectivity, road_network_analysis};
use crate::core::world::World;
use crate::interface::input::BuildingKind;

const COMMERCIAL_SHOPPER_CAPACITY: i32 = 4;
const COMMERCIAL_SALARY: i32 = 3;
const INDUSTRIAL_SALARY: i32 = 4;
const LOCAL_SHOPPING_COST: i32 = 1;
const IMPORTED_SHOPPING_COST: i32 = 2;
const LOCAL_SHOPPING_HAPPINESS_BONUS: i32 = 3;
const IMPORTED_SHOPPING_HAPPINESS_BONUS: i32 = 1;
const MISSED_RENT_HAPPINESS_PENALTY: i32 = 5;
const MISSED_SHOPPING_HAPPINESS_PENALTY: i32 = 1;
const INDUSTRIAL_GOODS_PRODUCTION: i32 = 4;
const INDUSTRIAL_GOODS_PRODUCTION_PER_EXTRA_LEVEL: i32 = 2;
const COMMERCIAL_GOODS_STORAGE: i32 = 8;
const COMMERCIAL_GOODS_STORAGE_PER_EXTRA_LEVEL: i32 = 4;
const MANUFACTURING_TAX_PER_GOOD: i32 = 1;
const EXPORT_TAX_PER_GOOD: i32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EconomyBreakdown {
    pub salaries_paid: i32,
    pub workplace_tax: i32,
    pub rent_income: i32,
    pub commercial_sales_tax: i32,
    pub shoppers_served: i32,
    pub local_goods_produced: i32,
    pub local_goods_stored: i32,
    pub local_goods_sold: i32,
    pub imported_goods_sold: i32,
    pub exported_goods: i32,
    pub manufacturing_tax: i32,
    pub export_tax: i32,
    pub rent_failures: i32,
    pub maintenance_cost: i32,
    pub net: i32,
}

pub(crate) fn run(world: &mut World) -> EconomyBreakdown {
    // Maintenance is charged even when a building is unproductive. It represents
    // city upkeep for keeping the building on the map, not private profit.
    let maintenance_cost = maintenance_cost(world);

    // Workplaces and shops are recalculated every tick from powered,
    // road-connected buildings. This keeps employment and shopping deterministic
    // after build, bulldoze, replace, upgrade, save, or load.
    ensure_commercial_building_data(world);
    assign_workplaces(world);

    // Industry flow:
    // 1. Productive industrial buildings create local goods.
    // 2. Local goods fill productive commercial storage in map order.
    // 3. Surplus local goods are exported for export tax.
    // 4. Citizens shopping later consume stored local goods first; empty shops
    //    import goods at a higher citizen price and lower happiness gain.
    //
    // This encourages local production without introducing individual cargo
    // entities or pathfinding. Existing powered + road-connected rules decide
    // which industrial and commercial buildings can participate.
    let goods_flow = distribute_local_goods(world);

    let mut shopping_slots = commercial_shopping_slot_entities(world);
    let mut salaries_paid = 0;
    let mut workplace_tax = 0;
    let mut rent_income = 0;
    let mut commercial_sales_tax = 0;
    let mut shoppers_served = 0;
    let mut local_goods_sold = 0;
    let mut imported_goods_sold = 0;
    let mut rent_failures = 0;

    let mut citizen_entities: Vec<_> = world.citizens.keys().copied().collect();
    citizen_entities.sort_by_key(|citizen| citizen.0);

    for citizen_entity in citizen_entities {
        // Read-only calculations happen before the mutable citizen borrow.
        // Salary/tax come from the assigned workplace; rent comes from the
        // citizen's home land value and building level; shopping tax comes from
        // the next available commercial shopping slot.
        let salary = world
            .citizens
            .get(&citizen_entity)
            .and_then(|citizen| citizen.workplace)
            .and_then(|workplace| {
                salary_for_workplace(world, workplace)
                    .map(|salary| (salary, workplace_tax_for_workplace(world, workplace)))
            })
            .unwrap_or((0, 0));
        let rent = world
            .citizens
            .get(&citizen_entity)
            .map(|citizen| rent_per_citizen(world, citizen.home))
            .unwrap_or(1);
        let shopping = world
            .citizens
            .get(&citizen_entity)
            .map(|citizen| next_shopping_offer(world, citizen.home, &shopping_slots))
            .unwrap_or_else(|| {
                next_shopping_offer(
                    world,
                    crate::core::entity::Entity(u32::MAX),
                    &shopping_slots,
                )
            });

        let mut sold_local_good_from = None;
        {
            let Some(citizen) = world.citizens.get_mut(&citizen_entity) else {
                continue;
            };

            // Salary is private citizen money. Workplace tax is the city's income
            // from that productive job. Industrial jobs intentionally pay more tax
            // than commercial jobs.
            if salary.0 > 0 {
                citizen.money += salary.0;
                salaries_paid += salary.0;
                workplace_tax += salary.1;
            }

            // Rent is paid per citizen. Failure does not remove the citizen, but it
            // records rent stress so the happiness system can lower future morale.
            if citizen.money >= rent {
                citizen.money -= rent;
                rent_income += rent;
                citizen.rent_stress = 0;
            } else {
                rent_failures += 1;
                citizen.rent_stress = 1;
                citizen.happiness -= MISSED_RENT_HAPPINESS_PENALTY;
            }

            // Shopping is optional and capacity-limited by commercial buildings.
            // A shopping slot is consumed only when the citizen can actually buy,
            // so poor citizens do not block later citizens from shopping.
            if citizen.money >= shopping.cost && shopping.sales_tax > 0 {
                if let Some(slot_index) = shopping.slot_index {
                    shopping_slots.remove(slot_index);
                }
                citizen.money -= shopping.cost;
                commercial_sales_tax += shopping.sales_tax;
                shoppers_served += 1;
                citizen.happiness += shopping.happiness_bonus;
                if shopping.local_goods {
                    local_goods_sold += 1;
                    sold_local_good_from = Some(shopping.commercial);
                } else {
                    imported_goods_sold += 1;
                }
            } else {
                citizen.happiness -= MISSED_SHOPPING_HAPPINESS_PENALTY;
            }

            citizen.happiness = citizen.happiness.clamp(0, 100);
        }
        if let Some(commercial) = sold_local_good_from {
            consume_local_good(world, commercial);
        }
    }

    // City money changes only through tax/rent income minus public maintenance.
    // Salaries and shopping costs are tracked on citizens but are not direct
    // expenses for the city budget in this simplified model.
    let manufacturing_tax = goods_flow.manufacturing_tax;
    let export_tax = goods_flow.export_tax;
    let net = workplace_tax + rent_income + commercial_sales_tax + manufacturing_tax + export_tax
        - maintenance_cost;
    world.resources.money += net;

    EconomyBreakdown {
        salaries_paid,
        workplace_tax,
        rent_income,
        commercial_sales_tax,
        shoppers_served,
        local_goods_produced: goods_flow.local_goods_produced,
        local_goods_stored: goods_flow.local_goods_stored,
        local_goods_sold,
        imported_goods_sold,
        exported_goods: goods_flow.exported_goods,
        manufacturing_tax,
        export_tax,
        rent_failures,
        maintenance_cost,
        net,
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct GoodsFlow {
    local_goods_produced: i32,
    local_goods_stored: i32,
    exported_goods: i32,
    manufacturing_tax: i32,
    export_tax: i32,
}

#[derive(Debug, Clone, Copy)]
struct ShoppingOffer {
    commercial: crate::core::entity::Entity,
    cost: i32,
    sales_tax: i32,
    happiness_bonus: i32,
    local_goods: bool,
    slot_index: Option<usize>,
}

fn assign_workplaces(world: &mut World) {
    let mut workplaces = workplace_slots(world);
    // Position order makes assignment stable across runs and independent of
    // HashMap iteration order.
    workplaces.sort_by_key(|entity| {
        world
            .positions
            .get(entity)
            .map(|position| (position.y, position.x, entity.0))
            .unwrap_or((usize::MAX, usize::MAX, entity.0))
    });

    let mut citizen_entities: Vec<_> = world.citizens.keys().copied().collect();
    citizen_entities.sort_by_key(|citizen| citizen.0);

    // Citizens take the nearest reachable job slot. Ties use map order through
    // the pre-sorted slot list, keeping assignment deterministic.
    for citizen_entity in citizen_entities {
        let home = world
            .citizens
            .get(&citizen_entity)
            .map(|citizen| citizen.home);
        let workplace_index = home.and_then(|home| nearest_slot_index(world, home, &workplaces));
        let workplace = workplace_index.map(|index| workplaces.remove(index));
        if let Some(citizen) = world.citizens.get_mut(&citizen_entity) {
            citizen.workplace = workplace;
        }
    }
}

fn workplace_slots(world: &World) -> Vec<crate::core::entity::Entity> {
    let mut slots = Vec::new();
    for (entity, building) in &world.buildings {
        if !is_effective_workplace(world, *entity) {
            continue;
        }

        for _ in 0..building.kind.jobs().max(0) {
            slots.push(*entity);
        }
    }
    slots
}

fn commercial_shopping_slot_entities(world: &World) -> Vec<crate::core::entity::Entity> {
    let mut slots = Vec::new();
    let mut commercials: Vec<_> = world
        .buildings
        .iter()
        .filter(|(entity, building)| {
            building.kind == BuildingKind::Commercial && is_effective_workplace(world, **entity)
        })
        .map(|(entity, _)| *entity)
        .collect();
    commercials.sort_by_key(|entity| {
        world
            .positions
            .get(entity)
            .map(|position| (position.y, position.x, entity.0))
            .unwrap_or((usize::MAX, usize::MAX, entity.0))
    });

    for commercial in commercials {
        // Shopping capacity is separate from job count. A small fixed capacity
        // keeps the model readable while still letting disconnected shops matter.
        for _ in 0..COMMERCIAL_SHOPPER_CAPACITY {
            slots.push(commercial);
        }
    }
    slots
}

fn distribute_local_goods(world: &mut World) -> GoodsFlow {
    let mut local_goods_produced = 0;
    let mut local_goods_stored = 0;
    let mut exported_goods = 0;
    let mut manufacturing_tax = 0;
    let mut export_tax = 0;
    let commercials = productive_commercials(world);

    for industrial in productive_industrials(world) {
        let produced = industrial_goods_production(world, industrial);
        local_goods_produced += produced;
        let mut remaining_goods = produced;

        for commercial in nearest_commercials_for_goods(world, industrial, &commercials) {
            let capacity = commercial_goods_capacity_for_entity(world, commercial);
            let stored = commercial_goods_stored(world, commercial);
            let free_capacity = (capacity - stored).max(0);
            let supplied = free_capacity.min(remaining_goods);
            if supplied <= 0 {
                continue;
            }
            add_commercial_goods(world, commercial, supplied);
            remaining_goods -= supplied;
            local_goods_stored += supplied;
            let distance =
                road_network_analysis::distance_between_buildings(world, industrial, commercial);
            manufacturing_tax += supplied * margin_per_good(MANUFACTURING_TAX_PER_GOOD, distance);
            if remaining_goods == 0 {
                break;
            }
        }

        if remaining_goods > 0 {
            exported_goods += remaining_goods;
            let distance =
                road_network_analysis::access_for(world, industrial).import_export_distance;
            export_tax += remaining_goods * margin_per_good(EXPORT_TAX_PER_GOOD, distance);
            manufacturing_tax +=
                remaining_goods * margin_per_good(MANUFACTURING_TAX_PER_GOOD, distance);
        }
    }

    GoodsFlow {
        local_goods_produced,
        local_goods_stored,
        exported_goods,
        manufacturing_tax,
        export_tax,
    }
}

fn productive_industrials(world: &World) -> Vec<crate::core::entity::Entity> {
    let mut industrials: Vec<_> = world
        .buildings
        .iter()
        .filter(|(entity, building)| {
            building.kind == BuildingKind::Industrial && is_effective_workplace(world, **entity)
        })
        .map(|(entity, _)| *entity)
        .collect();
    sort_entities_by_position(world, &mut industrials);
    industrials
}

fn productive_commercials(world: &World) -> Vec<crate::core::entity::Entity> {
    let mut commercials: Vec<_> = world
        .buildings
        .iter()
        .filter(|(entity, building)| {
            building.kind == BuildingKind::Commercial && is_effective_workplace(world, **entity)
        })
        .map(|(entity, _)| *entity)
        .collect();
    sort_entities_by_position(world, &mut commercials);
    commercials
}

fn sort_entities_by_position(world: &World, entities: &mut [crate::core::entity::Entity]) {
    entities.sort_by_key(|entity| {
        world
            .positions
            .get(entity)
            .map(|position| (position.y, position.x, entity.0))
            .unwrap_or((usize::MAX, usize::MAX, entity.0))
    });
}

fn salary_for_workplace(world: &World, workplace: crate::core::entity::Entity) -> Option<i32> {
    let building = world.buildings.get(&workplace)?;
    if !is_effective_workplace(world, workplace) {
        return None;
    }

    match building.kind {
        BuildingKind::Commercial => Some(COMMERCIAL_SALARY),
        BuildingKind::Industrial => Some(INDUSTRIAL_SALARY),
        _ => None,
    }
}

fn workplace_tax_for_workplace(world: &World, workplace: crate::core::entity::Entity) -> i32 {
    let Some(building) = world.buildings.get(&workplace) else {
        return 0;
    };
    let level = i32::from(building.level.max(1));

    match building.kind {
        BuildingKind::Commercial => level,
        BuildingKind::Industrial => level + 1,
        _ => 0,
    }
}

pub(crate) fn rent_per_citizen(world: &World, home: crate::core::entity::Entity) -> i32 {
    let Some(position) = world.positions.get(&home) else {
        return 1;
    };
    let land_value = world.local_effects.get(position.x, position.y).land_value;
    let level = world
        .buildings
        .get(&home)
        .map(|building| i32::from(building.level.max(1)))
        .unwrap_or(1);

    // Higher-value neighborhoods and upgraded homes charge more rent. Integer
    // division keeps values small and deterministic.
    1 + land_value / 4 + (level - 1)
}

pub(crate) fn commercial_sales_tax_for_purchase(
    world: &World,
    commercial: crate::core::entity::Entity,
) -> i32 {
    let Some(position) = world.positions.get(&commercial) else {
        return 1;
    };
    let land_value = world.local_effects.get(position.x, position.y).land_value;
    let level = world
        .buildings
        .get(&commercial)
        .map(|building| i32::from(building.level.max(1)))
        .unwrap_or(1);

    // Better commercial locations and upgraded commercial buildings collect a
    // little more tax per shopper.
    1 + land_value / 4 + (level - 1)
}

pub(crate) fn industrial_goods_production(
    world: &World,
    industrial: crate::core::entity::Entity,
) -> i32 {
    let Some(building) = world.buildings.get(&industrial) else {
        return 0;
    };
    if building.kind != BuildingKind::Industrial {
        return 0;
    }

    INDUSTRIAL_GOODS_PRODUCTION
        + (i32::from(building.level.max(1)) - 1) * INDUSTRIAL_GOODS_PRODUCTION_PER_EXTRA_LEVEL
}

pub(crate) fn commercial_goods_capacity(level: u8) -> i32 {
    COMMERCIAL_GOODS_STORAGE
        + (i32::from(level.max(1)) - 1) * COMMERCIAL_GOODS_STORAGE_PER_EXTRA_LEVEL
}

pub(crate) fn commercial_goods_stored(
    world: &World,
    commercial: crate::core::entity::Entity,
) -> i32 {
    world
        .buildings
        .get(&commercial)
        .and_then(|building| match building.data {
            BuildingData::Commercial { local_goods_stored } => Some(local_goods_stored),
            BuildingData::None => None,
        })
        .unwrap_or(0)
}

pub(crate) fn commercial_goods_capacity_for_entity(
    world: &World,
    commercial: crate::core::entity::Entity,
) -> i32 {
    world
        .buildings
        .get(&commercial)
        .map(|building| commercial_goods_capacity(building.level))
        .unwrap_or(0)
}

fn next_shopping_offer(
    world: &World,
    home: crate::core::entity::Entity,
    shopping_slots: &[crate::core::entity::Entity],
) -> ShoppingOffer {
    let slot_index = nearest_slot_index(world, home, shopping_slots);
    let commercial = slot_index.and_then(|index| shopping_slots.get(index).copied());
    let Some(commercial) = commercial else {
        return ShoppingOffer {
            commercial: crate::core::entity::Entity(u32::MAX),
            cost: 0,
            sales_tax: 0,
            happiness_bonus: 0,
            local_goods: false,
            slot_index: None,
        };
    };
    let local_goods = commercial_goods_stored(world, commercial) > 0;
    let distance = road_network_analysis::distance_between_buildings(world, home, commercial);

    ShoppingOffer {
        commercial,
        cost: if local_goods {
            LOCAL_SHOPPING_COST
        } else {
            IMPORTED_SHOPPING_COST
        },
        sales_tax: commercial_sales_tax_for_purchase(world, commercial),
        happiness_bonus: (if local_goods {
            LOCAL_SHOPPING_HAPPINESS_BONUS
        } else {
            IMPORTED_SHOPPING_HAPPINESS_BONUS
        } + road_network_analysis::shopping_happiness_modifier(distance))
        .max(0),
        local_goods,
        slot_index,
    }
}

fn nearest_slot_index(
    world: &World,
    from: crate::core::entity::Entity,
    slots: &[crate::core::entity::Entity],
) -> Option<usize> {
    slots
        .iter()
        .enumerate()
        .filter_map(|(index, slot)| {
            road_network_analysis::distance_between_buildings(world, from, *slot)
                .map(|distance| (index, distance))
        })
        .min_by_key(|(index, distance)| (*distance, *index))
        .map(|(index, _distance)| index)
}

fn nearest_commercials_for_goods(
    world: &World,
    industrial: crate::core::entity::Entity,
    commercials: &[crate::core::entity::Entity],
) -> Vec<crate::core::entity::Entity> {
    let mut ordered: Vec<_> = commercials
        .iter()
        .copied()
        .filter_map(|commercial| {
            road_network_analysis::distance_between_buildings(world, industrial, commercial)
                .map(|distance| (commercial, distance))
        })
        .collect();
    ordered.sort_by_key(|(commercial, distance)| {
        let position_key = world
            .positions
            .get(commercial)
            .map(|position| (position.y, position.x, commercial.0))
            .unwrap_or((usize::MAX, usize::MAX, commercial.0));
        (*distance, position_key)
    });
    ordered
        .into_iter()
        .map(|(commercial, _distance)| commercial)
        .collect()
}

fn margin_per_good(base: i32, distance: Option<u32>) -> i32 {
    (base - road_network_analysis::route_margin_penalty(distance)).max(0)
}

fn consume_local_good(world: &mut World, commercial: crate::core::entity::Entity) {
    if let Some(building) = world.buildings.get_mut(&commercial) {
        if let BuildingData::Commercial { local_goods_stored } = &mut building.data {
            if *local_goods_stored > 0 {
                *local_goods_stored -= 1;
            }
        }
    }
}

fn add_commercial_goods(world: &mut World, commercial: crate::core::entity::Entity, amount: i32) {
    if amount <= 0 {
        return;
    }
    if let Some(building) = world.buildings.get_mut(&commercial) {
        let capacity = commercial_goods_capacity(building.level);
        if let BuildingData::Commercial { local_goods_stored } = &mut building.data {
            *local_goods_stored = (*local_goods_stored + amount).clamp(0, capacity);
        }
    }
}

fn ensure_commercial_building_data(world: &mut World) {
    let commercials: Vec<_> = world
        .buildings
        .iter()
        .filter_map(|(entity, building)| {
            (building.kind == BuildingKind::Commercial).then_some((*entity, building.level))
        })
        .collect();

    for (commercial, level) in commercials {
        let capacity = commercial_goods_capacity(level);
        if let Some(building) = world.buildings.get_mut(&commercial) {
            match &mut building.data {
                BuildingData::Commercial { local_goods_stored } => {
                    *local_goods_stored = (*local_goods_stored).clamp(0, capacity);
                }
                BuildingData::None => {
                    building.data = BuildingData::Commercial {
                        local_goods_stored: 0,
                    };
                }
            }
        }
    }
}

fn is_effective_workplace(world: &World, entity: crate::core::entity::Entity) -> bool {
    let powered = world
        .power_consumers
        .get(&entity)
        .map(|consumer| consumer.powered)
        .unwrap_or(false);
    powered && road_connectivity::is_road_connected(world, entity)
}

fn maintenance_cost(world: &World) -> i32 {
    world
        .buildings
        .values()
        .map(|building| maintenance_for_building(building.kind, building.level))
        .sum()
}

pub(crate) fn maintenance_for_building(kind: BuildingKind, level: u8) -> i32 {
    // Level starts at 1. Each upgrade adds one maintenance on top of the base
    // building upkeep so upgrades have an ongoing city-budget tradeoff.
    kind.maintenance_cost() + (i32::from(level.max(1)) - 1)
}
