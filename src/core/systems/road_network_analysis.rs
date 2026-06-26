//! Derived road-network distances used by economy, happiness, and inspect explanations.
//!
//! P1 (pathfinding) adds `road_predecessors` — a destination-rooted, multi-source
//! Dijkstra that records `came_from` for path reconstruction. Edge weight is the
//! geometric `step_cost(current)` of the cell entered in the forward direction
//! (1 straight / 2 turn or T-junction / 4 four-way; see `step_cost`). See
//! `docs/traffic-pathfinding-plan.md` §7b P1 and `docs/travel-subtick-plan.md`.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};

use crate::core::entity::Entity;
use crate::core::systems::road_connectivity::{self, RoadNetwork};
use crate::core::world::World;
use crate::interface::input::BuildingKind;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct RoadNetworkAnalysis {
    pub building_access: HashMap<Entity, RoadAccess>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct RoadAccess {
    pub network_id: Option<u32>,
    pub commute_distance: Option<u32>,
    pub nearest_shop_distance: Option<u32>,
    pub goods_route_distance: Option<u32>,
    pub import_export_distance: Option<u32>,
}

pub(crate) fn run(world: &mut World) {
    world.road_analysis = analyze(world);
}

pub(crate) fn access_for(world: &World, entity: Entity) -> RoadAccess {
    world
        .road_analysis
        .building_access
        .get(&entity)
        .copied()
        .unwrap_or_default()
}

pub(crate) fn commute_penalty(distance: Option<u32>) -> i32 {
    match distance {
        Some(0..=4) => 0,
        Some(5..=8) => 1,
        Some(9..=14) => 2,
        Some(_) => 3,
        None => 0,
    }
}

pub(crate) fn shopping_happiness_modifier(distance: Option<u32>) -> i32 {
    match distance {
        Some(0..=4) => 1,
        Some(5..=10) => 0,
        Some(_) => -1,
        None => 0,
    }
}

pub(crate) fn route_margin_penalty(distance: Option<u32>) -> i32 {
    distance.map(|distance| (distance / 8) as i32).unwrap_or(0)
}

pub(crate) fn import_cost_penalty(distance: Option<u32>) -> i32 {
    distance.map(|distance| (distance / 8) as i32).unwrap_or(0)
}

pub(crate) fn distance_between_buildings(world: &World, from: Entity, to: Entity) -> Option<u32> {
    let from_access = access_for(world, from);
    let to_access = access_for(world, to);
    if from_access.network_id.is_none() || from_access.network_id != to_access.network_id {
        return None;
    }

    let networks = road_connectivity::discover_road_networks(world);
    let network = networks
        .iter()
        .find(|network| Some(network.id) == from_access.network_id)?;
    let to_roads = adjacent_roads_in_network(world, to, network);
    let distances = road_distances(world, network, &to_roads);
    let from_roads = adjacent_roads_in_network(world, from, network);
    nearest_distance(&from_roads, &distances)
}

fn analyze(world: &World) -> RoadNetworkAnalysis {
    let mut analysis = RoadNetworkAnalysis::default();
    for network in road_connectivity::discover_road_networks(world) {
        analyze_network(world, &network, &mut analysis);
    }
    analysis
}

fn analyze_network(world: &World, network: &RoadNetwork, analysis: &mut RoadNetworkAnalysis) {
    let commercial_roads = destination_roads(world, network, &[BuildingKind::Commercial]);
    let workplace_roads = destination_roads(
        world,
        network,
        &[BuildingKind::Commercial, BuildingKind::Industrial],
    );
    let industrial_roads = destination_roads(world, network, &[BuildingKind::Industrial]);
    let edge_roads = edge_roads(world, network);

    let commercial_distances = road_distances(world, network, &commercial_roads);
    let workplace_distances = road_distances(world, network, &workplace_roads);
    let industrial_distances = road_distances(world, network, &industrial_roads);
    let edge_distances = road_distances(world, network, &edge_roads);

    let mut buildings = connected_buildings(world, network);
    road_connectivity::sort_entities_by_position(world, &mut buildings);

    for building in buildings {
        let Some(kind) = world.buildings.get(&building).map(|building| building.kind) else {
            continue;
        };
        let adjacent_roads = adjacent_roads_in_network(world, building, network);
        let access = RoadAccess {
            network_id: Some(network.id),
            commute_distance: (kind == BuildingKind::Residential)
                .then(|| nearest_distance(&adjacent_roads, &workplace_distances))
                .flatten(),
            nearest_shop_distance: (kind == BuildingKind::Residential)
                .then(|| nearest_distance(&adjacent_roads, &commercial_distances))
                .flatten(),
            goods_route_distance: match kind {
                BuildingKind::Industrial => {
                    nearest_distance(&adjacent_roads, &commercial_distances)
                }
                BuildingKind::Commercial => {
                    nearest_distance(&adjacent_roads, &industrial_distances)
                }
                _ => None,
            },
            import_export_distance: matches!(
                kind,
                BuildingKind::Industrial | BuildingKind::Commercial
            )
            .then(|| nearest_distance(&adjacent_roads, &edge_distances))
            .flatten(),
        };
        analysis.building_access.insert(building, access);
    }
}

fn destination_roads(world: &World, network: &RoadNetwork, kinds: &[BuildingKind]) -> Vec<Entity> {
    let mut roads = Vec::new();
    let mut buildings: Vec<_> = world
        .buildings
        .iter()
        .filter_map(|(entity, building)| kinds.contains(&building.kind).then_some(*entity))
        .collect();
    road_connectivity::sort_entities_by_position(world, &mut buildings);

    for building in buildings {
        roads.extend(adjacent_roads_in_network(world, building, network));
    }
    roads.sort_by_key(|entity| {
        world
            .positions
            .get(entity)
            .map(|position| (position.y, position.x, entity.0))
            .unwrap_or((usize::MAX, usize::MAX, entity.0))
    });
    roads.dedup();
    roads
}

fn connected_buildings(world: &World, network: &RoadNetwork) -> Vec<Entity> {
    world
        .buildings
        .keys()
        .filter(|entity| !road_connectivity::is_road_entity(world, **entity))
        .filter(|entity| {
            road_connectivity::adjacent_road_entities(world, **entity)
                .any(|road| network.roads.contains(&road))
        })
        .copied()
        .collect()
}

pub(crate) fn adjacent_roads_in_network(
    world: &World,
    entity: Entity,
    network: &RoadNetwork,
) -> Vec<Entity> {
    let mut roads: Vec<_> = road_connectivity::adjacent_road_entities(world, entity)
        .filter(|road| network.roads.contains(road))
        .collect();
    road_connectivity::sort_entities_by_position(world, &mut roads);
    roads
}

fn edge_roads(world: &World, network: &RoadNetwork) -> Vec<Entity> {
    let max_x = world.grid.width().saturating_sub(1);
    let max_y = world.grid.height().saturating_sub(1);
    let mut roads: Vec<_> = network
        .roads
        .iter()
        .copied()
        .filter(|road| {
            world.positions.get(road).is_some_and(|position| {
                position.x == 0 || position.y == 0 || position.x == max_x || position.y == max_y
            })
        })
        .collect();
    road_connectivity::sort_entities_by_position(world, &mut roads);
    roads
}

fn road_distances(
    world: &World,
    network: &RoadNetwork,
    sources: &[Entity],
) -> HashMap<Entity, u32> {
    let source_set: HashSet<_> = sources.iter().copied().collect();
    let mut distances = HashMap::new();
    let mut queue = VecDeque::new();

    for source in sources {
        if network.roads.contains(source) && distances.insert(*source, 0).is_none() {
            queue.push_back(*source);
        }
    }

    while let Some(current) = queue.pop_front() {
        let distance = distances.get(&current).copied().unwrap_or(0);
        let mut neighbors: Vec<_> = road_connectivity::adjacent_road_entities(world, current)
            .filter(|neighbor| network.roads.contains(neighbor))
            .filter(|neighbor| !source_set.contains(neighbor) || *neighbor != current)
            .collect();
        road_connectivity::sort_entities_by_position(world, &mut neighbors);

        for neighbor in neighbors {
            if distances.contains_key(&neighbor) {
                continue;
            }
            distances.insert(neighbor, distance + 1);
            queue.push_back(neighbor);
        }
    }

    distances
}

/// Destination-rooted, multi-source Dijkstra that records `came_from` for
/// every road cell reachable from `sources` (P1, pathfinding).
///
/// `sources` are the destination's entry road cells. The search expands
/// outward and records `came_from[child] = parent` so the citizen can
/// reconstruct the path by walking the tree inward (one HashMap lookup per
/// tick — see the pathfinding plan §2a).
///
/// **Edge weight** — `step_cost(current)` (destination-rooted reverse search:
/// relaxing `current → neighbor` represents the forward step `neighbor →
/// current` toward the destination, so the cost charges the cell being entered,
/// which is `current`). `step_cost` is geometric: 1 for a straight pass, 2 for a
/// turn or T-junction, 4 for a 4-way (see `step_cost`). This makes paths prefer
/// fewer/cheaper crossings and turns.
///
/// **Determinism** — `BinaryHeap` does not guarantee pop order for equal
/// priorities, so the heap key is `Reverse<(cost, entity)>`. The tuple
/// orders equal-cost heap pops by entity id deterministically (lower entity
/// id pops first). It does not *directly* select parents, but it determines
/// which equal-cost relaxation is *recorded first*; strict `<` then
/// preserves that first parent.
///
/// **Sources** must be road cells in the `network` — sources outside the
/// network's `roads` set are ignored (the relax loop filters by
/// `network.roads.contains(...)`). Empty `sources` returns an empty
/// `HashMap`. Sources are roots and are absent from the returned `came_from`.
///
/// **Stale-heap skip** — `if cost != dist[current] { continue; }` ignores
/// stale heap entries from a relaxed update (a node may be in the heap with
/// an old cost before its `dist` is lowered).
#[allow(dead_code)] // P1 is a standalone patch; P2 wires this into the route cache.
pub(crate) fn road_predecessors(
    world: &World,
    network: &RoadNetwork,
    sources: &[Entity],
) -> HashMap<Entity, Entity> {
    road_predecessors_inner(world, network, sources).0
}

/// Shared implementation: returns `(came_from, dist)`. The cost is the geometric
/// `step_cost(current)` (1 straight / 2 turn or T-junction / 4 four-way).
/// Used by [`road_predecessors`] (production) and [`road_predecessors_with_dist`]
/// (test helper).
fn road_predecessors_inner(
    world: &World,
    network: &RoadNetwork,
    sources: &[Entity],
) -> (HashMap<Entity, Entity>, HashMap<Entity, u32>) {
    let mut came_from: HashMap<Entity, Entity> = HashMap::new();
    let mut dist: HashMap<Entity, u32> = HashMap::new();
    let mut heap: BinaryHeap<Reverse<(u32, Entity)>> = BinaryHeap::new();

    // Seed: source cells get distance 0 and are pushed onto the heap.
    // Sources outside the network are ignored (the relax loop filters by
    // network.roads anyway, but skipping them here avoids a useless entry
    // for a source that would never be reached).
    for source in sources {
        if network.roads.contains(source) && dist.insert(*source, 0).is_none() {
            heap.push(Reverse((0, *source)));
        }
    }

    while let Some(Reverse((cost, current))) = heap.pop() {
        // Stale-heap skip: this entry was pushed before a later relaxation
        // lowered `dist[current]`. The current top-of-heap value is stale.
        if cost != *dist.get(&current).unwrap_or(&u32::MAX) {
            continue;
        }

        // Relax neighbors. Destination-rooted: relaxing `current → neighbor`
        // represents the forward step `neighbor → current` toward the
        // destination, so the penalty charges `current`.
        let mut neighbors: Vec<_> = road_connectivity::adjacent_road_entities(world, current)
            .filter(|neighbor| network.roads.contains(neighbor))
            .collect();
        // Neighbour order does not affect correctness (the heap reorders by
        // key) or determinism (the heap key is (cost, entity_id)). Sort only
        // so the test is reproducible across runs without relying on
        // HashMap iteration order.
        road_connectivity::sort_entities_by_position(world, &mut neighbors);

        // `neighbors` are exactly the road neighbors of `current` in the
        // network, so the degree is `neighbors.len()` (no second scan).
        let degree = neighbors.len() as u32;
        // `current`'s fixed forward direction in the tree (toward the
        // destination); `None` at a source/root. The turn at `current` is
        // between the incoming edge `neighbor → current` and this exit.
        let forward = came_from.get(&current).copied();

        for neighbor in neighbors {
            // Destination-rooted reverse search: relaxing `current → neighbor`
            // represents the forward step `neighbor → current` toward the
            // destination, so the cost charges `current` (the cell entered in
            // the forward direction). `step_cost` makes a turn cost as much as a
            // T-junction (see its docs).
            let nd = cost + step_cost(world, Some(neighbor), current, forward, degree);
            if nd < *dist.get(&neighbor).unwrap_or(&u32::MAX) {
                dist.insert(neighbor, nd);
                came_from.insert(neighbor, current);
                heap.push(Reverse((nd, neighbor)));
            }
        }
    }

    (came_from, dist)
}

/// Cost (in travel sub-ticks) to traverse `current`, entering from `in_cell` and
/// leaving toward `out_cell`. Geometric, not just degree-based:
///
/// ```text
///   degree ≥ 4                            → 4   4-way intersection
///   degree = 3  OR  in ⊥ out (a 90° turn) → 2   T-junction or corner
///   else (straight pass, in ∥ out)        → 1   straight / dead-end / arrival
/// ```
///
/// A turn needs both endpoints, so `in_cell`/`out_cell` are `Option`: at a
/// destination-root (`out_cell = None`) or the very first step from a building
/// (`in_cell = None`) there is no turn, only the base/degree cost.
///
/// The single source of truth for both the P1 routing weight and the per-cell
/// dwell the mover pays, so the route the cache prefers is a strong heuristic for
/// the fastest one (see `docs/travel-subtick-plan.md`).
pub(crate) fn step_cost(
    world: &World,
    in_cell: Option<Entity>,
    current: Entity,
    out_cell: Option<Entity>,
    degree: u32,
) -> u32 {
    if degree >= 4 {
        return 4;
    }
    let turns = match (in_cell, out_cell) {
        (Some(i), Some(o)) => !collinear(world, i, current, o),
        _ => false,
    };
    if degree == 3 || turns { 2 } else { 1 }
}

/// Whether `b` is the straight midpoint between orthogonally-adjacent road cells
/// `a` and `c` — i.e. entering `b` from `a` and leaving to `c` is a straight pass,
/// not a 90° turn. `false` (treated as a turn) if any position is missing.
fn collinear(world: &World, a: Entity, b: Entity, c: Entity) -> bool {
    let (Some(pa), Some(pb), Some(pc)) = (
        world.positions.get(&a),
        world.positions.get(&b),
        world.positions.get(&c),
    ) else {
        return false;
    };
    // `a` and `c` are opposite neighbours of `b` ⇔ `b` is their midpoint.
    pa.x + pc.x == 2 * pb.x && pa.y + pc.y == 2 * pb.y
}

/// Number of road cells in `network` that are orthogonally adjacent to
/// `road_entity` — the cell's degree, fed to `step_cost` (3 → T-junction,
/// ≥ 4 → 4-way). A degree-2 cell is straight *or* a corner; `step_cost`
/// distinguishes those by direction, not by degree.
pub(crate) fn road_degree_in_network(
    world: &World,
    road_entity: Entity,
    network: &RoadNetwork,
) -> u32 {
    road_connectivity::adjacent_road_entities(world, road_entity)
        .filter(|neighbor| network.roads.contains(neighbor))
        .count() as u32
}

/// Test helper: returns the `dist` map (shortest cost from any source to each
/// reachable cell) alongside the `came_from` tree. Thin wrapper over the
/// shared [`road_predecessors_inner`]; exists so tests can assert on the
/// cost values directly. `pub(crate)` for test access; not part of the P1
/// production API.
#[allow(dead_code)] // P1 standalone; helper is used by tests.
pub(crate) fn road_predecessors_with_dist(
    world: &World,
    network: &RoadNetwork,
    sources: &[Entity],
) -> (HashMap<Entity, Entity>, HashMap<Entity, u32>) {
    road_predecessors_inner(world, network, sources)
}

fn nearest_distance(roads: &[Entity], distances: &HashMap<Entity, u32>) -> Option<u32> {
    roads
        .iter()
        .filter_map(|road| distances.get(road).copied())
        .min()
}

#[cfg(test)]
mod tests {
    use super::{access_for, run};
    use crate::core::components::{Building, BuildingData, Footprint, Position};
    use crate::core::world::World;
    use crate::interface::input::BuildingKind;

    #[test]
    fn analysis_finds_nearest_destinations_on_same_road_network() {
        let mut world = World::new(6, 3);
        let home = place(&mut world, 1, 0, BuildingKind::Residential);
        let commercial = place(&mut world, 4, 0, BuildingKind::Commercial);
        for x in 1..=4 {
            place(&mut world, x, 1, BuildingKind::Road);
        }

        run(&mut world);

        let home_access = access_for(&world, home);
        let commercial_access = access_for(&world, commercial);
        assert_eq!(home_access.nearest_shop_distance, Some(3));
        assert_eq!(home_access.commute_distance, Some(3));
        assert_eq!(commercial_access.import_export_distance, None);
    }

    #[test]
    fn analysis_leaves_disconnected_destinations_unreachable() {
        let mut world = World::new(6, 3);
        let home = place(&mut world, 1, 0, BuildingKind::Residential);
        place(&mut world, 4, 0, BuildingKind::Commercial);
        place(&mut world, 1, 1, BuildingKind::Road);
        place(&mut world, 4, 1, BuildingKind::Road);

        run(&mut world);

        let home_access = access_for(&world, home);
        assert_eq!(home_access.nearest_shop_distance, None);
        assert_eq!(home_access.commute_distance, None);
    }

    #[test]
    fn analysis_finds_edge_access_through_roads() {
        let mut world = World::new(5, 3);
        let industrial = place(&mut world, 3, 0, BuildingKind::Industrial);
        for x in 0..=3 {
            place(&mut world, x, 1, BuildingKind::Road);
        }

        run(&mut world);

        assert_eq!(
            access_for(&world, industrial).import_export_distance,
            Some(3)
        );
    }

    // P1 tests (pathfinding) — destination-rooted Dijkstra with crossing penalty.

    use super::{
        Entity, RoadNetwork, road_degree_in_network, road_predecessors, road_predecessors_with_dist,
    };
    use crate::core::systems::road_connectivity::discover_road_networks;

    /// Helper: place a single road cell and return its entity.
    fn place_road(world: &mut World, x: usize, y: usize) -> crate::core::entity::Entity {
        place(world, x, y, BuildingKind::Road)
    }

    /// Helper: build a single-network `Vec<RoadNetwork>` and return the only
    /// network (panics if there's not exactly one).
    fn single_network(world: &World) -> RoadNetwork {
        let networks = discover_road_networks(world);
        assert_eq!(networks.len(), 1, "expected exactly one road network");
        networks.into_iter().next().unwrap()
    }

    /// P1: determinism. Two calls with the same input produce structurally
    /// equal `came_from` trees, and the tree routes the path correctly.
    #[test]
    fn road_predecessors_deterministic_across_runs() {
        let mut world = World::new(5, 1);
        // Linear road A-B-C-D-E (left to right); source = E (rightmost).
        let _a = place_road(&mut world, 0, 0);
        let _b = place_road(&mut world, 1, 0);
        let _c = place_road(&mut world, 2, 0);
        let _d = place_road(&mut world, 3, 0);
        let e = place_road(&mut world, 4, 0);
        let network = single_network(&world);

        let first = road_predecessors(&world, &network, &[e]);
        let second = road_predecessors(&world, &network, &[e]);

        // Two calls produce equal trees (determinism).
        assert_eq!(first, second);

        // Sanity: the tree routes D → E (D's parent is E), C → D, B → C, A → B.
        // Entity ids are spawned left-to-right: 0, 1, 2, 3, 4.
        let a_id = entity_at(&world, 0, 0);
        let b_id = entity_at(&world, 1, 0);
        let c_id = entity_at(&world, 2, 0);
        let d_id = entity_at(&world, 3, 0);
        let e_id = entity_at(&world, 4, 0);
        assert!(!first.contains_key(&e_id), "source E must be absent");
        assert_eq!(first.get(&d_id), Some(&e_id), "D → E");
        assert_eq!(first.get(&c_id), Some(&d_id), "C → D");
        assert_eq!(first.get(&b_id), Some(&c_id), "B → C");
        assert_eq!(first.get(&a_id), Some(&b_id), "A → B");
    }

    fn entity_at(world: &World, x: usize, y: usize) -> Entity {
        world.grid.get(x, y).expect("entity at cell")
    }

    /// P1: crossing penalty. Verify the algorithm charges the penalty on a
    /// degree-4 cell (4-way intersection) by checking the cost map. Build a
    /// "+" shape where the center has 4 road neighbors and a single source.
    /// The cost of reaching X from the source arm is 1 (no penalty on the
    /// arm, which is degree 1). The cost of reaching the opposite arm via X
    /// is `1 + (1 + penalty(X)) = 1 + 3 = 4` (penalty 2 because X is
    /// degree 4). If the penalty were not applied, the opposite arm would
    /// cost 2.
    #[test]
    fn road_predecessors_crossing_penalty_charged() {
        // Layout (3x3 grid, 5 roads — a "+" shape):
        //   row 0:  . R .
        //   row 1:  R X R
        //   row 2:  . R .
        //
        // X is degree 4 (all 4 arm cells are road neighbors). Arm cells (R)
        // are degree 1 (only X is a road neighbor — the opposite arm is
        // not orthogonally adjacent).
        let mut world = World::new(3, 3);
        let r_north = place_road(&mut world, 1, 0);
        let r_east = place_road(&mut world, 2, 1);
        let r_south = place_road(&mut world, 1, 2);
        let r_west = place_road(&mut world, 0, 1);
        let x = place_road(&mut world, 1, 1);
        let network = single_network(&world);

        // Sanity: verify the topology before testing the algorithm.
        let degree_x = road_degree_in_network(&world, x, &network);
        assert_eq!(degree_x, 4, "X should be a 4-way intersection");
        let degree_arm = road_degree_in_network(&world, r_north, &network);
        assert_eq!(degree_arm, 1, "arm cells should be degree 1 (leaves)");

        let (_, dist) = road_predecessors_with_dist(&world, &network, &[r_north]);

        // The source has cost 0.
        assert_eq!(dist.get(&r_north), Some(&0));

        // X is reached from the source. The reverse relaxation goes from
        // r_north to x with edge weight `step_cost(current)` where current =
        // r_north (degree 1 → cost 1). So dist[x] = 0 + 1 = 1. X's own (4-way)
        // cost is charged when LEAVING X in reverse = ENTERING X forward.
        let dist_x = dist.get(&x).copied().expect("X must be reached");
        assert_eq!(
            dist_x, 1,
            "X should be reached with cost 1 (from r_north, no penalty on r_north); \
             the penalty on X is charged on the next step"
        );

        // The other R's are reached via X. The reverse relaxation goes from
        // x to r_south with edge weight `step_cost(X) = 4` (X is a 4-way, so the
        // geometric cost is 4 regardless of the turn). So dist[r_south] =
        // dist[x] + 4 = 1 + 4 = 5. (Under the old `1 + crossing_penalty` model
        // this was 1 + 3 = 4.)
        let dist_south = dist
            .get(&r_south)
            .copied()
            .expect("south R must be reached");
        assert_eq!(
            dist_south, 5,
            "south R should be reached with cost 5 (through the 4-way X, cost 4)"
        );
        // r_east and r_west are reached the same way (both arm cells have
        // the same cost from the source through X).
        assert_eq!(
            dist.get(&r_east).copied(),
            Some(5),
            "east R should also have cost 5 (symmetric arm)"
        );
        assert_eq!(
            dist.get(&r_west).copied(),
            Some(5),
            "west R should also have cost 5 (symmetric arm)"
        );
    }

    /// P7a: a 90° turn at a degree-2 cell costs 2× (like a T-junction), while a
    /// straight pass through a degree-2 cell costs 1×.
    #[test]
    fn road_predecessors_turn_costs_like_a_junction() {
        // L-shape: A(0,0) ─ B(1,0) ─ C(1,1). At B the path turns (west↔south).
        let mut corner = World::new(2, 2);
        let a = place_road(&mut corner, 0, 0);
        let b = place_road(&mut corner, 1, 0);
        let c = place_road(&mut corner, 1, 1);
        let corner_net = single_network(&corner);
        let (_, dist) = road_predecessors_with_dist(&corner, &corner_net, &[c]);
        // C→B straight-into-the-corner cost 1 (C is degree 1); B→A turns → cost 2.
        assert_eq!(dist.get(&b), Some(&1), "B reached from C, cost 1");
        assert_eq!(
            dist.get(&a),
            Some(&3),
            "A reached through the turn at B (1 + turn 2 = 3)"
        );

        // Straight line A(0,0) ─ B(1,0) ─ C(2,0): B is a straight pass, cost 1.
        let mut straight = World::new(3, 1);
        let a2 = place_road(&mut straight, 0, 0);
        let _b2 = place_road(&mut straight, 1, 0);
        let c2 = place_road(&mut straight, 2, 0);
        let straight_net = single_network(&straight);
        let (_, dist2) = road_predecessors_with_dist(&straight, &straight_net, &[c2]);
        assert_eq!(
            dist2.get(&a2),
            Some(&2),
            "A reached through a straight B (1 + 1 = 2), cheaper than the turn"
        );
    }

    /// P1: unreachable / empty sources. Empty `sources` returns an empty
    /// tree (no cells were ever reached). Sources outside the network are
    /// ignored.
    #[test]
    fn road_predecessors_empty_or_foreign_sources_yield_empty_tree() {
        let mut world = World::new(3, 1);
        let a = place_road(&mut world, 0, 0);
        let b = place_road(&mut world, 1, 0);
        let c = place_road(&mut world, 2, 0);
        let network = single_network(&world);

        // Empty sources → empty tree.
        let tree = road_predecessors(&world, &network, &[]);
        assert!(tree.is_empty());

        // Foreign-network source: an entity not in the network's roads set
        // is ignored. (The existing `road_distances` does the same filtering.)
        let foreign = world.spawn();
        let tree = road_predecessors(&world, &network, &[foreign]);
        assert!(tree.is_empty(), "foreign source should be ignored");

        // Sanity: a real source returns a non-empty tree.
        let tree = road_predecessors(&world, &network, &[c]);
        assert!(!tree.is_empty());
        // c is the source — absent from came_from.
        assert!(!tree.contains_key(&c));
        // a and b are both reached (one step from c each).
        assert!(tree.contains_key(&a), "a must be reached");
        assert!(tree.contains_key(&b), "b must be reached");
    }

    /// P1: cross-network filtering. Sources from network 2 are ignored when
    /// called on network 1.
    #[test]
    fn road_predecessors_cross_network_source_is_ignored() {
        // Two disconnected road networks:
        //   network 1: row 0 (horizontal road)
        //   network 2: row 2 (horizontal road)
        let mut world = World::new(3, 3);
        let n1_a = place_road(&mut world, 0, 0);
        let n1_b = place_road(&mut world, 1, 0);
        let n1_c = place_road(&mut world, 2, 0);
        let n2_a = place_road(&mut world, 0, 2);
        let n2_b = place_road(&mut world, 1, 2);
        let n2_c = place_road(&mut world, 2, 2);

        // The two networks are disconnected (no vertical road between row 0 and row 2).
        let networks = discover_road_networks(&world);
        assert_eq!(networks.len(), 2, "expected two disconnected networks");

        // Get network 1 (its id is 0 or 1 depending on discovery order).
        let net1 = networks.iter().find(|n| n.roads.contains(&n1_a)).unwrap();

        // Foreign source: pass an n2 cell to network 1. It must be ignored.
        let tree = road_predecessors(&world, net1, &[n2_a]);
        assert!(
            tree.is_empty(),
            "n2 source must be ignored when calling on n1"
        );
        // Network 1 cells are also absent (no valid source).
        assert!(!tree.contains_key(&n1_a));
        assert!(!tree.contains_key(&n1_b));
        assert!(!tree.contains_key(&n1_c));

        // Real source: n1_c on n1 returns n1 cells only (n2 cells are not
        // reached because they're not in the network's roads set).
        let tree = road_predecessors(&world, net1, &[n1_c]);
        assert!(tree.contains_key(&n1_a));
        assert!(tree.contains_key(&n1_b));
        assert!(
            !tree.contains_key(&n2_a),
            "n2 cells must not be reached from n1"
        );
        assert!(!tree.contains_key(&n2_b));
        assert!(!tree.contains_key(&n2_c));
    }

    /// P1: multi-source. Two destination entry cells as sources — both are
    /// absent from `came_from`. An ambiguous cell (directly adjacent to both
    /// sources with equal cost and equal penalty) records the lower-entity-id
    /// source as its parent.
    #[test]
    fn road_predecessors_multi_source_deterministic_tie_break() {
        // Linear road: A — B — C. Sources = {A, C}. B is directly adjacent
        // to both, with equal cost (1 hop) and equal penalty (B is degree 2).
        let mut world = World::new(3, 1);
        let a = place_road(&mut world, 0, 0);
        let b = place_road(&mut world, 1, 0);
        let c = place_road(&mut world, 2, 0);
        let network = single_network(&world);

        let tree = road_predecessors(&world, &network, &[a, c]);

        // Both sources are absent (roots have no parent).
        assert!(!tree.contains_key(&a), "source a must be absent");
        assert!(!tree.contains_key(&c), "source c must be absent");

        // B's parent is the lower-entity-id source. The reverse
        // relaxation charges the source cell being entered (A or C are
        // degree 1 → no penalty; B is degree 2 → no penalty; the only
        // cost difference is the entity-id tie-break). Since a and c are
        // 0-indexed and spawned in order, a.0 < c.0, so the canonical
        // parent is a (the first source in entity-id order).
        let parent = tree.get(&b).expect("b must have a parent");
        assert_eq!(
            *parent, a,
            "tie-break should pick the lower-entity-id source"
        );
    }

    fn place(
        world: &mut World,
        x: usize,
        y: usize,
        kind: BuildingKind,
    ) -> crate::core::entity::Entity {
        let entity = world.spawn();
        world.attach_position(entity, Position { x, y });
        world.attach_building(
            entity,
            Building {
                kind,
                level: 1,
                data: BuildingData::None,
                footprint: Footprint::single(),
            },
        );
        assert!(world.grid.set(x, y, entity));
        entity
    }
}
