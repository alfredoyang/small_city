//! P3 movement — steps each citizen one road cell per tick along the route
//! cache, driven by the daily schedule (`systems/schedule.rs`).
//!
//! This is the consumer the schedule layer was built for: the schedule answers
//! *what* a citizen wants (Home / Work / Leisure), and this system resolves that
//! to a concrete target building and walks the citizen there over the road graph
//! using the P2 route cache (`World::routes_to`). It owns no pathfinding of its
//! own — only the per-citizen state machine.
//!
//! ```text
//!   tick (after happiness::run, before turn += 1)
//!     │
//!     ▼  for each citizen, sorted by entity.0 (determinism):
//!   intent = schedule_intent(hour, citizen)         // schedule.rs
//!   target = resolve_target(intent)                 // Home→home, Work→local|home
//!            (remote Work idles at home in v1; P5 routes to a border-exit cell)
//!     │
//!     ├─ idle in a building (current_cell = None)
//!     │     at target?      → stay idle (normalise AtHome/AtWork)
//!     │     target changed? → depart onto an entry road cell of the *origin*
//!     │                        building if a route exists, else stay put (§4b)
//!     │
//!     └─ en route (current_cell = Some(cell))
//!           adjacent to target? → arrive (status AtHome/AtWork, cell = None)
//!           else                → step current_cell = came_from[cell] (one cell),
//!                                  or stay put if unreachable (§4b, no teleport)
//! ```
//!
//! Determinism: citizens are visited in `entity.0` order; networks are discovered
//! deterministically; `routes_to` is a deterministic Dijkstra tree; building entry
//! cells are sorted by position. Same inputs → same movement.
//!
//! Persistence: `World::travel` is `#[serde(skip)]`; trips are not saved. On load
//! the first tick re-derives placement from the schedule.

use std::collections::HashSet;

use crate::core::components::{TravelState, TravelStatus};
use crate::core::entity::Entity;
use crate::core::regions::RegionId;
use crate::core::systems::road_connectivity::{self, RoadNetwork};
use crate::core::systems::schedule::{ScheduleIntent, schedule_intent};
use crate::core::world::World;

/// Steps every citizen's commute one cell. Runs once per tick.
pub(crate) fn run(world: &mut World) {
    let hour = world.resources.time.hour_of_day();
    let region = world.region_id;
    // Discovered once per tick and reused for every citizen's cell→network lookup.
    let networks = road_connectivity::discover_road_networks(world);

    // Visit citizens in a fixed order so movement is deterministic.
    let mut ids: Vec<Entity> = world.citizens.keys().copied().collect();
    ids.sort_unstable_by_key(|entity| entity.0);

    for id in &ids {
        // Pull the citizen-side inputs out up front, releasing the `world.citizens`
        // borrow before the road-graph reads (`routes_to` borrows the whole world).
        let Some((intent, home)) = world
            .citizens
            .get(id)
            .map(|citizen| (schedule_intent(hour, citizen), citizen.home))
        else {
            continue;
        };

        let target = resolve_target(region, home, intent);
        let state = world.travel.get(id).copied().unwrap_or_default();
        let next = advance(world, &networks, home, target, state);
        world.travel.insert(*id, next);
    }

    // Drop trip state for citizens that no longer exist (died/relocated).
    let live: HashSet<Entity> = world.citizens.keys().copied().collect();
    world.travel.retain(|id, _| live.contains(id));
}

/// The building a citizen should be at right now. Everything that isn't a *local*
/// job resolves to home: a remote job idles at home in v1 (P5 adds border-exit
/// routing), a jobless/off-hours citizen wants home, and Leisure is deferred.
fn resolve_target(region: RegionId, home: Entity, intent: ScheduleIntent) -> Entity {
    match intent {
        ScheduleIntent::Home | ScheduleIntent::Leisure => home,
        ScheduleIntent::Work(workplace) => workplace.as_local(region).unwrap_or(home),
    }
}

/// Advances one citizen's `TravelState` by one tick toward `target`.
fn advance(
    world: &World,
    networks: &[RoadNetwork],
    home: Entity,
    target: Entity,
    state: TravelState,
) -> TravelState {
    let arrived_status = if target == home {
        TravelStatus::AtHome
    } else {
        TravelStatus::AtWork
    };

    match state.current_cell {
        // Idle inside a building. The origin is read from movement state (the
        // building actually occupied), defaulting to home on the first tick
        // before `building` has been recorded.
        None => {
            // Origin = the building actually occupied. If it was bulldozed/replaced
            // (the recorded entity is gone), the citizen is *displaced*: there is no
            // current location left to stay at, so it returns home. This is a
            // deliberate exception to §4b — §4b's "no teleport" governs *unreachable
            // routes* (the origin still exists, just can't reach the target), not a
            // *destroyed origin*. We can't relocate onto the building's adjacent road
            // here because its position is already gone; doing so at the removal
            // chokepoint would couple movement into entity_cleanup for a corner case.
            let origin = state
                .building
                .filter(|building| world.buildings.contains_key(building))
                .unwrap_or(home);
            if origin == target {
                // Already where we want to be — normalise the status, stay idle.
                return idle(arrived_status, target);
            }
            // Depart if a route exists; otherwise stay idle at the origin (§4b).
            // The stay-status matches the origin so it never lingers as AtWork at
            // home after a fallback.
            let stay_status = if origin == home {
                TravelStatus::AtHome
            } else {
                TravelStatus::AtWork
            };
            let stay = idle(stay_status, origin);
            depart(world, networks, origin, target).unwrap_or(stay)
        }
        // En route on a road cell.
        Some(cell) => {
            if is_adjacent(world, target, cell) {
                // Reached a road cell touching the target → arrived.
                return idle(arrived_status, target);
            }
            step(world, networks, cell, target)
        }
    }
}

/// Depart from `origin` toward `target`: step onto the first (position-sorted)
/// entry road cell of `origin` from which `target` is reachable. If no entry can
/// reach the target (disconnected / no road), stay put (§4b — no teleport).
fn depart(
    world: &World,
    networks: &[RoadNetwork],
    origin: Entity,
    target: Entity,
) -> Option<TravelState> {
    for entry in adjacent_roads_sorted(world, origin) {
        let Some(network) = network_of_cell(networks, entry) else {
            continue;
        };
        // Reachable iff the entry already touches the target (arrive next tick) or
        // the target's route tree on this network includes the entry cell.
        let reachable = is_adjacent(world, target, entry) || {
            let tree = world.routes_to(target, network);
            tree.contains_key(&entry)
        };
        if reachable {
            return Some(travelling(entry, target));
        }
    }
    None
}

/// Step one cell from `cell` toward `target` along its `came_from` tree. If the
/// cell is off the graph or unreachable from here, stay put (§4b).
fn step(world: &World, networks: &[RoadNetwork], cell: Entity, target: Entity) -> TravelState {
    let Some(network) = network_of_cell(networks, cell) else {
        return travelling(cell, target);
    };
    let next = {
        let tree = world.routes_to(target, network);
        tree.get(&cell).copied()
    };
    match next {
        Some(next) => travelling(next, target),
        None => travelling(cell, target),
    }
}

fn idle(status: TravelStatus, building: Entity) -> TravelState {
    TravelState {
        status,
        current_cell: None,
        destination: None,
        building: Some(building),
    }
}

fn travelling(cell: Entity, target: Entity) -> TravelState {
    TravelState {
        status: TravelStatus::Traveling,
        current_cell: Some(cell),
        destination: Some(target),
        building: None,
    }
}

/// The road cells orthogonally adjacent to `building`, in deterministic order.
fn adjacent_roads_sorted(world: &World, building: Entity) -> Vec<Entity> {
    let mut roads: Vec<Entity> =
        road_connectivity::adjacent_road_entities(world, building).collect();
    road_connectivity::sort_entities_by_position(world, &mut roads);
    roads
}

/// Whether `cell` is a road cell orthogonally adjacent to `building`.
fn is_adjacent(world: &World, building: Entity, cell: Entity) -> bool {
    road_connectivity::adjacent_road_entities(world, building).any(|road| road == cell)
}

/// The discovered network containing `cell`, if any.
fn network_of_cell(networks: &[RoadNetwork], cell: Entity) -> Option<&RoadNetwork> {
    networks
        .iter()
        .find(|network| network.roads.contains(&cell))
}

#[cfg(test)]
mod tests {
    use super::run;
    use crate::core::city_refs::CityCellRef;
    use crate::core::components::{Citizen, Morale, TravelStatus, WorkplaceAssignment};
    use crate::core::entity::Entity;
    use crate::core::regions::RegionId;
    use crate::core::systems::entity_cleanup::remove_entity;
    use crate::core::systems::placement::place_building;
    use crate::core::world::World;
    use crate::interface::input::BuildingKind;

    /// Advances time to the next occurrence of `hour` (absolute hour-of-day), so
    /// tests read `set_hour(16)` rather than an additive delta. Travel only reads
    /// `hour_of_day()`, so the day is irrelevant here.
    fn set_hour(world: &mut World, hour: u8) {
        let current = u64::from(world.resources.time.hour_of_day());
        let delta = (24 + u64::from(hour) - current) % 24;
        world.resources.time.advance_hours(delta);
    }

    /// Inserts a citizen (off-grid) with `home` and an optional `workplace`.
    fn add_citizen(
        world: &mut World,
        local: u32,
        home: Entity,
        workplace: Option<Entity>,
    ) -> Entity {
        let id = Entity::new(world.region_id, local);
        let workplace_assignment = workplace.map(|w| WorkplaceAssignment {
            workplace: w,
            location: CityCellRef::local(w.region(), 0, 0),
            salary: 100,
        });
        world.citizens.insert(
            id,
            Citizen {
                id,
                age: 1,
                home,
                workplace_assignment,
                morale: Morale::default(),
                money: 0,
            },
        );
        id
    }

    /// Linear road r0..r3 along row 0; home at (0,1) touches r0, work at (3,1)
    /// touches r3. One connected network.
    fn commute_world() -> (World, [Entity; 4], Entity, Entity) {
        let mut world = World::new(4, 2);
        let mut roads = [Entity::default(); 4];
        for (x, slot) in roads.iter_mut().enumerate() {
            place_building(&mut world, x, 0, BuildingKind::Road);
            *slot = world.grid.get(x, 0).expect("road placed");
        }
        place_building(&mut world, 0, 1, BuildingKind::Residential);
        place_building(&mut world, 3, 1, BuildingKind::Commercial);
        let home = world.grid.get(0, 1).expect("home placed");
        let work = world.grid.get(3, 1).expect("work placed");
        (world, roads, home, work)
    }

    /// At 09:00 the citizen departs home and walks r0→r1→r2→r3, then idles AtWork.
    #[test]
    fn citizen_commutes_home_to_work_then_idles() {
        let (mut world, roads, home, work) = commute_world();
        let id = add_citizen(&mut world, 100, home, Some(work));
        set_hour(&mut world, 9);

        // Departs onto r0, then steps one cell per tick to r3.
        let mut seen = Vec::new();
        for _ in 0..4 {
            run(&mut world);
            seen.push(world.travel[&id].current_cell.expect("on a road cell"));
        }
        assert_eq!(seen, roads.to_vec(), "exact route cells r0..r3");

        // Next tick: r3 touches work → arrived, idling AtWork.
        run(&mut world);
        let state = world.travel[&id];
        assert_eq!(state.status, TravelStatus::AtWork);
        assert_eq!(state.current_cell, None);

        // Stays idle at work for subsequent work-hour ticks.
        run(&mut world);
        assert_eq!(world.travel[&id].status, TravelStatus::AtWork);
        assert_eq!(world.travel[&id].current_cell, None);
    }

    /// After the workday the citizen walks back home and idles AtHome.
    #[test]
    fn citizen_returns_home_after_work() {
        let (mut world, _roads, home, work) = commute_world();
        let id = add_citizen(&mut world, 100, home, Some(work));

        // Commute to work first (run through the morning until idle AtWork).
        set_hour(&mut world, 9);
        for _ in 0..6 {
            run(&mut world);
        }
        assert_eq!(world.travel[&id].status, TravelStatus::AtWork);

        // Jump to the evening home phase and run until idle.
        set_hour(&mut world, 16); // evening Home phase
        for _ in 0..6 {
            run(&mut world);
        }
        let state = world.travel[&id];
        assert_eq!(state.status, TravelStatus::AtHome);
        assert_eq!(state.current_cell, None);
    }

    /// Work on a disconnected road network → no route → the citizen never departs.
    #[test]
    fn unreachable_workplace_keeps_citizen_home() {
        // r0,r1 connected; r3 isolated (gap at x=2). home touches r0, work touches r3.
        let mut world = World::new(4, 2);
        place_building(&mut world, 0, 0, BuildingKind::Road);
        place_building(&mut world, 1, 0, BuildingKind::Road);
        place_building(&mut world, 3, 0, BuildingKind::Road);
        place_building(&mut world, 0, 1, BuildingKind::Residential);
        place_building(&mut world, 3, 1, BuildingKind::Commercial);
        let home = world.grid.get(0, 1).expect("home");
        let work = world.grid.get(3, 1).expect("work");
        let id = add_citizen(&mut world, 100, home, Some(work));

        set_hour(&mut world, 10);
        for _ in 0..5 {
            run(&mut world);
        }
        let state = world.travel[&id];
        assert_eq!(state.status, TravelStatus::AtHome, "no route → stay home");
        assert_eq!(state.current_cell, None, "never departs");
    }

    /// Stranded at work when the road home is torn up: the citizen stays AtWork
    /// (no teleport, §4b) and walks home once the road reconnects. Also guards the
    /// origin-from-state fix — the workplace assignment is unchanged throughout,
    /// so the citizen must depart from where it actually is, not re-infer it.
    #[test]
    fn stranded_at_work_stays_until_road_reconnects() {
        let (mut world, roads, home, work) = commute_world();
        let id = add_citizen(&mut world, 100, home, Some(work));

        // Commute to work and idle there.
        set_hour(&mut world, 9);
        for _ in 0..6 {
            run(&mut world);
        }
        assert_eq!(world.travel[&id].status, TravelStatus::AtWork);

        // Tear up the middle road r1 (at (1,0)) → home unreachable from work.
        remove_entity(&mut world, roads[1], 1, 0);

        // Evening home phase: no route home → stays AtWork (stranded, no teleport).
        set_hour(&mut world, 16); // evening Home phase
        for _ in 0..6 {
            run(&mut world);
        }
        let state = world.travel[&id];
        assert_eq!(
            state.status,
            TravelStatus::AtWork,
            "stranded, no route home"
        );
        assert_eq!(state.current_cell, None);

        // Rebuild r1 → route home reappears → citizen walks home.
        place_building(&mut world, 1, 0, BuildingKind::Road);
        for _ in 0..6 {
            run(&mut world);
        }
        assert_eq!(
            world.travel[&id].status,
            TravelStatus::AtHome,
            "routes home after the road reconnects"
        );
    }

    /// Bulldozing the building a citizen is idling in displaces it home (the
    /// documented §4b exception — a destroyed origin has no location to stay at,
    /// distinct from an unreachable-but-existing origin, which *does* stay put as
    /// the stranded/reconnect test above shows). The citizen must not be stranded
    /// in the deleted building forever.
    #[test]
    fn bulldozing_occupied_workplace_relocates_citizen_home() {
        let (mut world, _roads, home, work) = commute_world();
        let id = add_citizen(&mut world, 100, home, Some(work));

        set_hour(&mut world, 9);
        for _ in 0..6 {
            run(&mut world);
        }
        assert_eq!(world.travel[&id].status, TravelStatus::AtWork);
        assert_eq!(world.travel[&id].building, Some(work));

        // Bulldoze the workplace the citizen is standing in.
        remove_entity(&mut world, work, 3, 1);
        for _ in 0..6 {
            run(&mut world);
        }
        let state = world.travel[&id];
        assert_eq!(
            state.status,
            TravelStatus::AtHome,
            "fell back home, not stranded in the deleted building"
        );
        assert_eq!(state.current_cell, None);
    }

    /// Changing the workplace assignment while idle AtWork must depart from the
    /// *old* workplace (recorded in travel state), not teleport to the new one.
    /// This is the guard the reconnect test couldn't provide.
    #[test]
    fn reassigned_worker_departs_from_old_workplace() {
        // road r0..r4 on row 0; home~r0 (0,1), workA~r2 (2,1), workB~r4 (4,1).
        let mut world = World::new(5, 2);
        let mut roads = [Entity::default(); 5];
        for (x, slot) in roads.iter_mut().enumerate() {
            place_building(&mut world, x, 0, BuildingKind::Road);
            *slot = world.grid.get(x, 0).expect("road placed");
        }
        place_building(&mut world, 0, 1, BuildingKind::Residential);
        place_building(&mut world, 2, 1, BuildingKind::Commercial);
        place_building(&mut world, 4, 1, BuildingKind::Commercial);
        let home = world.grid.get(0, 1).expect("home");
        let work_a = world.grid.get(2, 1).expect("workA");
        let work_b = world.grid.get(4, 1).expect("workB");
        let id = add_citizen(&mut world, 100, home, Some(work_a));

        // Commute to workA and idle there.
        set_hour(&mut world, 9);
        for _ in 0..8 {
            run(&mut world);
        }
        assert_eq!(world.travel[&id].status, TravelStatus::AtWork);
        assert_eq!(world.travel[&id].building, Some(work_a));

        // Reassign to workB while idle at A.
        world
            .citizens
            .get_mut(&id)
            .expect("citizen")
            .workplace_assignment = Some(WorkplaceAssignment {
            workplace: work_b,
            location: CityCellRef::local(work_b.region(), 4, 1),
            salary: 100,
        });

        // One tick: depart from workA's entry road r2 — not teleport to B.
        run(&mut world);
        let state = world.travel[&id];
        assert_eq!(state.status, TravelStatus::Traveling);
        assert_eq!(
            state.current_cell,
            Some(roads[2]),
            "departs from workA's entry road, proving origin came from travel state"
        );
    }

    /// A remote workplace (different region) idles at home in v1.
    #[test]
    fn remote_worker_idles_at_home() {
        let (mut world, _roads, home, _work) = commute_world();
        let remote = Entity::new(RegionId(7), 1); // different region → as_local None
        let id = add_citizen(&mut world, 100, home, Some(remote));

        set_hour(&mut world, 10);
        run(&mut world);
        let state = world.travel[&id];
        assert_eq!(state.status, TravelStatus::AtHome);
        assert_eq!(state.current_cell, None);
    }

    /// A removed citizen is pruned from the travel map.
    #[test]
    fn dead_citizen_is_pruned_from_travel() {
        let (mut world, _roads, home, work) = commute_world();
        let id = add_citizen(&mut world, 100, home, Some(work));
        set_hour(&mut world, 9);
        run(&mut world);
        assert!(world.travel.contains_key(&id));

        world.citizens.remove(&id);
        run(&mut world);
        assert!(
            !world.travel.contains_key(&id),
            "pruned after citizen removed"
        );
    }

    /// Movement is deterministic: two identical worlds step to identical state.
    #[test]
    fn movement_is_deterministic() {
        let build = || {
            let (mut world, _roads, home, work) = commute_world();
            add_citizen(&mut world, 100, home, Some(work));
            world.resources.time.advance_hours(9);
            world
        };
        let mut a = build();
        let mut b = build();
        for _ in 0..3 {
            run(&mut a);
            run(&mut b);
        }
        let mut ta: Vec<_> = a.travel.iter().map(|(k, v)| (*k, *v)).collect();
        let mut tb: Vec<_> = b.travel.iter().map(|(k, v)| (*k, *v)).collect();
        ta.sort_by_key(|(k, _)| k.0);
        tb.sort_by_key(|(k, _)| k.0);
        assert_eq!(ta, tb);
    }
}
