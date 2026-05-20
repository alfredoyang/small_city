//! Economy system for citizen salary, rent, shopping, taxes, and building maintenance.
//!
//! The city budget is intentionally simple: each tick, citizens earn salary from
//! productive workplaces, pay rent to the city, optionally shop at commercial
//! buildings, and buildings charge maintenance. The system returns a structured
//! `EconomyBreakdown` so UI layers can explain the money change without reading
//! ECS internals.

use crate::core::systems::road_connectivity;
use crate::core::world::World;
use crate::interface::input::BuildingKind;

const COMMERCIAL_SHOPPER_CAPACITY: i32 = 4;
const COMMERCIAL_SALARY: i32 = 3;
const INDUSTRIAL_SALARY: i32 = 4;
const SHOPPING_COST: i32 = 1;
const SHOPPING_HAPPINESS_BONUS: i32 = 3;
const MISSED_RENT_HAPPINESS_PENALTY: i32 = 5;
const MISSED_SHOPPING_HAPPINESS_PENALTY: i32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EconomyBreakdown {
    pub salaries_paid: i32,
    pub workplace_tax: i32,
    pub rent_income: i32,
    pub commercial_sales_tax: i32,
    pub shoppers_served: i32,
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
    assign_workplaces(world);

    let mut shopping_slots = commercial_shopping_slot_entities(world);
    let mut salaries_paid = 0;
    let mut workplace_tax = 0;
    let mut rent_income = 0;
    let mut commercial_sales_tax = 0;
    let mut shoppers_served = 0;
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
        let shopping_tax = next_commercial_sales_tax(world, &shopping_slots);

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
        if citizen.money >= SHOPPING_COST && shopping_tax > 0 {
            shopping_slots.remove(0);
            citizen.money -= SHOPPING_COST;
            commercial_sales_tax += shopping_tax;
            shoppers_served += 1;
            citizen.happiness += SHOPPING_HAPPINESS_BONUS;
        } else {
            citizen.happiness -= MISSED_SHOPPING_HAPPINESS_PENALTY;
        }

        citizen.happiness = citizen.happiness.clamp(0, 100);
    }

    // City money changes only through tax/rent income minus public maintenance.
    // Salaries and shopping costs are tracked on citizens but are not direct
    // expenses for the city budget in this simplified model.
    let net = workplace_tax + rent_income + commercial_sales_tax - maintenance_cost;
    world.resources.money += net;

    EconomyBreakdown {
        salaries_paid,
        workplace_tax,
        rent_income,
        commercial_sales_tax,
        shoppers_served,
        rent_failures,
        maintenance_cost,
        net,
    }
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

    // Citizens take one slot each. Extra citizens become unemployed; extra slots
    // remain vacant and therefore pay no salary or workplace tax.
    for (index, citizen_entity) in citizen_entities.into_iter().enumerate() {
        let workplace = workplaces.get(index).copied();
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

fn next_commercial_sales_tax(world: &World, shopping_slots: &[crate::core::entity::Entity]) -> i32 {
    shopping_slots
        .first()
        .map(|commercial| commercial_sales_tax_for_purchase(world, *commercial))
        .unwrap_or(0)
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
