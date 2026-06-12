//! Weekly business reinvestment system for automatic commercial and industrial upgrades.

use crate::core::entity::Entity;
use crate::core::systems::{citizens, economy, road_connectivity};
use crate::core::world::World;
use crate::interface::input::BuildingKind;

const COMMERCIAL_REINVESTMENT_THRESHOLD: i32 = 8;
const INDUSTRIAL_REINVESTMENT_THRESHOLD: i32 = 14;
pub(crate) const MAX_REINVESTMENT_LEVEL: u8 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct BusinessGrowthSummary {
    pub commercial_upgrades: i32,
    pub industrial_upgrades: i32,
    pub upgrades: Vec<BusinessUpgrade>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BusinessUpgrade {
    pub x: usize,
    pub y: usize,
    pub kind: BuildingKind,
    pub level: u8,
}

pub(crate) fn run(world: &mut World) -> BusinessGrowthSummary {
    economy::ensure_business_building_data(world);
    let context = BusinessGrowthContext::from_world(world);
    let candidates = business_candidates(world);

    let mut summary = BusinessGrowthSummary::default();
    for entity in candidates {
        let Some(candidate) = evaluate_reinvestment(world, &context, entity) else {
            continue;
        };

        if let Some(building) = world.buildings.get_mut(&candidate.entity) {
            building.level = candidate.next_level;
        }
        economy::spend_business_cash(world, candidate.entity, candidate.cost);
        apply_business_upgrade_effect(
            world,
            candidate.entity,
            candidate.kind,
            candidate.next_level,
        );
        match candidate.kind {
            BuildingKind::Commercial => summary.commercial_upgrades += 1,
            BuildingKind::Industrial => summary.industrial_upgrades += 1,
            _ => {}
        }
        if let Some(position) = world.positions.get(&candidate.entity) {
            summary.upgrades.push(BusinessUpgrade {
                x: position.x,
                y: position.y,
                kind: candidate.kind,
                level: candidate.next_level,
            });
        }
    }
    if !summary.upgrades.is_empty() {
        world.invalidate_jobs_registry();
    }

    summary
}

pub(crate) fn reinvestment_threshold(kind: BuildingKind) -> Option<i32> {
    match kind {
        BuildingKind::Commercial => Some(COMMERCIAL_REINVESTMENT_THRESHOLD),
        BuildingKind::Industrial => Some(INDUSTRIAL_REINVESTMENT_THRESHOLD),
        _ => None,
    }
}

pub(crate) fn can_reinvest(world: &World, entity: Entity, kind: BuildingKind) -> bool {
    let context = BusinessGrowthContext::from_world(world);
    evaluate_reinvestment(world, &context, entity).is_some_and(|candidate| candidate.kind == kind)
}

pub(crate) fn demand_allows_reinvestment(world: &World, kind: BuildingKind) -> bool {
    let context = BusinessGrowthContext::from_world(world);
    demand_allows_reinvestment_with_context(&context, kind)
}

#[derive(Debug, Clone, Copy)]
struct BusinessGrowthContext {
    citizen_count: i32,
    pollution: i32,
}

impl BusinessGrowthContext {
    fn from_world(world: &World) -> Self {
        Self {
            citizen_count: citizens::citizen_count(world),
            pollution: world.stats.pollution,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReinvestmentCandidate {
    entity: Entity,
    kind: BuildingKind,
    next_level: u8,
    cost: i32,
}

fn business_candidates(world: &World) -> Vec<Entity> {
    let mut candidates: Vec<_> = world
        .buildings
        .iter()
        .filter_map(|(entity, building)| {
            matches!(
                building.kind,
                BuildingKind::Commercial | BuildingKind::Industrial
            )
            .then_some(*entity)
        })
        .collect();
    candidates.sort_by_key(|entity| {
        world
            .positions
            .get(entity)
            .map(|position| (position.y, position.x, entity.0))
            .unwrap_or((usize::MAX, usize::MAX, entity.0))
    });
    candidates
}

fn evaluate_reinvestment(
    world: &World,
    context: &BusinessGrowthContext,
    entity: Entity,
) -> Option<ReinvestmentCandidate> {
    let building = world.buildings.get(&entity)?;
    if building.level >= MAX_REINVESTMENT_LEVEL {
        return None;
    }
    let kind = building.kind;
    let next_level = building.level + 1;
    let cost = reinvestment_upgrade_cost(kind, next_level)?;
    let powered = world
        .power_consumers
        .get(&entity)
        .is_some_and(|consumer| consumer.powered);
    if !powered
        || !road_connectivity::is_road_connected(world, entity)
        || !demand_allows_reinvestment_with_context(context, kind)
    {
        return None;
    }

    let finance = economy::business_finance(world, entity)?;
    (finance.business_cash >= cost && finance.last_period_profit > 0).then_some(
        ReinvestmentCandidate {
            entity,
            kind,
            next_level,
            cost,
        },
    )
}

pub(crate) fn reinvestment_upgrade_cost(kind: BuildingKind, target_level: u8) -> Option<i32> {
    match (kind, target_level) {
        (BuildingKind::Commercial, 2 | 3) => Some(COMMERCIAL_REINVESTMENT_THRESHOLD),
        (BuildingKind::Industrial, 2 | 3) => Some(INDUSTRIAL_REINVESTMENT_THRESHOLD),
        _ => None,
    }
}

fn demand_allows_reinvestment_with_context(
    context: &BusinessGrowthContext,
    kind: BuildingKind,
) -> bool {
    match kind {
        BuildingKind::Commercial => context.citizen_count > 0,
        BuildingKind::Industrial => context.citizen_count > 0 && context.pollution <= 8,
        _ => false,
    }
}

fn apply_business_upgrade_effect(world: &mut World, entity: Entity, kind: BuildingKind, level: u8) {
    if kind == BuildingKind::Industrial {
        if let Some(source) = world.pollution_sources.get_mut(&entity) {
            // Industrial production scales by level through the economy system.
            // Pollution follows the same simple level curve so level 3 factories
            // carry a stronger local and city-wide environmental tradeoff.
            source.amount = i32::from(level.max(1)) + 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{reinvestment_upgrade_cost, run};
    use crate::core::components::{BuildingData, BusinessFinance};
    use crate::core::systems::{citizens, placement};
    use crate::core::world::World;
    use crate::interface::input::BuildingKind;

    #[test]
    fn run_upgrades_eligible_businesses_from_single_candidate_pass() {
        let mut world = World::new(5, 3);
        placement::place_building(&mut world, 0, 0, BuildingKind::Residential);
        placement::place_building(&mut world, 1, 0, BuildingKind::Commercial);
        placement::place_building(&mut world, 2, 0, BuildingKind::Industrial);
        for x in 0..=2 {
            placement::place_building(&mut world, x, 1, BuildingKind::Road);
        }

        let residential = world.grid.get(0, 0).expect("residential entity");
        let commercial = world.grid.get(1, 0).expect("commercial entity");
        let industrial = world.grid.get(2, 0).expect("industrial entity");
        citizens::spawn_for_home(&mut world, residential, 1);
        world.power_consumers.get_mut(&commercial).unwrap().powered = true;
        world.power_consumers.get_mut(&industrial).unwrap().powered = true;
        set_business_finance(&mut world, commercial, 10, 3);
        set_business_finance(&mut world, industrial, 20, 5);

        let summary = run(&mut world);

        assert_eq!(summary.commercial_upgrades, 1);
        assert_eq!(summary.industrial_upgrades, 1);
        assert_eq!(
            summary
                .upgrades
                .iter()
                .map(|upgrade| (upgrade.x, upgrade.y, upgrade.kind, upgrade.level))
                .collect::<Vec<_>>(),
            vec![
                (1, 0, BuildingKind::Commercial, 2),
                (2, 0, BuildingKind::Industrial, 2)
            ]
        );
        assert_eq!(world.buildings.get(&commercial).unwrap().level, 2);
        assert_eq!(world.buildings.get(&industrial).unwrap().level, 2);
        assert_eq!(business_cash(&world, commercial), 2);
        assert_eq!(business_cash(&world, industrial), 6);
        assert_eq!(world.pollution_sources.get(&industrial).unwrap().amount, 3);
    }

    #[test]
    fn run_does_not_upgrade_when_business_cash_cannot_cover_cost() {
        let mut world = World::new(4, 3);
        placement::place_building(&mut world, 0, 0, BuildingKind::Residential);
        placement::place_building(&mut world, 1, 0, BuildingKind::Commercial);
        for x in 0..=1 {
            placement::place_building(&mut world, x, 1, BuildingKind::Road);
        }

        let residential = world.grid.get(0, 0).expect("residential entity");
        let commercial = world.grid.get(1, 0).expect("commercial entity");
        citizens::spawn_for_home(&mut world, residential, 1);
        world.power_consumers.get_mut(&commercial).unwrap().powered = true;
        let cost = reinvestment_upgrade_cost(BuildingKind::Commercial, 2).expect("upgrade cost");
        set_business_finance(&mut world, commercial, cost - 1, 3);

        let summary = run(&mut world);

        assert_eq!(summary.commercial_upgrades, 0);
        assert!(summary.upgrades.is_empty());
        assert_eq!(world.buildings.get(&commercial).unwrap().level, 1);
        assert_eq!(business_cash(&world, commercial), cost - 1);
    }

    fn set_business_finance(
        world: &mut World,
        entity: crate::core::entity::Entity,
        business_cash: i32,
        last_period_profit: i32,
    ) {
        let building = world.buildings.get_mut(&entity).expect("business building");
        match &mut building.data {
            BuildingData::Commercial { business, .. } | BuildingData::Industrial { business } => {
                *business = BusinessFinance {
                    business_cash,
                    last_period_profit,
                    ..BusinessFinance::default()
                };
            }
            BuildingData::None => panic!("expected business finance"),
        }
    }

    fn business_cash(world: &World, entity: crate::core::entity::Entity) -> i32 {
        match world.buildings.get(&entity).unwrap().data {
            BuildingData::Commercial { business, .. } | BuildingData::Industrial { business } => {
                business.business_cash
            }
            BuildingData::None => 0,
        }
    }
}
