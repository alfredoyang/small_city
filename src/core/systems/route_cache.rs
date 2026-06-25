//! Region-owned route cache — destination-rooted `came_from` trees keyed by
//! `(destination, road_network_id)` (P2, pathfinding).
//!
//! A path is a pure function of `(road graph, origin cell, destination cell)`
//! and a single road network, so trees are deduped per destination rather than
//! per citizen. The cache lives on `World` as a `#[serde(skip)]` `RefCell`
//! (derived state — not persisted) and is recomputed lazily on miss.
//!
//! **Cache key.** `(destination, road_network_id)` — the crossing penalty is
//! a compile-time const, so it doesn't enter the key. The network id matters
//! because a destination can touch roads from more than one disconnected
//! local road network; those trees must not collapse into one entry.
//!
//! **Destination types.**
//! - A non-road **building** (home, workplace, …): sources are the building's
//!   adjacent road cells in `network`.
//! - A **road entity** (P5 border-exit routing): the source is the road cell
//!   itself. `routes_to` treats `dest` as a single-element source list
//!   `[dest]` when `dest` is a road entity.
//!
//! **Invalidation.** System-level, chokepoint-specific (see §3 of the
//! pathfinding plan): `RouteCache::clear()` for road changes (any tree might
//! be affected) and `RouteCache::evict(dest)` for building changes (only this
//! destination's trees are affected).

use std::collections::HashMap;

use crate::core::entity::Entity;

#[derive(Debug, Default)]
pub(crate) struct RouteCache {
    trees: HashMap<(Entity, u32), HashMap<Entity, Entity>>,
}

impl RouteCache {
    /// Coarse clear — drop every tree. Called on road topology change,
    /// because a new road can connect previously-disconnected areas and a
    /// removed road can disconnect them; the affected set isn't computable
    /// from a single road change alone.
    pub fn clear(&mut self) {
        self.trees.clear();
    }

    /// Per-destination eviction — drop every tree whose key's destination is
    /// `dest`. Called on building change (bulldoze, upgrade footprint growth),
    /// because the building's destination entry roads and reachability may
    /// have changed. Scans all networks for that destination.
    pub fn evict(&mut self, dest: Entity) {
        self.trees.retain(|(d, _), _| *d != dest);
    }

    /// Cache lookup or compute. On miss, runs `compute` and inserts the
    /// result; on hit, returns the cached tree. The returned `&HashMap`
    /// borrows from `self` for the duration of the borrow.
    ///
    /// Used by `World::routes_to` to wire the cache into the road-predecessor
    /// algorithm. The closure is called at most once per cache miss.
    pub fn get_or_compute<F>(&mut self, key: (Entity, u32), compute: F) -> &HashMap<Entity, Entity>
    where
        F: FnOnce() -> HashMap<Entity, Entity>,
    {
        self.trees.entry(key).or_insert_with(compute)
    }

    /// Read-only lookup. Returns `None` if the key isn't in the cache.
    /// Used by `World::routes_to` to return a `Ref` after `get_or_compute`
    /// has populated the cache.
    pub fn get(&self, key: &(Entity, u32)) -> Option<&HashMap<Entity, Entity>> {
        self.trees.get(key)
    }

    /// Whether `key` is currently in the cache. Test helper for
    /// verifying chokepoint invalidation (asserts selective eviction by
    /// checking key presence before and after the operation).
    #[cfg(test)]
    pub fn contains(&self, key: &(Entity, u32)) -> bool {
        self.trees.contains_key(key)
    }
}

#[cfg(test)]
mod tests {
    //! P2 route-cache tests — exercise `RouteCache` directly and the
    //! `World::routes_to` accessor with chokepoint invalidation.
    //!
    //! `RouteCache` is a small, pure data structure; the interesting logic
    //! lives in `World::routes_to` (source selection, cache wiring) and the
    //! chokepoints (clear on road change, evict on building change). The
    //! tests below cover both layers.

    use std::collections::{HashMap, HashSet};

    use crate::core::entity::Entity;
    use crate::core::systems::entity_cleanup::remove_entity;
    use crate::core::systems::placement::place_building;
    use crate::core::world::World;
    use crate::interface::input::BuildingKind;

    use super::RouteCache;

    /// 4-cell road (left to right at row 0) with one residential building
    /// at (0, 1) touching the leftmost road. The road is one connected
    /// network; the building is adjacent to the road at (0, 0).
    fn linear_road_with_house() -> (World, Entity, Entity) {
        let mut world = World::new(4, 2);
        place_building(&mut world, 0, 0, BuildingKind::Road);
        place_building(&mut world, 1, 0, BuildingKind::Road);
        place_building(&mut world, 2, 0, BuildingKind::Road);
        place_building(&mut world, 3, 0, BuildingKind::Road);
        place_building(&mut world, 0, 1, BuildingKind::Residential);
        let r0 = world.grid.get(0, 0).expect("r0 placed");
        let house = world.grid.get(0, 1).expect("house placed");
        (world, house, r0)
    }

    /// The road network containing the cell at `(0, 0)`.
    fn first_network(world: &World) -> crate::core::systems::road_connectivity::RoadNetwork {
        let networks = crate::core::systems::road_connectivity::discover_road_networks(world);
        assert_eq!(networks.len(), 1, "expected exactly one network");
        networks.into_iter().next().unwrap()
    }

    /// P2: `RouteCache::clear` drops every entry.
    #[test]
    fn route_cache_clear_drops_every_entry() {
        let mut cache = RouteCache::default();
        let dest = Entity(0);
        let tree: HashMap<Entity, Entity> = HashMap::new();
        cache.get_or_compute((dest, 0), || tree.clone());
        cache.get_or_compute((dest, 1), || tree.clone());
        cache.get_or_compute((Entity(1), 0), || tree.clone());
        assert_eq!(cache.trees.len(), 3);

        cache.clear();
        assert!(cache.trees.is_empty());
    }

    /// P2: `RouteCache::evict` drops only entries for that destination.
    #[test]
    fn route_cache_evict_drops_only_matching_destination() {
        let mut cache = RouteCache::default();
        let dest_a = Entity(0);
        let dest_b = Entity(1);
        let tree: HashMap<Entity, Entity> = HashMap::new();
        cache.get_or_compute((dest_a, 0), || tree.clone());
        cache.get_or_compute((dest_a, 1), || tree.clone());
        cache.get_or_compute((dest_b, 0), || tree.clone());
        assert_eq!(cache.trees.len(), 3);

        cache.evict(dest_a);
        // Both `dest_a` entries gone; `dest_b` survives.
        assert!(!cache.trees.contains_key(&(dest_a, 0)));
        assert!(!cache.trees.contains_key(&(dest_a, 1)));
        assert!(cache.trees.contains_key(&(dest_b, 0)));
    }

    /// P2: `get_or_compute` runs the closure exactly once on miss, then
    /// reuses the cached result.
    #[test]
    fn get_or_compute_runs_closure_only_on_miss() {
        use std::cell::Cell;
        let mut cache = RouteCache::default();
        let key = (Entity(7), 0);
        let call_count = Cell::new(0);
        // Use a `Cell` so the closure can be called multiple times (it's
        // `Fn`, not `FnOnce`). `or_insert_with` takes `FnOnce` and
        // consumes the closure on the first miss; subsequent hits don't
        // call it at all.
        let compute = || {
            call_count.set(call_count.get() + 1);
            let mut tree = HashMap::new();
            tree.insert(Entity(1), Entity(2));
            tree
        };

        // First call: miss, closure runs.
        let _ = cache.get_or_compute(key, compute);
        assert_eq!(call_count.get(), 1, "closure runs on first miss");
        // Second and third calls: hit, closure does not run.
        let _ = cache.get_or_compute(key, compute);
        let _ = cache.get_or_compute(key, compute);
        assert_eq!(
            call_count.get(),
            1,
            "closure must not run on subsequent hits"
        );
    }

    /// P2: `World::routes_to` returns a valid tree for a building destination.
    #[test]
    fn routes_to_building_returns_came_from_tree() {
        let (world, house, r0) = linear_road_with_house();
        let network = first_network(&world);

        let tree = world.routes_to(house, &network);
        // House is adjacent to road at (0, 0) — that's the only source, so
        // r0 is absent from came_from. The other 3 roads (r1, r2, r3) are
        // reached and have parents pointing back toward r0.
        assert!(
            !tree.contains_key(&r0),
            "r0 is the source, must be absent from came_from"
        );
        // House itself is not on the road graph, so it must NOT be in the tree.
        assert!(
            !tree.contains_key(&house),
            "house is not a road cell, must not appear in came_from"
        );
        // Tree has 3 entries (one per non-source road).
        assert_eq!(tree.len(), 3, "tree should reach the 3 non-source roads");
        // Sanity: r1's parent is r0 (the only source).
        let r1 = world.grid.get(1, 0).expect("r1 placed");
        assert_eq!(tree.get(&r1), Some(&r0), "r1 came_from r0");
    }

    /// P2: `World::routes_to` returns a valid tree for a road destination
    /// (P5 border-exit routing).
    #[test]
    fn routes_to_road_entity_uses_self_as_source() {
        let (world, _house, _r0) = linear_road_with_house();
        let network = first_network(&world);

        // r3 is the rightmost road. Routes to r3 should have r3 as the
        // single source — it is absent from came_from, and all other roads
        // point back toward r3.
        let tree = world.routes_to(r3_entity(&world), &network);
        assert!(!tree.contains_key(&r3_entity(&world)), "source r3 absent");
        // r0, r1, r2 are all reached (each has a parent closer to r3).
        assert_eq!(tree.len(), 3, "tree should reach the 3 non-source roads");
    }

    fn r3_entity(world: &World) -> Entity {
        world.grid.get(3, 0).expect("entity at (3, 0)")
    }

    /// P2: `routes_to` is deterministic — two calls with the same input
    /// produce structurally equal trees. The "no recompute on hit" claim
    /// is covered by `get_or_compute_runs_closure_only_on_miss` (the
    /// closure is `road_predecessors`); this test verifies the accessor
    /// round-trips correctly.
    #[test]
    fn routes_to_is_deterministic() {
        let (world, house, _r0) = linear_road_with_house();
        let network = first_network(&world);

        // Snapshot the first tree (clone the inner HashMap, drop the Ref).
        let first_snapshot: HashMap<Entity, Entity> = {
            let first = world.routes_to(house, &network);
            first.clone()
        };

        // Second call: should return the same tree (deterministic result).
        let second = world.routes_to(house, &network);
        assert_eq!(
            first_snapshot, *second,
            "two calls must return identical tree"
        );
    }

    /// P2: `placement::place_building` for a road coarse-clears the cache.
    /// The next `routes_to` call must recompute.
    #[test]
    fn placing_a_road_coarse_clears_the_cache() {
        let (mut world, house, _r0) = linear_road_with_house();
        let network = first_network(&world);

        // Populate the cache.
        let _ = world.routes_to(house, &network);
        // Place a new road — this should coarse-clear the cache.
        place_building(&mut world, 3, 1, BuildingKind::Road);
        // Recompute the network (the new road is adjacent to r3, so it's
        // in the same network).
        let network = first_network(&world);
        let tree = world.routes_to(house, &network);
        // The new road at (3, 1) is adjacent to the road at (3, 0) — but
        // (3, 1) is a road, not the house. The tree's reachability from
        // the house should now include the new road cell at (3, 1).
        let new_road_entity = world.grid.get(3, 1).expect("new road placed");
        assert!(
            tree.contains_key(&new_road_entity),
            "new road cell must be reached after placement+recompute"
        );
    }

    /// P2: `entity_cleanup::remove_entity` for a building per-destination
    /// evicts the cache. Other destinations' trees survive.
    #[test]
    fn bulldozing_a_building_per_destination_evicts() {
        let (mut world, house, _r0) = linear_road_with_house();
        let network = first_network(&world);

        // Populate the cache for the house.
        let _ = world.routes_to(house, &network);
        // Add a second destination (a commercial building adjacent to r2).
        place_building(&mut world, 2, 1, BuildingKind::Commercial);
        let commercial = world.grid.get(2, 1).expect("commercial placed");
        let _ = world.routes_to(commercial, &network);

        // Sanity: both keys are cached before the bulldoze.
        assert!(
            world.route_cache_contains(house, network.id),
            "house should be cached before bulldoze"
        );
        assert!(
            world.route_cache_contains(commercial, network.id),
            "commercial should be cached before bulldoze"
        );

        // Bulldoze the house — should evict the house's entries but keep
        // the commercial's.
        remove_entity(&mut world, house, 0, 1);

        // Selective eviction: house's key is gone, commercial's survives.
        assert!(
            !world.route_cache_contains(house, network.id),
            "house key must be evicted after bulldoze"
        );
        assert!(
            world.route_cache_contains(commercial, network.id),
            "commercial key must survive house eviction (per-dest, not coarse clear)"
        );
    }

    /// P2: `entity_cleanup::remove_entity` for a road coarse-clears the cache.
    #[test]
    fn bulldozing_a_road_coarse_clears_the_cache() {
        let (mut world, house, _r0) = linear_road_with_house();
        let network = first_network(&world);

        // Populate the cache.
        let _ = world.routes_to(house, &network);
        // Bulldoze r2 (the middle road) — this splits the network in two,
        // and should coarse-clear the cache.
        let r2 = world.grid.get(2, 0).expect("r2 exists");
        remove_entity(&mut world, r2, 2, 0);

        // After removal, the network splits into {r0, r1} and {r3}. The
        // house is adjacent to r0, so it's on the {r0, r1} network. The
        // tree for the house should reach only r0 and r1 (2 cells).
        let networks = crate::core::systems::road_connectivity::discover_road_networks(&world);
        assert_eq!(networks.len(), 2, "expected two networks after split");
        let house_network = networks
            .iter()
            .find(|n| n.roads.contains(&world.grid.get(0, 0).unwrap()))
            .expect("house network must exist");
        let tree = world.routes_to(house, house_network);
        assert_eq!(
            tree.len(),
            1,
            "tree should reach only r1 (r0 is the source, r3 is in a different network)"
        );
    }

    /// P2: a destination in a network different from the one passed
    /// returns an empty tree (the caller must pass the right network).
    #[test]
    fn routes_to_with_wrong_network_returns_empty_tree() {
        let (world, house, _r0) = linear_road_with_house();
        // Build a fake network that doesn't contain the house's adjacent
        // roads. We use network_id 999 to simulate this.
        let fake_network = crate::core::systems::road_connectivity::RoadNetwork {
            id: 999,
            roads: HashSet::new(),
        };
        let tree = world.routes_to(house, &fake_network);
        assert!(tree.is_empty(), "wrong network must produce an empty tree");
    }

    /// P2: `upgrade::grow_to_level` per-destination evicts the surviving
    /// building when its footprint grows. The building's destination entry
    /// cells may have moved (or the building may now touch different road
    /// networks), so its cache entries must be dropped.
    #[test]
    fn grow_to_level_evicts_surviving_building() {
        use crate::core::components::{BuildingData, BusinessFinance, Footprint};
        use crate::core::systems::upgrade::grow_to_level;

        let mut world = World::new(4, 4);
        // Build a 4-cell road (row 1) with a commercial building at (1, 0)
        // that can grow east into (2, 0). The commercial's adjacent road
        // is at (1, 1).
        place_building(&mut world, 0, 1, BuildingKind::Road);
        place_building(&mut world, 1, 1, BuildingKind::Road);
        place_building(&mut world, 2, 1, BuildingKind::Road);
        place_building(&mut world, 3, 1, BuildingKind::Road);
        place_building(&mut world, 1, 0, BuildingKind::Commercial);
        let commercial = world.grid.get(1, 0).expect("commercial placed");

        // Give the commercial the finance data it needs to be upgradeable.
        if let Some(building) = world.buildings.get_mut(&commercial) {
            building.data = BuildingData::Commercial {
                local_goods_stored: 0,
                business: BusinessFinance::default(),
            };
        }

        let network = first_network(&world);
        // Populate the cache.
        let _ = world.routes_to(commercial, &network);
        assert!(
            world.route_cache_contains(commercial, network.id),
            "commercial should be cached before upgrade"
        );

        // Upgrade must succeed for this test to be meaningful — the
        // eviction only runs when the footprint changes. If grow_to_level
        // returns false, the test setup is wrong (the commercial couldn't
        // grow) and the test should fail loudly.
        assert!(
            grow_to_level(&mut world, commercial, 2),
            "upgrade should succeed in this fixture"
        );

        // Footprint must have grown (not just level bumped).
        let building = world
            .buildings
            .get(&commercial)
            .copied()
            .expect("commercial still exists");
        assert_eq!(building.level, 2, "upgrade succeeded → level is 2");
        assert_ne!(
            building.footprint,
            Footprint::single(),
            "upgrade succeeded → footprint grew"
        );

        // The cache must be evicted because the destination entry cells
        // may have moved.
        assert!(
            !world.route_cache_contains(commercial, network.id),
            "commercial key must be evicted after footprint growth"
        );
    }
}
