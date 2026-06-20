//! Upgrade command handling: zoned buildings grow their footprint and capacity, merging same-type
//! neighbours and blocking when there is no room to level up.
//!
//! ```text
//!  Strict-rectangle growth (target area from the building rules):
//!    extend one full side (N,E,S,W order); a side qualifies only if it yields a rectangle of the
//!    target area whose new cells are all claimable (empty, or a same-type building fully inside).
//!    Prefer a side that merges a same-type neighbour, else the first all-empty side; else block.
//! ```

use crate::core::building_stats::capacity_for;
use crate::core::components::{BuildingData, Footprint, Position};
use crate::core::entity::Entity;
use crate::core::systems::{citizens, economy, entity_cleanup};
use crate::core::world::World;
use crate::interface::events::{CommandResult, GameEventView};
use crate::interface::input::BuildingKind;

/// Highest level a building can reach. Zoned buildings grow 1x1 -> ~2x1 -> 2x2 across these levels.
pub(crate) const MAX_UPGRADE_LEVEL: u8 = 3;

pub(crate) fn upgrade(world: &mut World, x: usize, y: usize) -> CommandResult {
    if !world.grid.contains(x, y) {
        return fail("Cannot upgrade outside the map");
    }

    let Some(entity) = world.grid.get(x, y) else {
        return fail("Cannot upgrade an empty cell");
    };

    let Some(building) = world.buildings.get(&entity).copied() else {
        return fail("Cannot upgrade unknown building");
    };

    if building.level >= MAX_UPGRADE_LEVEL {
        return fail("Building is already fully upgraded");
    }

    let next_level = building.level + 1;
    let Some(cost) = building.kind.upgrade_cost_for_level(next_level) else {
        // No upgrade defined for the next level. A building that upgrades at all (e.g. Park, Power
        // capped at level 2) is fully upgraded; one that never upgrades (e.g. Road) simply cannot.
        return if building.kind.upgrade_cost_for_level(2).is_some() {
            fail("Building is already fully upgraded")
        } else {
            CommandResult::failure(GameEventView::UpgradeFailed {
                reason: format!("{} cannot be upgraded", building.kind.label()),
            })
        };
    };

    // Money sufficiency is checked before space (so a broke player sees "not enough money"), but is
    // only *spent* once space is confirmed below — a blocked upgrade changes nothing.
    if world.resources.money < cost {
        return fail("Not enough money to upgrade");
    }

    // Grow the footprint (atomic: nothing changes if there is no room). Charge only on success.
    if !grow_to_level(world, entity, next_level) {
        return fail("There is no space to level up");
    }
    world.resources.money -= cost;

    CommandResult::success(GameEventView::BuildingUpgraded {
        x,
        y,
        kind: building.kind,
        level: next_level,
    })
}

/// Grows `entity`'s footprint to the area its `next_level` requires, merging same-type neighbours,
/// then applies the new level and capacity. Returns `false` and changes nothing if there is no room
/// (so callers can treat it atomically). Shared by the manual upgrade command and business
/// reinvestment. Does not touch money — the caller owns cost.
pub(crate) fn grow_to_level(world: &mut World, entity: Entity, next_level: u8) -> bool {
    let Some(building) = world.buildings.get(&entity).copied() else {
        return false;
    };
    let Some(anchor) = world.positions.get(&entity).copied() else {
        return false;
    };
    let current = Rect::new(
        anchor.x,
        anchor.y,
        building.footprint.width as usize,
        building.footprint.height as usize,
    );
    let target_area = world
        .building_rules()
        .footprint_area(building.kind, next_level) as usize;

    // Already-large-enough footprints (e.g. grown by earlier merges) just level up in place;
    // otherwise extend one side, or fail when there is no room.
    let new_rect = if current.area() >= target_area {
        current
    } else {
        match choose_extension(world, entity, building.kind, current, target_area) {
            Some(rect) => rect,
            None => return false,
        }
    };

    // Merge: absorb every same-type neighbour fully inside the new rectangle, transferring its
    // contents into this building (M3). Citizens are re-homed *before* removal so they are not
    // despawned with the neighbour; goods/cash are accumulated and applied once the new capacity is
    // known.
    let neighbours = same_type_entities_in(world, entity, new_rect);
    let mut absorbed_goods = 0;
    let mut absorbed_cash = 0;
    for &neighbour in &neighbours {
        reassign_citizen_homes(world, neighbour, entity);
        absorbed_goods += economy::commercial_goods_stored(world, neighbour);
        absorbed_cash += business_cash_of(world, neighbour);
        let (nx, ny) = world
            .positions
            .get(&neighbour)
            .map(|position| (position.x, position.y))
            .unwrap_or((anchor.x, anchor.y));
        entity_cleanup::remove_entity(world, neighbour, nx, ny);
    }

    // Stamp the building into every cell of its new footprint and update its components.
    world
        .grid
        .set_footprint(new_rect.x, new_rect.y, new_rect.w, new_rect.h, entity);
    world.positions.insert(
        entity,
        Position {
            x: new_rect.x,
            y: new_rect.y,
        },
    );
    if let Some(building) = world.buildings.get_mut(&entity) {
        building.level = next_level;
        building.footprint = Footprint {
            width: new_rect.w as u8,
            height: new_rect.h as u8,
        };
    }
    apply_upgrade_effect(world, entity, building.kind);

    // Apply absorbed contents now that the merged building's new capacity is set: cash sums, goods
    // are capped at the new storage, and excess residents (beyond the new max) are dropped.
    if absorbed_cash != 0 {
        add_business_cash(world, entity, absorbed_cash);
    }
    if absorbed_goods != 0 {
        add_commercial_goods_capped(world, entity, absorbed_goods);
    }
    cap_residents_to_capacity(world, entity);

    // Growing the footprint changes grid occupancy (power/road adjacency, jobs), so refresh derived
    // state broadly rather than relying on per-kind invalidation alone.
    world.invalidate_resource_registry();
    true
}

fn fail(reason: &str) -> CommandResult {
    CommandResult::failure(GameEventView::UpgradeFailed {
        reason: reason.to_string(),
    })
}

/// Re-homes every citizen living in `from` to `to`. Called before a merged neighbour is removed so
/// its residents move into the merged building instead of being despawned with it.
fn reassign_citizen_homes(world: &mut World, from: Entity, to: Entity) {
    for citizen in world.citizens.values_mut() {
        if citizen.home == from {
            citizen.home = to;
        }
    }
}

/// Private business cash a building holds (0 for kinds without business data).
fn business_cash_of(world: &World, entity: Entity) -> i32 {
    match world.buildings.get(&entity).map(|building| &building.data) {
        Some(BuildingData::Commercial { business, .. } | BuildingData::Industrial { business }) => {
            business.business_cash
        }
        _ => 0,
    }
}

/// Adds `amount` to a building's business cash (commercial or industrial).
fn add_business_cash(world: &mut World, entity: Entity, amount: i32) {
    if let Some(building) = world.buildings.get_mut(&entity) {
        match &mut building.data {
            BuildingData::Commercial { business, .. } | BuildingData::Industrial { business } => {
                business.business_cash += amount;
            }
            BuildingData::None => {}
        }
    }
}

/// Adds `amount` goods to a commercial building's local stock, capped at its (level-based) storage.
fn add_commercial_goods_capped(world: &mut World, entity: Entity, amount: i32) {
    let capacity = economy::commercial_goods_capacity_for_entity(world, entity);
    if let Some(building) = world.buildings.get_mut(&entity) {
        if let BuildingData::Commercial {
            local_goods_stored, ..
        } = &mut building.data
        {
            *local_goods_stored = (*local_goods_stored + amount).min(capacity);
        }
    }
}

/// Drops citizens beyond a residential building's max population after a merge (deterministic by
/// entity id), then refreshes the population cache. A no-op for buildings without residents.
fn cap_residents_to_capacity(world: &mut World, entity: Entity) {
    let max = world
        .populations
        .get(&entity)
        .map(|population| population.max.max(0))
        .unwrap_or(0);
    let mut homed: Vec<Entity> = world
        .citizens
        .iter()
        .filter(|(_, citizen)| citizen.home == entity)
        .map(|(citizen_entity, _)| *citizen_entity)
        .collect();
    homed.sort_by_key(|citizen_entity| citizen_entity.0);
    for &citizen in homed.iter().skip(max as usize) {
        world.citizens.remove(&citizen);
        world.entities.remove(&citizen);
    }
    citizens::sync_population_from_citizens(world);
}

pub(crate) fn apply_upgrade_effect(world: &mut World, entity: Entity, kind: BuildingKind) {
    match kind {
        BuildingKind::Residential => {
            // Capacity follows the footprint area through the single capacity_for source.
            let area = world
                .buildings
                .get(&entity)
                .map(|building| building.footprint.area())
                .unwrap_or(1);
            if let Some(population) = world.populations.get_mut(&entity) {
                population.max = capacity_for(BuildingKind::Residential, area);
            }
        }
        BuildingKind::PowerPlant => {
            if let Some(provider) = world.power_providers.get_mut(&entity) {
                provider.capacity = 15;
            }
        }
        BuildingKind::Park => {
            if let Some(effect) = world.happiness_effects.get_mut(&entity) {
                effect.amount = 5;
            }
        }
        BuildingKind::Industrial => {
            // Pollution follows the level curve (level + 1), consistent for manual and auto upgrades.
            let level = world
                .buildings
                .get(&entity)
                .map(|building| building.level)
                .unwrap_or(1);
            if let Some(source) = world.pollution_sources.get_mut(&entity) {
                source.amount = i32::from(level.max(1)) + 1;
            }
        }
        // Commercial capacity (jobs) is derived from footprint area in the jobs registry.
        BuildingKind::Road | BuildingKind::Commercial => {}
    }
    match kind {
        BuildingKind::PowerPlant => world.invalidate_resource_registry(),
        BuildingKind::Residential | BuildingKind::Commercial | BuildingKind::Industrial => {
            world.invalidate_jobs_registry();
        }
        BuildingKind::Road | BuildingKind::Park => {}
    }
}

/// An axis-aligned rectangle of grid cells (anchor at top-left).
#[derive(Clone, Copy)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

impl Rect {
    fn new(x: usize, y: usize, w: usize, h: usize) -> Self {
        Self { x, y, w, h }
    }

    fn area(&self) -> usize {
        self.w * self.h
    }

    fn contains_cell(&self, cx: usize, cy: usize) -> bool {
        cx >= self.x && cx < self.x + self.w && cy >= self.y && cy < self.y + self.h
    }

    fn contains_rect(&self, other: &Rect) -> bool {
        other.x >= self.x
            && other.y >= self.y
            && other.x + other.w <= self.x + self.w
            && other.y + other.h <= self.y + self.h
    }

    fn cells(&self) -> Vec<(usize, usize)> {
        let mut cells = Vec::with_capacity(self.w * self.h);
        for cy in self.y..self.y + self.h {
            for cx in self.x..self.x + self.w {
                cells.push((cx, cy));
            }
        }
        cells
    }
}

#[derive(Clone, Copy)]
enum Side {
    North,
    East,
    South,
    West,
}

/// Fixed scan order so growth is deterministic.
const SIDES: [Side; 4] = [Side::North, Side::East, Side::South, Side::West];

/// Returns the rectangle obtained by extending `rect` one cell along `side`, or `None` if that
/// would move the anchor off the top/left edge of the grid.
fn extend(rect: Rect, side: Side) -> Option<Rect> {
    match side {
        Side::North => rect.y.checked_sub(1).map(|y| Rect {
            x: rect.x,
            y,
            w: rect.w,
            h: rect.h + 1,
        }),
        Side::South => Some(Rect {
            x: rect.x,
            y: rect.y,
            w: rect.w,
            h: rect.h + 1,
        }),
        Side::East => Some(Rect {
            x: rect.x,
            y: rect.y,
            w: rect.w + 1,
            h: rect.h,
        }),
        Side::West => rect.x.checked_sub(1).map(|x| Rect {
            x,
            y: rect.y,
            w: rect.w + 1,
            h: rect.h,
        }),
    }
}

/// Picks the rectangle to grow into. Tries each side N,E,S,W; a side qualifies only if it yields a
/// rectangle of exactly `target_area` whose newly-added cells are all claimable. Prefers a side
/// that merges a same-type neighbour; otherwise the first all-empty side. `None` blocks the upgrade.
fn choose_extension(
    world: &World,
    entity: Entity,
    kind: BuildingKind,
    current: Rect,
    target_area: usize,
) -> Option<Rect> {
    let mut first_empty: Option<Rect> = None;
    for side in SIDES {
        let Some(rect) = extend(current, side) else {
            continue;
        };
        if rect.area() != target_area {
            continue;
        }

        let mut merges_neighbour = false;
        let mut all_claimable = true;
        for (cx, cy) in rect.cells() {
            if current.contains_cell(cx, cy) {
                continue; // only the newly-added cells matter
            }
            match claim_kind(world, entity, kind, cx, cy, &rect) {
                Claim::Empty => {}
                Claim::SameType => merges_neighbour = true,
                Claim::Blocked => {
                    all_claimable = false;
                    break;
                }
            }
        }
        if !all_claimable {
            continue;
        }
        if merges_neighbour {
            return Some(rect); // same-type merge preferred over any empty side
        }
        if first_empty.is_none() {
            first_empty = Some(rect);
        }
    }
    first_empty
}

enum Claim {
    Empty,
    SameType,
    Blocked,
}

/// Whether the cell `(cx, cy)` can be claimed while keeping the footprint a rectangle: it must be
/// empty, or a same-type building whose entire footprint lies inside `new_rect` (no overhang).
fn claim_kind(
    world: &World,
    self_entity: Entity,
    kind: BuildingKind,
    cx: usize,
    cy: usize,
    new_rect: &Rect,
) -> Claim {
    if !world.grid.contains(cx, cy) {
        return Claim::Blocked;
    }
    match world.grid.get(cx, cy) {
        None => Claim::Empty,
        Some(other) if other == self_entity => Claim::Empty,
        Some(other) => {
            if world.buildings.get(&other).map(|building| building.kind) != Some(kind) {
                return Claim::Blocked;
            }
            match (
                world.positions.get(&other).copied(),
                world
                    .buildings
                    .get(&other)
                    .map(|building| building.footprint),
            ) {
                (Some(position), Some(footprint)) => {
                    let other_rect = Rect::new(
                        position.x,
                        position.y,
                        footprint.width as usize,
                        footprint.height as usize,
                    );
                    if new_rect.contains_rect(&other_rect) {
                        Claim::SameType
                    } else {
                        Claim::Blocked
                    }
                }
                _ => Claim::Blocked,
            }
        }
    }
}

/// Distinct neighbour entities occupying `rect` (other than `self_entity`). By construction of
/// `choose_extension` these are all same-type buildings fully inside the rectangle, to be merged.
fn same_type_entities_in(world: &World, self_entity: Entity, rect: Rect) -> Vec<Entity> {
    let mut neighbours = Vec::new();
    for (cx, cy) in rect.cells() {
        if let Some(other) = world.grid.get(cx, cy) {
            if other != self_entity && !neighbours.contains(&other) {
                neighbours.push(other);
            }
        }
    }
    neighbours
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::components::{
        Building, BuildingData, BusinessFinance, PollutionSource, Population,
    };

    fn place_zone(
        world: &mut World,
        x: usize,
        y: usize,
        kind: BuildingKind,
        data: BuildingData,
    ) -> Entity {
        let entity = world.spawn();
        world.attach_position(entity, Position { x, y });
        world.attach_building(
            entity,
            Building {
                kind,
                level: 1,
                data,
                footprint: Footprint::single(),
            },
        );
        world.grid.set(x, y, entity);
        entity
    }

    #[test]
    fn merging_a_residential_transfers_its_residents() {
        let mut world = World::new(4, 4);
        world.resources.money = 1000;
        let a = place_zone(
            &mut world,
            1,
            1,
            BuildingKind::Residential,
            BuildingData::None,
        );
        world.attach_population(a, Population { current: 0, max: 5 });
        let b = place_zone(
            &mut world,
            2,
            1,
            BuildingKind::Residential,
            BuildingData::None,
        );
        world.attach_population(b, Population { current: 0, max: 5 });
        citizens::spawn_for_home(&mut world, b, 3);

        // Upgrading A grows it east, merging the same-type B; B's residents move into A.
        assert!(upgrade(&mut world, 1, 1).success);

        assert!(!world.buildings.contains_key(&b), "B is absorbed");
        let a_residents = world
            .citizens
            .values()
            .filter(|citizen| citizen.home == a)
            .count();
        assert_eq!(a_residents, 3, "B's residents were re-homed to A, not lost");
    }

    #[test]
    fn merging_residents_over_capacity_drops_the_excess() {
        let mut world = World::new(4, 4);
        world.resources.money = 1000;
        let a = place_zone(
            &mut world,
            1,
            1,
            BuildingKind::Residential,
            BuildingData::None,
        );
        world.attach_population(a, Population { current: 0, max: 5 });
        let b = place_zone(
            &mut world,
            2,
            1,
            BuildingKind::Residential,
            BuildingData::None,
        );
        world.attach_population(b, Population { current: 0, max: 5 });
        // 10 + 10 = 20 residents; the merged 2-cell building caps at capacity_for(Residential, 2) = 15.
        citizens::spawn_for_home(&mut world, a, 10);
        citizens::spawn_for_home(&mut world, b, 10);

        assert!(upgrade(&mut world, 1, 1).success);

        let a_residents = world
            .citizens
            .values()
            .filter(|citizen| citizen.home == a)
            .count();
        assert_eq!(a_residents, 15, "capped at the new max population");
        assert_eq!(
            world.citizens.len(),
            15,
            "excess citizens were despawned, not left orphaned"
        );
        assert_eq!(
            world.populations.get(&a).unwrap().current,
            15,
            "population cache resynced to the capped count"
        );
    }

    #[test]
    fn merging_a_commercial_transfers_goods_and_cash() {
        let mut world = World::new(4, 4);
        world.resources.money = 1000;
        let a = place_zone(
            &mut world,
            1,
            1,
            BuildingKind::Commercial,
            BuildingData::Commercial {
                local_goods_stored: 2,
                business: BusinessFinance {
                    business_cash: 10,
                    ..BusinessFinance::default()
                },
            },
        );
        place_zone(
            &mut world,
            2,
            1,
            BuildingKind::Commercial,
            BuildingData::Commercial {
                local_goods_stored: 3,
                business: BusinessFinance {
                    business_cash: 7,
                    ..BusinessFinance::default()
                },
            },
        );

        assert!(upgrade(&mut world, 1, 1).success);

        match world.buildings.get(&a).unwrap().data {
            BuildingData::Commercial {
                local_goods_stored,
                business,
            } => {
                assert_eq!(local_goods_stored, 5, "goods summed (2 + 3)");
                assert_eq!(business.business_cash, 17, "cash summed (10 + 7)");
            }
            other => panic!("expected commercial data, got {other:?}"),
        }
    }

    #[test]
    fn manual_industrial_upgrade_to_level_three_sets_level_based_pollution() {
        // Regression: manual and business upgrades must agree on industrial pollution (level + 1).
        let mut world = World::new(4, 4);
        world.resources.money = 1000;
        let entity = world.spawn();
        world.attach_position(entity, Position { x: 1, y: 1 });
        world.attach_building(
            entity,
            Building {
                kind: BuildingKind::Industrial,
                level: 1,
                data: BuildingData::None,
                footprint: Footprint::single(),
            },
        );
        world.attach_pollution_source(entity, PollutionSource { amount: 2 });
        world.grid.set(1, 1, entity);

        // (1,1) has room to grow N then E into a 2x2, reaching level 3.
        assert!(upgrade(&mut world, 1, 1).success, "level 2");
        assert!(upgrade(&mut world, 1, 1).success, "level 3");

        assert_eq!(world.buildings.get(&entity).unwrap().level, 3);
        assert_eq!(world.pollution_sources.get(&entity).unwrap().amount, 4);
    }
}
