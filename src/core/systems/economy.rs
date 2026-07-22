//! Economy system for citizen salary, rent, shopping, taxes, and building maintenance.
//!
//! The city budget is intentionally simple: each tick, citizens earn salary from
//! productive workplaces, pay rent to the city, optionally shop at commercial
//! buildings, and buildings charge maintenance. The system returns a structured
//! `EconomyBreakdown` so UI layers can explain the money change without reading
//! ECS internals.

use std::collections::HashMap;

use crate::core::city_refs::CityCellRef;
use crate::core::components::{BuildingData, BusinessFinance, Citizen, WorkplaceAssignment};
use crate::core::entity::Entity;
use crate::core::regions::RegionId;
use crate::core::resource_registry::JobAssignment;
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

/// Resolves and applies local workplace assignments for the derived pass.
///
/// Local assignments are pure derived state and can update while paused. Existing
/// remote assignments are preserved here because remote work is owned by the
/// cross-region tick phase and intentionally stays frozen until that phase runs.
///
/// Remote/exported job assignment does not update while paused: it resolves
/// during the daily producer-authoritative request/grant/release phase, which
/// needs a tick. Paused reads keep the last confirmed remote assignment.
pub(crate) fn assign_local_jobs(world: &mut World, local_region: RegionId) {
    let job_resolution = world.cached_job_resolution();
    apply_workplace_assignments(world, local_region, &job_resolution.assignments);
}

/// Runs the daily economy after job assignment (local in `assign_local_jobs`,
/// remote via the employment ledger) has already written each citizen's
/// `workplace_assignment`.
///
/// `exported_job_slots` lists this region's workplace entities reserved for
/// remote workers in other regions. The exporting region owns those slots, so it
/// accrues their workplace tax here; the home region only pays salary and rent.
#[cfg(test)]
pub(crate) fn run(world: &mut World, exported_job_slots: &[Entity]) -> EconomyBreakdown {
    run_with_prepared_goods_flow(world, exported_job_slots, 0, None)
}

pub(crate) fn run_with_prepared_goods_flow(
    world: &mut World,
    exported_job_slots: &[Entity],
    exported_goods_units: u32,
    prepared_goods_flow: Option<PreparedGoodsFlow>,
) -> EconomyBreakdown {
    // Workplaces and shops are recalculated every tick from powered,
    // road-connected buildings. This keeps employment and shopping deterministic
    // after build, bulldoze, replace, upgrade, save, or load.
    ensure_business_building_data(world);
    let job_resolution = world.cached_job_resolution();
    let index = EconomyIndex::from_world(world, &job_resolution.workplace_slots);
    reset_business_periods(world, &index.business_entities);
    let delivered_goods_flow = apply_pending_goods_delivery_revenue(world);

    // Industry flow:
    // 1. Confirmed truck deliveries from earlier travel settle into this
    //    period's manufacturing tax/profit.
    // 2. Productive industrial buildings create new goods.
    // 3. New local supply becomes truck grants; commercial storage changes only
    //    when those trucks arrive.
    // 4. Surplus local goods are exported for export tax.
    // 5. Citizens shopping later consume stored local goods first; empty shops
    //    import goods at a higher citizen price and lower happiness gain.
    let mut goods_flow = if let Some(prepared) = prepared_goods_flow {
        apply_prepared_goods_flow(world, &prepared, false);
        prepared.flow
    } else {
        distribute_local_goods(
            world,
            &index.productive_industrials,
            &index.productive_commercials,
            exported_goods_units,
        )
    };
    goods_flow.local_goods_stored += delivered_goods_flow.local_goods_stored;
    goods_flow.manufacturing_tax += delivered_goods_flow.manufacturing_tax;

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
        let (home, workplace_assignment, attended) = world
            .citizens
            .get(&citizen_entity)
            .map(|citizen| {
                (
                    citizen.home,
                    citizen.workplace_assignment,
                    citizen.attended_since_daily_settlement,
                )
            })
            .unwrap_or((Entity(u64::MAX), None, false));
        // A remote workplace pays the citizen salary captured at grant time but
        // collects no local workplace tax: that tax accrues to the exporting
        // region instead (see `exported_job_slots` below).
        // A local job (`workplace.region == this region`) re-checks effective local
        // salary/tax from authoritative world state; a remote job pays the salary
        // captured at grant time and collects no local workplace tax (that accrues to
        // the exporting region — see `exported_job_slots`).
        let (ungated_salary, local_workplace_tax) = match workplace_assignment {
            Some(assignment) => match assignment.workplace.as_local(world.region_id) {
                Some(workplace) => salary_for_workplace(world, workplace)
                    .map(|salary| (salary, workplace_tax_for_workplace(world, workplace)))
                    .unwrap_or((0, 0)),
                None => (assignment.salary, 0),
            },
            None => (0, 0),
        };
        let salary = if attended { ungated_salary } else { 0 };
        let rent = rent_per_citizen(world, home);
        let shopping = next_shopping_offer(world, home, &shopping_slots);

        let mut sold_local_good_from = None;
        let mut business_profit_from_sale = None;
        let mut goods_sale_source = None;
        {
            let Some(citizen) = world.citizens.get_mut(&citizen_entity) else {
                continue;
            };

            // Attendance gates the citizen's private salary. Producer-side local
            // workplace tax remains tied to productive capacity, and remote tax
            // still accrues through the exporting region's contract slot below.
            if salary > 0 {
                citizen.money += salary;
                salaries_paid += salary;
            }
            if ungated_salary > 0 {
                workplace_tax += local_workplace_tax;
            }

            // Rent is paid per citizen. Failure does not remove the citizen, but it
            // records rent stress so the happiness system can lower future morale.
            if citizen.money >= rent {
                citizen.money -= rent;
                rent_income += rent;
                citizen.morale.rent_stress = 0;
            } else {
                rent_failures += 1;
                citizen.morale.rent_stress = 1;
                citizen.morale.actual -= MISSED_RENT_HAPPINESS_PENALTY;
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
                citizen.morale.actual += shopping.happiness_bonus;
                if shopping.local_goods {
                    recover_happiness_from_shopping(citizen, LOCAL_SHOPPING_DECAY_RECOVERY);
                    local_goods_sold += 1;
                    sold_local_good_from = Some(shopping.commercial);
                    business_profit_from_sale =
                        Some((shopping.commercial, COMMERCIAL_LOCAL_GOOD_PROFIT));
                    goods_sale_source = Some((shopping.commercial, true));
                } else {
                    recover_happiness_from_shopping(citizen, IMPORTED_SHOPPING_DECAY_RECOVERY);
                    imported_goods_sold += 1;
                    business_profit_from_sale =
                        Some((shopping.commercial, COMMERCIAL_IMPORTED_GOOD_PROFIT));
                    goods_sale_source = Some((shopping.commercial, false));
                }
            } else {
                citizen.morale.actual -= MISSED_SHOPPING_HAPPINESS_PENALTY;
            }

            citizen.morale.actual = citizen.morale.actual.clamp(0, 100);
            citizen.attended_since_daily_settlement = false;
        }
        if let Some(commercial) = sold_local_good_from {
            consume_local_good(world, commercial);
        }
        if let Some((commercial, profit)) = business_profit_from_sale {
            record_business_profit(world, commercial, profit);
        }
        if let Some((commercial, from_city)) = goods_sale_source {
            record_goods_sale_source(world, commercial, from_city);
        }
    }

    // Slots this region exports to remote workers are owned here, so their
    // workplace tax accrues to this city even though the worker lives elsewhere.
    // Business profit in this model is goods/sales-based, not per-worker (a local
    // worker generates none either), so the producer simply keeps its building's
    // own profit. A reserved slot must still be an effective workplace to pay out.
    for slot in exported_job_slots {
        if is_effective_workplace(world, *slot) {
            workplace_tax += workplace_tax_for_workplace(world, *slot);
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
    fn from_world(world: &World, workplace_slots: &[Entity]) -> Self {
        let mut index = Self::default();
        let mut shopping_commercials = Vec::new();
        index.workplace_slots = workplace_slots.to_vec();

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
pub(crate) struct GoodsFlow {
    local_goods_produced: i32,
    local_goods_stored: i32,
    exported_goods: i32,
    manufacturing_tax: i32,
    export_tax: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocalGoodsGrant {
    pub industrial: Entity,
    pub commercial: Entity,
    pub caller_network: u32,
    pub units: u32,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PreparedGoodsFlow {
    flow: GoodsFlow,
    local_grants: Vec<LocalGoodsGrant>,
    industrial_profits: Vec<(Entity, i32)>,
}

impl PreparedGoodsFlow {
    pub(crate) fn local_grants(&self) -> &[LocalGoodsGrant] {
        &self.local_grants
    }
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

fn apply_workplace_assignments(
    world: &mut World,
    local_region: RegionId,
    assignments: &[JobAssignment],
) {
    for assignment in assignments {
        let workplace_assignment = assignment.workplace.and_then(|workplace| {
            let position = *world.positions.get(&workplace)?;
            Some(WorkplaceAssignment {
                workplace,
                location: CityCellRef::local(local_region, position.x, position.y),
                salary: salary_for_workplace(world, workplace).unwrap_or(0),
            })
        });
        if let Some(citizen) = world.citizens.get_mut(&assignment.citizen) {
            match (citizen.workplace_assignment, workplace_assignment) {
                // An existing remote job (`workplace.region() != this region`) is a
                // stable ledger lease; local matching never touches it. Only the
                // employment ledger (claim/apply/release/loss) changes remote work.
                (Some(existing), _) if existing.workplace.as_local(local_region).is_none() => {}
                (_, next) => citizen.workplace_assignment = next,
            }
        }
    }
}

fn distribute_local_goods(
    world: &mut World,
    productive_industrials: &[Entity],
    productive_commercials: &[Entity],
    exported_goods_units: u32,
) -> GoodsFlow {
    let prepared = prepare_goods_flow(
        world,
        productive_industrials,
        productive_commercials,
        exported_goods_units,
    );
    apply_prepared_goods_flow(world, &prepared, true);
    prepared.flow
}

pub(crate) fn prepare_goods_flow_for_current_world(
    world: &mut World,
    exported_goods_units: u32,
) -> PreparedGoodsFlow {
    ensure_business_building_data(world);
    let job_resolution = world.cached_job_resolution();
    let index = EconomyIndex::from_world(world, &job_resolution.workplace_slots);
    prepare_goods_flow(
        world,
        &index.productive_industrials,
        &index.productive_commercials,
        exported_goods_units,
    )
}

fn prepare_goods_flow(
    world: &World,
    productive_industrials: &[Entity],
    productive_commercials: &[Entity],
    exported_goods_units: u32,
) -> PreparedGoodsFlow {
    let mut local_goods_produced = 0;
    let local_goods_stored = 0;
    let mut exported_goods = 0;
    let mut manufacturing_tax = 0;
    let mut export_tax = 0;
    let mut local_grants = Vec::new();
    let mut industrial_profits = Vec::new();
    let mut commercial_free_capacity = productive_commercials
        .iter()
        .map(|commercial| {
            let capacity = commercial_goods_capacity_for_entity(world, *commercial);
            let stored = commercial_goods_stored(world, *commercial);
            let inbound_reserved = commercial_inbound_reserved_units(world, *commercial);
            (*commercial, (capacity - stored - inbound_reserved).max(0))
        })
        .collect::<HashMap<_, _>>();
    let mut regional_export_units_remaining = exported_goods_units as i32;
    for industrial in productive_industrials {
        let mut industrial_profit = 0;
        let produced = industrial_goods_production(world, *industrial);
        local_goods_produced += produced;
        let mut remaining_goods = produced;

        for commercial in nearest_commercials_for_goods(world, *industrial, productive_commercials)
        {
            let Some(free_capacity) = commercial_free_capacity.get_mut(&commercial) else {
                continue;
            };
            let supplied = (*free_capacity).min(remaining_goods);
            if supplied <= 0 {
                continue;
            }
            *free_capacity -= supplied;
            remaining_goods -= supplied;
            let caller_network = road_network_analysis::access_for(world, commercial)
                .network_id
                .unwrap_or_default();
            local_grants.push(LocalGoodsGrant {
                industrial: *industrial,
                commercial,
                caller_network,
                units: supplied as u32,
            });
            if remaining_goods == 0 {
                break;
            }
        }

        if remaining_goods > 0 && regional_export_units_remaining > 0 {
            let regional_exported = remaining_goods.min(regional_export_units_remaining);
            remaining_goods -= regional_exported;
            regional_export_units_remaining -= regional_exported;
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
        industrial_profits.push((*industrial, industrial_profit));
    }

    PreparedGoodsFlow {
        flow: GoodsFlow {
            local_goods_produced,
            local_goods_stored,
            exported_goods,
            manufacturing_tax,
            export_tax,
        },
        local_grants,
        industrial_profits,
    }
}

fn apply_prepared_goods_flow(
    world: &mut World,
    prepared: &PreparedGoodsFlow,
    apply_local_goods: bool,
) {
    if apply_local_goods {
        for grant in &prepared.local_grants {
            add_commercial_goods(world, grant.commercial, grant.units as i32);
            record_local_goods_period_revenue(
                world,
                grant.industrial,
                grant.commercial,
                grant.units as i32,
            );
        }
    }
    for (industrial, profit) in &prepared.industrial_profits {
        record_business_profit(world, *industrial, *profit);
    }
}

#[derive(Debug, Default)]
struct GoodsDistributionPlan {
    commercial_free_capacity: HashMap<Entity, i32>,
}

fn goods_distribution_after_local_storage(world: &World) -> GoodsDistributionPlan {
    let mut productive_industrials = Vec::new();
    let mut productive_commercials = Vec::new();

    let mut workplaces = world
        .buildings
        .iter()
        .filter_map(|(entity, building)| {
            is_effective_workplace(world, *entity).then_some((*entity, building.kind))
        })
        .collect::<Vec<_>>();
    workplaces.sort_by_key(|(entity, _kind)| {
        world
            .positions
            .get(entity)
            .map(|position| (position.y, position.x, entity.0))
            .unwrap_or((usize::MAX, usize::MAX, entity.0))
    });

    for (entity, kind) in workplaces {
        match kind {
            BuildingKind::Industrial => productive_industrials.push(entity),
            BuildingKind::Commercial => productive_commercials.push(entity),
            _ => {}
        }
    }

    let mut commercial_free_capacity = productive_commercials
        .iter()
        .map(|commercial| {
            let capacity = commercial_goods_capacity_for_entity(world, *commercial);
            let stored = commercial_goods_stored(world, *commercial);
            let inbound_reserved = commercial_inbound_reserved_units(world, *commercial);
            (*commercial, (capacity - stored - inbound_reserved).max(0))
        })
        .collect::<HashMap<_, _>>();

    for industrial in productive_industrials {
        let mut remaining_goods = industrial_goods_production(world, industrial);
        for commercial in nearest_commercials_for_goods(world, industrial, &productive_commercials)
        {
            let Some(free_capacity) = commercial_free_capacity.get_mut(&commercial) else {
                continue;
            };
            let supplied = (*free_capacity).min(remaining_goods);
            *free_capacity -= supplied;
            remaining_goods -= supplied;
            if remaining_goods == 0 {
                break;
            }
        }
    }

    GoodsDistributionPlan {
        commercial_free_capacity,
    }
}

pub(crate) fn commercial_goods_demands_after_local_distribution(
    world: &World,
) -> Vec<(Entity, u32, u32)> {
    let mut demands = goods_distribution_after_local_storage(world)
        .commercial_free_capacity
        .into_iter()
        .filter_map(|(commercial, free_capacity)| {
            let network_id = road_network_analysis::access_for(world, commercial).network_id?;
            (free_capacity > 0).then_some((commercial, free_capacity as u32, network_id))
        })
        .collect::<Vec<_>>();
    demands.sort_by_key(|(commercial, _units, network_id)| {
        world
            .positions
            .get(commercial)
            .map(|position| (*network_id, position.y, position.x, commercial.0))
            .unwrap_or((*network_id, usize::MAX, usize::MAX, commercial.0))
    });
    demands
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

pub(crate) fn salary_for_workplace(world: &World, workplace: Entity) -> Option<i32> {
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

fn commercial_inbound_reserved_units(world: &World, commercial: Entity) -> i32 {
    world
        .goods_orders
        .values()
        .filter(|order| order.commercial == commercial)
        .map(|order| order.inbound_reserved_units)
        .sum::<i32>()
        .max(0)
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
            commercial: Entity(u64::MAX),
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
    if let Some(building) = world.buildings.get_mut(&commercial)
        && let BuildingData::Commercial {
            local_goods_stored, ..
        } = &mut building.data
        && *local_goods_stored > 0
    {
        *local_goods_stored -= 1;
    }
}

fn recover_happiness_from_shopping(citizen: &mut Citizen, amount: i32) {
    let recovered = citizen.morale.decay.min(amount.max(0));
    citizen.morale.decay -= recovered;
    citizen.morale.actual += recovered;
}

pub(crate) fn add_commercial_goods(world: &mut World, commercial: Entity, amount: i32) {
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

pub(crate) fn record_local_goods_delivery_revenue(
    world: &mut World,
    industrial: Entity,
    commercial: Entity,
    units: i32,
) -> i32 {
    let manufacturing_tax = local_goods_delivery_margin(world, industrial, commercial, units);
    if manufacturing_tax <= 0 {
        return 0;
    }
    let entry = world
        .pending_goods_delivery_revenue
        .entry(industrial)
        .or_default();
    if commercial.region() == world.region_id {
        entry.local_stored_units += units;
    }
    entry.manufacturing_tax += manufacturing_tax;
    manufacturing_tax
}

fn apply_pending_goods_delivery_revenue(world: &mut World) -> GoodsFlow {
    let pending = std::mem::take(&mut world.pending_goods_delivery_revenue);
    let mut flow = GoodsFlow::default();
    for (industrial, revenue) in pending {
        flow.local_goods_stored += revenue.local_stored_units;
        flow.manufacturing_tax += revenue.manufacturing_tax;
        record_business_profit(world, industrial, revenue.manufacturing_tax);
    }
    flow
}

fn record_local_goods_period_revenue(
    world: &mut World,
    industrial: Entity,
    commercial: Entity,
    units: i32,
) -> i32 {
    let manufacturing_tax = local_goods_delivery_margin(world, industrial, commercial, units);
    if manufacturing_tax > 0 {
        record_business_profit(world, industrial, manufacturing_tax);
    }
    manufacturing_tax
}

fn local_goods_delivery_margin(
    world: &World,
    industrial: Entity,
    commercial: Entity,
    units: i32,
) -> i32 {
    if units <= 0 {
        return 0;
    }
    let distance = if commercial.region() == world.region_id {
        road_network_analysis::distance_between_buildings(world, industrial, commercial)
    } else {
        // Cross-region confirms do not carry the full producer-to-consumer route.
        // Use the same producer-to-edge proxy as outside exports until grants
        // carry a route distance.
        road_network_analysis::access_for(world, industrial).import_export_distance
    };
    units * margin_per_good(MANUFACTURING_TAX_PER_GOOD, distance)
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
                        goods: Default::default(),
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
            BuildingData::Commercial { business, .. }
            | BuildingData::Industrial { business, .. } => Some(business),
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

pub(crate) fn recent_commercial_goods_sources(world: &World, entity: Entity) -> (i32, i32) {
    business_finance(world, entity)
        .map(|business| {
            (
                business.last_period_goods_from_city,
                business.last_period_goods_from_outside,
            )
        })
        .unwrap_or((0, 0))
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

fn record_goods_sale_source(world: &mut World, entity: Entity, from_city: bool) {
    if let Some(business) = business_finance_mut(world, entity) {
        if from_city {
            business.last_period_goods_from_city += 1;
        } else {
            business.last_period_goods_from_outside += 1;
        }
    }
}

fn reset_business_periods(world: &mut World, business_entities: &[Entity]) {
    for entity in business_entities {
        if let Some(business) = business_finance_mut(world, *entity) {
            business.last_period_profit = 0;
            business.last_period_goods_from_city = 0;
            business.last_period_goods_from_outside = 0;
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
            BuildingData::Commercial { business, .. }
            | BuildingData::Industrial { business, .. } => Some(business),
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
    use super::{EconomyIndex, assign_local_jobs, run};
    use crate::core::city_refs::CityCellRef;
    use crate::core::components::{GoodsOrder, GoodsOrderId, WorkplaceAssignment};
    use crate::core::entity::Entity;
    use crate::core::regional_types::UiRequestId;
    use crate::core::regions::RegionId;
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

        let job_resolution = world.cached_job_resolution();
        let index = EconomyIndex::from_world(&world, &job_resolution.workplace_slots);

        assert_eq!(index.business_entities, vec![commercial, industrial]);
        assert_eq!(index.productive_commercials, vec![commercial]);
        assert_eq!(index.productive_industrials, vec![industrial]);
        assert_eq!(index.workplace_slots.len(), 5);
        assert_eq!(index.shopping_slots, vec![commercial; 4]);
        assert_eq!(index.maintenance_cost, 2);
    }

    #[test]
    fn derived_local_assignment_preserves_existing_remote_job_until_daily_phase() {
        let mut world = World::new(4, 3);
        placement::place_building(&mut world, 0, 0, BuildingKind::Residential);
        placement::place_building(&mut world, 1, 0, BuildingKind::Commercial);
        placement::place_building(&mut world, 0, 1, BuildingKind::Road);
        placement::place_building(&mut world, 1, 1, BuildingKind::Road);
        let residential = world.grid.get(0, 0).expect("residential");
        let commercial = world.grid.get(1, 0).expect("commercial");
        world.power_consumers.get_mut(&commercial).unwrap().powered = true;
        road_network_analysis::run(&mut world);
        citizens::spawn_for_home(&mut world, residential, 1);
        let citizen = *world.citizens.keys().next().expect("citizen");
        // Remote workplace: Entity with birth region 2
        let remote_workplace = Entity::new(RegionId(2), 7);
        world
            .citizens
            .get_mut(&citizen)
            .unwrap()
            .workplace_assignment = Some(WorkplaceAssignment {
            workplace: remote_workplace,
            location: CityCellRef::local(RegionId(2), 3, 0),
            salary: 4,
        });

        assign_local_jobs(&mut world, RegionId(1));

        let assignment = world
            .citizens
            .get(&citizen)
            .expect("citizen")
            .workplace_assignment
            .expect("assignment");
        // The remote assignment (workplace.region() != local region 1) is preserved.
        assert_eq!(assignment.workplace, remote_workplace);
        assert_eq!(assignment.workplace.as_local(RegionId(1)), None);
    }

    #[test]
    fn successful_shopping_reduces_existing_happiness_decay() {
        let (mut world, citizen) = shopping_recovery_world(false, true);
        let before_happiness = citizen_happiness(&world, citizen);

        let economy = run(&mut world, &[]);

        assert_eq!(economy.shoppers_served, 1);
        assert_eq!(economy.imported_goods_sold, 1);
        assert_eq!(citizen_decay(&world, citizen), 8);
        assert_eq!(citizen_happiness(&world, citizen), before_happiness + 4);
    }

    #[test]
    fn local_goods_recover_more_happiness_decay_than_imported_goods() {
        let (mut local_world, local_citizen) = shopping_recovery_world(true, true);
        let (mut imported_world, imported_citizen) = shopping_recovery_world(false, true);

        let local_economy = run(&mut local_world, &[]);
        let imported_economy = run(&mut imported_world, &[]);

        assert_eq!(local_economy.local_goods_sold, 1);
        assert_eq!(imported_economy.imported_goods_sold, 1);
        assert_eq!(citizen_decay(&local_world, local_citizen), 6);
        assert_eq!(citizen_decay(&imported_world, imported_citizen), 8);
    }

    #[test]
    fn disconnected_citizen_does_not_recover_happiness_decay() {
        let (mut world, citizen) = shopping_recovery_world(false, false);

        let economy = run(&mut world, &[]);

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
            // The daily tick assigns local jobs before the economy settles; mirror
            // that here, then record the P1 work arrival before the citizen earns
            // the salary it spends on shopping.
            super::assign_local_jobs(&mut world, RegionId(1));
            world
                .citizens
                .get_mut(&citizen)
                .expect("citizen remains present")
                .attended_since_daily_settlement = true;
            let economy = run(&mut world, &[]);
            assert_eq!(economy.shoppers_served, 1);
        }

        assert_eq!(citizen_decay(&world, citizen), 0);
    }

    #[test]
    fn attendance_gates_local_salary_but_not_local_workplace_tax() {
        let (mut world, citizen) = shopping_recovery_world(false, true);
        super::assign_local_jobs(&mut world, RegionId(1));
        let workplace = world.citizens[&citizen]
            .workplace_assignment
            .expect("local job assigned")
            .workplace;
        let expected_tax = super::workplace_tax_for_workplace(&world, workplace);

        let unattended = run(&mut world, &[]);

        assert_eq!(unattended.salaries_paid, 0);
        assert_eq!(unattended.workplace_tax, expected_tax);
        assert!(
            !world.citizens[&citizen].attended_since_daily_settlement,
            "daily settlement clears the attendance interval"
        );

        world
            .citizens
            .get_mut(&citizen)
            .unwrap()
            .attended_since_daily_settlement = true;
        let attended = run(&mut world, &[]);

        assert!(attended.salaries_paid > 0);
        assert_eq!(attended.workplace_tax, expected_tax);
    }

    #[test]
    fn inbound_goods_orders_reduce_commercial_demand_capacity() {
        let mut world = World::new(4, 2);
        placement::place_building(&mut world, 0, 0, BuildingKind::Industrial);
        placement::place_building(&mut world, 1, 0, BuildingKind::Road);
        placement::place_building(&mut world, 2, 0, BuildingKind::Commercial);
        let industrial = world.grid.get(0, 0).expect("industrial");
        let commercial = world.grid.get(2, 0).expect("commercial");
        world.power_consumers.get_mut(&industrial).unwrap().powered = true;
        world.power_consumers.get_mut(&commercial).unwrap().powered = true;
        road_network_analysis::run(&mut world);

        let order = GoodsOrderId {
            commercial,
            request_id: UiRequestId(99),
            token: 0,
        };
        world.goods_orders.insert(
            order,
            GoodsOrder {
                id: order,
                commercial,
                requested_units: 8,
                inbound_reserved_units: super::commercial_goods_capacity_for_entity(
                    &world, commercial,
                ),
                remaining_units: 8,
            },
        );

        let prepared = super::prepare_goods_flow_for_current_world(&mut world, 0);
        assert!(
            prepared.local_grants().is_empty(),
            "in-flight truck cargo must reserve the shelf space from local grants"
        );
        assert!(
            super::commercial_goods_demands_after_local_distribution(&world).is_empty(),
            "in-flight truck cargo must reserve the shelf space from export demand"
        );
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
            citizen.morale.decay = 10;
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
        citizen.morale.decay = decay;
    }

    fn citizen_decay(world: &World, citizen: Entity) -> i32 {
        world.citizens.get(&citizen).expect("citizen").morale.decay
    }

    fn citizen_happiness(world: &World, citizen: Entity) -> i32 {
        world.citizens.get(&citizen).expect("citizen").morale.actual
    }
}
