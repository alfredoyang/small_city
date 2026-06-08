//! Economy system for citizen salary, rent, shopping, taxes, and building maintenance.
//!
//! The city budget is intentionally simple: each tick, citizens earn salary from
//! productive workplaces, pay rent to the city, optionally shop at commercial
//! buildings, and buildings charge maintenance. The system returns a structured
//! `EconomyBreakdown` so UI layers can explain the money change without reading
//! ECS internals.

use crate::core::components::{BuildingData, BusinessFinance, Citizen};
use crate::core::entity::Entity;
use crate::core::resource_registry::{JobAssignment, ResourceRegistry};
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
const LOCAL_SHOPPING_DECAY_RECOVERY: i32 = 4;
const IMPORTED_SHOPPING_DECAY_RECOVERY: i32 = 2;
const MISSED_RENT_HAPPINESS_PENALTY: i32 = 5;
const MISSED_SHOPPING_HAPPINESS_PENALTY: i32 = 1;
const INDUSTRIAL_GOODS_PRODUCTION: i32 = 4;
const INDUSTRIAL_GOODS_PRODUCTION_PER_EXTRA_LEVEL: i32 = 2;
const COMMERCIAL_GOODS_STORAGE: i32 = 8;
const COMMERCIAL_GOODS_STORAGE_PER_EXTRA_LEVEL: i32 = 4;
const MANUFACTURING_TAX_PER_GOOD: i32 = 1;
const EXPORT_TAX_PER_GOOD: i32 = 1;
const COMMERCIAL_LOCAL_GOOD_PROFIT: i32 = 2;
const COMMERCIAL_IMPORTED_GOOD_PROFIT: i32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
    // Workplaces and shops are recalculated every tick from powered,
    // road-connected buildings. This keeps employment and shopping deterministic
    // after build, bulldoze, replace, upgrade, save, or load.
    ensure_business_building_data(world);
    let registry = ResourceRegistry::for_jobs(world);
    let job_resolution = registry.resolve_local_jobs();
    let index = EconomyIndex::from_world(world, &registry);
    reset_business_periods(world, &index.business_entities);
    apply_workplace_assignments(world, &job_resolution.assignments);

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
    let goods_flow = distribute_local_goods(
        world,
        &index.productive_industrials,
        &index.productive_commercials,
    );

    let mut shopping_slots = index.shopping_slots.clone();
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
        let (home, workplace) = world
            .citizens
            .get(&citizen_entity)
            .map(|citizen| (citizen.home, citizen.workplace))
            .unwrap_or((Entity(u32::MAX), None));
        let salary = workplace
            .and_then(|workplace| {
                salary_for_workplace(world, workplace)
                    .map(|salary| (salary, workplace_tax_for_workplace(world, workplace)))
            })
            .unwrap_or((0, 0));
        let rent = rent_per_citizen(world, home);
        let shopping = next_shopping_offer(world, home, &shopping_slots);

        let mut sold_local_good_from = None;
        let mut business_profit_from_sale = None;
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
                    recover_happiness_from_shopping(citizen, LOCAL_SHOPPING_DECAY_RECOVERY);
                    local_goods_sold += 1;
                    sold_local_good_from = Some(shopping.commercial);
                    business_profit_from_sale =
                        Some((shopping.commercial, COMMERCIAL_LOCAL_GOOD_PROFIT));
                } else {
                    recover_happiness_from_shopping(citizen, IMPORTED_SHOPPING_DECAY_RECOVERY);
                    imported_goods_sold += 1;
                    business_profit_from_sale =
                        Some((shopping.commercial, COMMERCIAL_IMPORTED_GOOD_PROFIT));
                }
            } else {
                citizen.happiness -= MISSED_SHOPPING_HAPPINESS_PENALTY;
            }

            citizen.happiness = citizen.happiness.clamp(0, 100);
        }
        if let Some(commercial) = sold_local_good_from {
            consume_local_good(world, commercial);
        }
        if let Some((commercial, profit)) = business_profit_from_sale {
            record_business_profit(world, commercial, profit);
        }
    }

    finalize_business_periods(world, &index.business_entities);

    // City money changes only through tax/rent income minus public maintenance.
    // Salaries and shopping costs are tracked on citizens but are not direct
    // expenses for the city budget in this simplified model.
    let manufacturing_tax = goods_flow.manufacturing_tax;
    let export_tax = goods_flow.export_tax;
    let net = workplace_tax + rent_income + commercial_sales_tax + manufacturing_tax + export_tax
        - index.maintenance_cost;
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
        maintenance_cost: index.maintenance_cost,
        net,
    }
}

#[derive(Debug, Clone, Default)]
struct EconomyIndex {
    business_entities: Vec<Entity>,
    productive_commercials: Vec<Entity>,
    productive_industrials: Vec<Entity>,
    workplace_slots: Vec<Entity>,
    shopping_slots: Vec<Entity>,
    maintenance_cost: i32,
}

impl EconomyIndex {
    fn from_world(world: &World, registry: &ResourceRegistry) -> Self {
        let mut index = Self::default();
        let mut shopping_commercials = Vec::new();
        index.workplace_slots = registry.local_job_slots().to_vec();

        for (entity, building) in &world.buildings {
            index.maintenance_cost += maintenance_for_building(building.kind, building.level);

            if matches!(
                building.kind,
                BuildingKind::Commercial | BuildingKind::Industrial
            ) {
                index.business_entities.push(*entity);
            }

            if !is_effective_workplace(world, *entity) {
                continue;
            }

            match building.kind {
                BuildingKind::Commercial => {
                    index.productive_commercials.push(*entity);
                    shopping_commercials.push(*entity);
                }
                BuildingKind::Industrial => {
                    index.productive_industrials.push(*entity);
                }
                _ => {}
            }
        }

        sort_entities_by_position(world, &mut index.business_entities);
        sort_entities_by_position(world, &mut index.productive_commercials);
        sort_entities_by_position(world, &mut index.productive_industrials);
        sort_entities_by_position(world, &mut index.workplace_slots);
        sort_entities_by_position(world, &mut shopping_commercials);

        for commercial in shopping_commercials {
            // Shopping capacity is separate from job count. A small fixed capacity
            // keeps the model readable while still letting disconnected shops matter.
            for _ in 0..commercial_shopper_capacity(world, commercial) {
                index.shopping_slots.push(commercial);
            }
        }

        index
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
    commercial: Entity,
    cost: i32,
    sales_tax: i32,
    happiness_bonus: i32,
    local_goods: bool,
    slot_index: Option<usize>,
}

fn apply_workplace_assignments(world: &mut World, assignments: &[JobAssignment]) {
    for assignment in assignments {
        if let Some(citizen) = world.citizens.get_mut(&assignment.citizen) {
            citizen.workplace = assignment.workplace;
        }
    }
}

fn distribute_local_goods(
    world: &mut World,
    productive_industrials: &[Entity],
    productive_commercials: &[Entity],
) -> GoodsFlow {
    let mut local_goods_produced = 0;
    let mut local_goods_stored = 0;
    let mut exported_goods = 0;
    let mut manufacturing_tax = 0;
    let mut export_tax = 0;
    for industrial in productive_industrials {
        let mut industrial_profit = 0;
        let produced = industrial_goods_production(world, *industrial);
        local_goods_produced += produced;
        let mut remaining_goods = produced;

        for commercial in nearest_commercials_for_goods(world, *industrial, productive_commercials)
        {
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
                road_network_analysis::distance_between_buildings(world, *industrial, commercial);
            let margin = supplied * margin_per_good(MANUFACTURING_TAX_PER_GOOD, distance);
            manufacturing_tax += margin;
            industrial_profit += margin;
            if remaining_goods == 0 {
                break;
            }
        }

        if remaining_goods > 0 {
            exported_goods += remaining_goods;
            let distance =
                road_network_analysis::access_for(world, *industrial).import_export_distance;
            let export_margin = remaining_goods * margin_per_good(EXPORT_TAX_PER_GOOD, distance);
            let manufacturing_margin =
                remaining_goods * margin_per_good(MANUFACTURING_TAX_PER_GOOD, distance);
            export_tax += export_margin;
            manufacturing_tax += manufacturing_margin;
            industrial_profit += export_margin + manufacturing_margin;
        }
        record_business_profit(world, *industrial, industrial_profit);
    }

    GoodsFlow {
        local_goods_produced,
        local_goods_stored,
        exported_goods,
        manufacturing_tax,
        export_tax,
    }
}

fn sort_entities_by_position(world: &World, entities: &mut [Entity]) {
    entities.sort_by_key(|entity| {
        world
            .positions
            .get(entity)
            .map(|position| (position.y, position.x, entity.0))
            .unwrap_or((usize::MAX, usize::MAX, entity.0))
    });
}

fn commercial_shopper_capacity(world: &World, commercial: Entity) -> i32 {
    world
        .buildings
        .get(&commercial)
        .map(|building| COMMERCIAL_SHOPPER_CAPACITY + (i32::from(building.level.max(1)) - 1) * 2)
        .unwrap_or(COMMERCIAL_SHOPPER_CAPACITY)
}

fn salary_for_workplace(world: &World, workplace: Entity) -> Option<i32> {
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

fn workplace_tax_for_workplace(world: &World, workplace: Entity) -> i32 {
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

pub(crate) fn rent_per_citizen(world: &World, home: Entity) -> i32 {
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

pub(crate) fn commercial_sales_tax_for_purchase(world: &World, commercial: Entity) -> i32 {
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

pub(crate) fn industrial_goods_production(world: &World, industrial: Entity) -> i32 {
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

pub(crate) fn commercial_goods_stored(world: &World, commercial: Entity) -> i32 {
    world
        .buildings
        .get(&commercial)
        .and_then(|building| match building.data {
            BuildingData::Commercial {
                local_goods_stored, ..
            } => Some(local_goods_stored),
            BuildingData::Industrial { .. } | BuildingData::None => None,
        })
        .unwrap_or(0)
}

pub(crate) fn commercial_goods_capacity_for_entity(world: &World, commercial: Entity) -> i32 {
    world
        .buildings
        .get(&commercial)
        .map(|building| commercial_goods_capacity(building.level))
        .unwrap_or(0)
}

fn next_shopping_offer(world: &World, home: Entity, shopping_slots: &[Entity]) -> ShoppingOffer {
    let slot_index = nearest_slot_index(world, home, shopping_slots);
    let commercial = slot_index.and_then(|index| shopping_slots.get(index).copied());
    let Some(commercial) = commercial else {
        return ShoppingOffer {
            commercial: Entity(u32::MAX),
            cost: 0,
            sales_tax: 0,
            happiness_bonus: 0,
            local_goods: false,
            slot_index: None,
        };
    };
    let local_goods = commercial_goods_stored(world, commercial) > 0;
    let distance = road_network_analysis::distance_between_buildings(world, home, commercial);
    let import_distance =
        road_network_analysis::access_for(world, commercial).import_export_distance;

    ShoppingOffer {
        commercial,
        cost: if local_goods {
            LOCAL_SHOPPING_COST
        } else {
            IMPORTED_SHOPPING_COST + road_network_analysis::import_cost_penalty(import_distance)
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

fn nearest_slot_index(world: &World, from: Entity, slots: &[Entity]) -> Option<usize> {
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
    industrial: Entity,
    commercials: &[Entity],
) -> Vec<Entity> {
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

fn consume_local_good(world: &mut World, commercial: Entity) {
    if let Some(building) = world.buildings.get_mut(&commercial) {
        if let BuildingData::Commercial {
            local_goods_stored, ..
        } = &mut building.data
        {
            if *local_goods_stored > 0 {
                *local_goods_stored -= 1;
            }
        }
    }
}

fn recover_happiness_from_shopping(citizen: &mut Citizen, amount: i32) {
    let recovered = citizen.happiness_decay.min(amount.max(0));
    citizen.happiness_decay -= recovered;
    citizen.happiness += recovered;
}

fn add_commercial_goods(world: &mut World, commercial: Entity, amount: i32) {
    if amount <= 0 {
        return;
    }
    if let Some(building) = world.buildings.get_mut(&commercial) {
        let capacity = commercial_goods_capacity(building.level);
        if let BuildingData::Commercial {
            local_goods_stored, ..
        } = &mut building.data
        {
            *local_goods_stored = (*local_goods_stored + amount).clamp(0, capacity);
        }
    }
}

pub(crate) fn ensure_business_building_data(world: &mut World) {
    let businesses: Vec<_> = world
        .buildings
        .iter()
        .filter_map(|(entity, building)| {
            matches!(
                building.kind,
                BuildingKind::Commercial | BuildingKind::Industrial
            )
            .then_some((*entity, building.kind, building.level))
        })
        .collect();

    for (entity, kind, level) in businesses {
        if let Some(building) = world.buildings.get_mut(&entity) {
            match (kind, &mut building.data) {
                (
                    BuildingKind::Commercial,
                    BuildingData::Commercial {
                        local_goods_stored, ..
                    },
                ) => {
                    *local_goods_stored =
                        (*local_goods_stored).clamp(0, commercial_goods_capacity(level));
                }
                (BuildingKind::Commercial, _) => {
                    building.data = BuildingData::Commercial {
                        local_goods_stored: 0,
                        business: BusinessFinance::default(),
                    };
                }
                (BuildingKind::Industrial, BuildingData::Industrial { .. }) => {}
                (BuildingKind::Industrial, _) => {
                    building.data = BuildingData::Industrial {
                        business: BusinessFinance::default(),
                    };
                }
                _ => {}
            }
        }
    }
}

pub(crate) fn business_finance(world: &World, entity: Entity) -> Option<BusinessFinance> {
    world
        .buildings
        .get(&entity)
        .and_then(|building| match building.data {
            BuildingData::Commercial { business, .. } | BuildingData::Industrial { business } => {
                Some(business)
            }
            BuildingData::None => None,
        })
}

pub(crate) fn business_cash(world: &World, entity: Entity) -> i32 {
    business_finance(world, entity)
        .map(|business| business.business_cash)
        .unwrap_or(0)
}

pub(crate) fn recent_business_profit(world: &World, entity: Entity) -> i32 {
    business_finance(world, entity)
        .map(|business| business.last_period_profit)
        .unwrap_or(0)
}

pub(crate) fn spend_business_cash(world: &mut World, entity: Entity, amount: i32) {
    if amount <= 0 {
        return;
    }
    if let Some(business) = business_finance_mut(world, entity) {
        business.business_cash = (business.business_cash - amount).max(0);
    }
}

fn record_business_profit(world: &mut World, entity: Entity, gross: i32) {
    if let Some(business) = business_finance_mut(world, entity) {
        business.last_period_profit += gross;
    }
}

fn reset_business_periods(world: &mut World, business_entities: &[Entity]) {
    for entity in business_entities {
        if let Some(business) = business_finance_mut(world, *entity) {
            business.last_period_profit = 0;
        }
    }
}

fn finalize_business_periods(world: &mut World, business_entities: &[Entity]) {
    for entity in business_entities {
        let maintenance = world
            .buildings
            .get(entity)
            .map(|building| maintenance_for_building(building.kind, building.level))
            .unwrap_or(0);
        if let Some(business) = business_finance_mut(world, *entity) {
            let profit = business.last_period_profit - maintenance;
            business.last_period_profit = profit;
            if profit > 0 {
                business.business_cash += profit;
                business.lifetime_profit += profit;
                business.days_profitable += 1;
            } else {
                business.days_profitable = 0;
            }
        }
    }
}

fn business_finance_mut(world: &mut World, entity: Entity) -> Option<&mut BusinessFinance> {
    world
        .buildings
        .get_mut(&entity)
        .and_then(|building| match &mut building.data {
            BuildingData::Commercial { business, .. } | BuildingData::Industrial { business } => {
                Some(business)
            }
            BuildingData::None => None,
        })
}

fn is_effective_workplace(world: &World, entity: Entity) -> bool {
    let powered = world
        .power_consumers
        .get(&entity)
        .map(|consumer| consumer.powered)
        .unwrap_or(false);
    powered && road_connectivity::is_road_connected(world, entity)
}

pub(crate) fn maintenance_for_building(kind: BuildingKind, level: u8) -> i32 {
    // Level starts at 1. Each upgrade adds one maintenance on top of the base
    // building upkeep so upgrades have an ongoing city-budget tradeoff.
    kind.maintenance_cost() + (i32::from(level.max(1)) - 1)
}

#[cfg(test)]
mod tests {
    use super::{EconomyIndex, run};
    use crate::core::entity::Entity;
    use crate::core::resource_registry::ResourceRegistry;
    use crate::core::systems::{citizens, placement, road_network_analysis};
    use crate::core::world::World;
    use crate::interface::input::BuildingKind;

    #[test]
    fn economy_index_collects_daily_building_roles_once() {
        let mut world = World::new(4, 3);
        placement::place_building(&mut world, 1, 0, BuildingKind::Commercial);
        placement::place_building(&mut world, 2, 0, BuildingKind::Industrial);
        placement::place_building(&mut world, 1, 1, BuildingKind::Road);
        placement::place_building(&mut world, 2, 1, BuildingKind::Road);

        let commercial = world.grid.get(1, 0).expect("commercial entity");
        let industrial = world.grid.get(2, 0).expect("industrial entity");
        world.power_consumers.get_mut(&commercial).unwrap().powered = true;
        world.power_consumers.get_mut(&industrial).unwrap().powered = true;

        let registry = ResourceRegistry::for_jobs(&world);
        let index = EconomyIndex::from_world(&world, &registry);

        assert_eq!(index.business_entities, vec![commercial, industrial]);
        assert_eq!(index.productive_commercials, vec![commercial]);
        assert_eq!(index.productive_industrials, vec![industrial]);
        assert_eq!(index.workplace_slots.len(), 5);
        assert_eq!(index.shopping_slots, vec![commercial; 4]);
        assert_eq!(index.maintenance_cost, 2);
    }

    #[test]
    fn successful_shopping_reduces_existing_happiness_decay() {
        let (mut world, citizen) = shopping_recovery_world(false, true);
        let before_happiness = citizen_happiness(&world, citizen);

        let economy = run(&mut world);

        assert_eq!(economy.shoppers_served, 1);
        assert_eq!(economy.imported_goods_sold, 1);
        assert_eq!(citizen_decay(&world, citizen), 8);
        assert_eq!(citizen_happiness(&world, citizen), before_happiness + 4);
    }

    #[test]
    fn local_goods_recover_more_happiness_decay_than_imported_goods() {
        let (mut local_world, local_citizen) = shopping_recovery_world(true, true);
        let (mut imported_world, imported_citizen) = shopping_recovery_world(false, true);

        let local_economy = run(&mut local_world);
        let imported_economy = run(&mut imported_world);

        assert_eq!(local_economy.local_goods_sold, 1);
        assert_eq!(imported_economy.imported_goods_sold, 1);
        assert_eq!(citizen_decay(&local_world, local_citizen), 6);
        assert_eq!(citizen_decay(&imported_world, imported_citizen), 8);
    }

    #[test]
    fn disconnected_citizen_does_not_recover_happiness_decay() {
        let (mut world, citizen) = shopping_recovery_world(false, false);

        let economy = run(&mut world);

        assert_eq!(economy.shoppers_served, 0);
        assert_eq!(citizen_decay(&world, citizen), 10);
    }

    #[test]
    fn repeated_local_shopping_offsets_daily_decay_without_going_below_zero() {
        let (mut world, citizen) = shopping_recovery_world(true, true);
        set_citizen_decay(&mut world, citizen, 0);

        for _ in 0..5 {
            citizens::apply_daily_happiness_decay(&mut world);
            citizens::update_happiness(&mut world);
            let economy = run(&mut world);
            assert_eq!(economy.shoppers_served, 1);
        }

        assert_eq!(citizen_decay(&world, citizen), 0);
    }

    fn shopping_recovery_world(with_local_goods: bool, connected_shop: bool) -> (World, Entity) {
        let mut world = World::new(5, 3);
        placement::place_building(&mut world, 0, 0, BuildingKind::Residential);
        placement::place_building(&mut world, 1, 0, BuildingKind::Commercial);
        if with_local_goods {
            placement::place_building(&mut world, 2, 0, BuildingKind::Industrial);
        }
        placement::place_building(&mut world, 0, 1, BuildingKind::Road);
        if connected_shop {
            placement::place_building(&mut world, 1, 1, BuildingKind::Road);
            if with_local_goods {
                placement::place_building(&mut world, 2, 1, BuildingKind::Road);
            }
        }

        let residential = world.grid.get(0, 0).expect("residential entity");
        let commercial = world.grid.get(1, 0).expect("commercial entity");
        citizens::spawn_for_home(&mut world, residential, 1);
        let citizen = *world.citizens.keys().next().expect("citizen");
        if let Some(citizen) = world.citizens.get_mut(&citizen) {
            citizen.money = 10;
            citizen.happiness_decay = 10;
        }
        world.power_consumers.get_mut(&commercial).unwrap().powered = true;
        if with_local_goods {
            let industrial = world.grid.get(2, 0).expect("industrial entity");
            world.power_consumers.get_mut(&industrial).unwrap().powered = true;
        }
        road_network_analysis::run(&mut world);
        (world, citizen)
    }

    fn set_citizen_decay(world: &mut World, citizen: Entity, decay: i32) {
        let citizen = world.citizens.get_mut(&citizen).expect("citizen");
        citizen.happiness_decay = decay;
    }

    fn citizen_decay(world: &World, citizen: Entity) -> i32 {
        world
            .citizens
            .get(&citizen)
            .expect("citizen")
            .happiness_decay
    }

    fn citizen_happiness(world: &World, citizen: Entity) -> i32 {
        world.citizens.get(&citizen).expect("citizen").happiness
    }
}
