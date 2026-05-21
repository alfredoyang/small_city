//! Weekly business reinvestment system for automatic commercial and industrial upgrades.

use crate::core::systems::{citizens, economy, road_connectivity, upgrade};
use crate::core::world::World;
use crate::interface::input::BuildingKind;

const COMMERCIAL_REINVESTMENT_THRESHOLD: i32 = 8;
const INDUSTRIAL_REINVESTMENT_THRESHOLD: i32 = 14;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct BusinessGrowthSummary {
    pub commercial_upgrades: i32,
    pub industrial_upgrades: i32,
}

pub(crate) fn run(world: &mut World) -> BusinessGrowthSummary {
    economy::ensure_business_building_data(world);
    let mut candidates: Vec<_> = world
        .buildings
        .iter()
        .filter_map(|(entity, building)| {
            matches!(
                building.kind,
                BuildingKind::Commercial | BuildingKind::Industrial
            )
            .then_some((*entity, building.kind))
        })
        .collect();
    candidates.sort_by_key(|(entity, _kind)| {
        world
            .positions
            .get(entity)
            .map(|position| (position.y, position.x, entity.0))
            .unwrap_or((usize::MAX, usize::MAX, entity.0))
    });

    let mut summary = BusinessGrowthSummary::default();
    for (entity, kind) in candidates {
        if !can_reinvest(world, entity, kind) {
            continue;
        }
        let Some(threshold) = reinvestment_threshold(kind) else {
            continue;
        };
        if let Some(building) = world.buildings.get_mut(&entity) {
            building.level += 1;
        }
        economy::spend_business_cash(world, entity, threshold);
        upgrade::apply_upgrade_effect(world, entity, kind);
        match kind {
            BuildingKind::Commercial => summary.commercial_upgrades += 1,
            BuildingKind::Industrial => summary.industrial_upgrades += 1,
            _ => {}
        }
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

pub(crate) fn can_reinvest(
    world: &World,
    entity: crate::core::entity::Entity,
    kind: BuildingKind,
) -> bool {
    let Some(building) = world.buildings.get(&entity) else {
        return false;
    };
    if building.kind != kind || building.level >= upgrade::MAX_UPGRADE_LEVEL {
        return false;
    }
    let Some(threshold) = reinvestment_threshold(kind) else {
        return false;
    };
    let powered = world
        .power_consumers
        .get(&entity)
        .is_some_and(|consumer| consumer.powered);
    powered
        && road_connectivity::is_road_connected(world, entity)
        && demand_allows_reinvestment(world, kind)
        && economy::business_cash(world, entity) >= threshold
        && economy::recent_business_profit(world, entity) > 0
}

pub(crate) fn demand_allows_reinvestment(world: &World, kind: BuildingKind) -> bool {
    match kind {
        BuildingKind::Commercial => citizens::citizen_count(world) > 0,
        BuildingKind::Industrial => {
            citizens::citizen_count(world) > 0 && world.stats.pollution <= 8
        }
        _ => false,
    }
}
