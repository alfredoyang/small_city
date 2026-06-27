//! Citizen movement — steps each citizen along the P2 route cache, driven by the
//! daily schedule (`systems/schedule.rs`).
//!
//! The schedule answers *what* a citizen wants (Home / Work / Leisure); this system
//! resolves that to a concrete target and walks the citizen there over the road
//! graph using `World::routes_to`. It owns no pathfinding of its own — only the
//! per-citizen state machine.
//!
//! `step_travel` is the **10-minute movement sub-tick** (P7c): it is *not* part of
//! the hourly economy tick. The runner broadcasts a `RegionEvent::StepTravel` to
//! every region 6× per game hour, so a traveller advances ~6 cells/hour — gated by
//! `dwell` (P7b) so a crossing/turn cell holds it 2×/4× longer.
//!
//! ```text
//!   step_travel (one 10-min sub-tick), each citizen sorted by entity.0:
//!     dwell gate: still traversing this cell? (dwell+1 < step_cost) → stay, dwell++
//!     else advance one transition:
//!       idle in a building → depart onto an entry road cell (or stay, §4b)
//!       en route, adjacent to target → arrive (AtHome/AtWork, cell = None)
//!       en route, on a border-exit cell → cross (buffer handoff, mark Away — P5)
//!       en route otherwise → step current_cell = came_from[cell] (or stay, §4b)
//! ```
//!
//! Determinism: citizens are visited in `entity.0` order; networks are discovered
//! deterministically; `routes_to` is a deterministic Dijkstra tree; entry cells are
//! sorted by position. Same inputs → same movement.
//!
//! Persistence: `World::travel` is `#[serde(skip)]`; trips are not saved. On load
//! the first sub-tick re-derives placement from the schedule.

use std::collections::HashSet;

use crate::core::components::{
    PendingHandoff, ReturnHop, TravelState, TravelStatus, TravelerId, VisitingToken,
};
use crate::core::entity::Entity;
use crate::core::regions::RegionId;
use crate::core::systems::road_connectivity::{self, RoadNetwork};
use crate::core::systems::road_network_analysis::{road_degree_in_network, step_cost};
use crate::core::systems::schedule::{
    ScheduleIntent, SchedulePhase, schedule_intent, schedule_phase,
};
use crate::core::world::World;

/// Where a citizen is headed this sub-tick.
enum Target<'a> {
    /// A local building (home or local workplace) — normal P3 movement.
    Building(Entity),
    /// A remote-but-reachable workplace (P5): the mover walks to whichever of the
    /// `candidates` border-exit cells it can reach, then crosses to `to_region`
    /// carrying `workplace`.
    BorderExit {
        candidates: &'a [Entity],
        workplace: Entity,
        to_region: RegionId,
    },
}

/// Outcome of advancing one local citizen.
enum Advance {
    /// Update the citizen's travel state in place.
    Stay(TravelState),
    /// The token reached its border-exit cell — hand it off and mark the citizen
    /// `Away` (P5). `token.destination` is the remote workplace.
    Cross {
        to_region: RegionId,
        exit_cell: Entity,
        token: TravelState,
    },
}

/// Advances every citizen's commute by one 10-minute movement sub-tick (gated by
/// `dwell` so a crossing/turn cell holds the traveller for 2×/4× as long). Driven
/// 6× per game hour by the runner, separately from the hourly economy tick.
pub(crate) fn step_travel(world: &mut World) {
    let hour = world.resources.time.hour_of_day();
    let region = world.region_id;
    // Discovered once per sub-tick and reused for every citizen's cell→network lookup.
    let networks = road_connectivity::discover_road_networks(world);

    // Visit citizens in a fixed order so movement is deterministic.
    let mut ids: Vec<Entity> = world.citizens.keys().copied().collect();
    ids.sort_unstable_by_key(|entity| entity.0);

    for id in &ids {
        let state = world.travel.get(id).copied().unwrap_or_default();
        // P5: an Away citizen's token is out in a neighbor region — the home side
        // neither steps nor draws it until a Return clears the mark.
        if state.status == TravelStatus::Away {
            continue;
        }

        // Pull the citizen-side inputs out up front, releasing the `world.citizens`
        // borrow before the road-graph reads (`routes_to` borrows the whole world).
        let Some((intent, home)) = world
            .citizens
            .get(id)
            .map(|citizen| (schedule_intent(hour, citizen), citizen.home))
        else {
            continue;
        };

        let target = resolve_target(world, region, home, intent);

        // P7b dwell gate: a traveller traversing the current cell stays on it for
        // `step_cost(cell)` sub-ticks (1 straight, 2 turn/T-junction, 4 four-way)
        // before `advance` moves it. Idle citizens and cells about to arrive/cross
        // (`dwell_cost_for` → None) are not gated.
        if let Some(cost) = dwell_cost_for(world, &networks, &target, state) {
            if u32::from(state.dwell) + 1 < cost {
                let mut waiting = state;
                waiting.dwell += 1;
                world.travel.insert(*id, waiting);
                continue;
            }
        }

        let from = state.current_cell;
        match advance(world, &networks, home, target, state) {
            Advance::Stay(mut next) => {
                // Moved to a new road cell → record the cell just left (so the next
                // turn is known) and reset its dwell. `travelling`/`idle` already
                // zero `dwell`; an unchanged cell (stay-put) keeps the accumulated one.
                if next.current_cell.is_some() && next.current_cell != from {
                    next.prev_cell = from;
                } else if next.current_cell == from {
                    next.dwell = state.dwell;
                    next.prev_cell = state.prev_cell;
                }
                world.travel.insert(*id, next);
            }
            Advance::Cross {
                to_region,
                exit_cell,
                token,
            } => {
                // Bump the trip generation so a later stale Return is ignored.
                let generation = {
                    let counter = world.away_generation.entry(*id).or_insert(0);
                    *counter += 1;
                    *counter
                };
                world.outgoing_handoffs.push(PendingHandoff::Outbound {
                    traveler: TravelerId {
                        citizen: *id,
                        generation,
                    },
                    token,
                    to_region,
                    exit_cell,
                });
                // Remove the local token (no dot here) and mark the citizen Away.
                world.travel.insert(*id, away_state());
            }
        }
    }

    // Drop trip state for citizens that no longer exist (died/relocated). Away
    // citizens stay (they still exist), so their mark survives until Return.
    let live: HashSet<Entity> = world.citizens.keys().copied().collect();
    world.travel.retain(|id, _| live.contains(id));

    step_visiting_tokens(world, hour, &networks);
}

/// P5: steps the tokens neighbor regions handed in, drawing them as dots while
/// moving. Once parked at work they stay off-road until the workday ends, when
/// each is returned home and removed.
fn step_visiting_tokens(world: &mut World, hour: u8, networks: &[RoadNetwork]) {
    let phase = schedule_phase(hour);
    let mut ids: Vec<TravelerId> = world.visiting_travel.keys().copied().collect();
    ids.sort_unstable_by_key(|traveler| (traveler.citizen.0, traveler.generation));

    for traveler in &ids {
        let visiting = world
            .visiting_travel
            .get(traveler)
            .expect("id from keys")
            .clone();
        match step_visiting(world, networks, phase, &visiting.token) {
            Some(token) => {
                world
                    .visiting_travel
                    .get_mut(traveler)
                    .expect("id from keys")
                    .token = token;
            }
            None => {
                // Workday over (or malformed) — return the traveler home and drop
                // the token (its dot disappears).
                world.outgoing_handoffs.push(PendingHandoff::Return {
                    traveler: *traveler,
                    return_path: visiting.return_path.clone(),
                });
                world.visiting_travel.remove(traveler);
            }
        }
    }
}

/// One step for a visiting token toward its workplace. `Some(token)` keeps it
/// (walking, or parked at the workplace off-road); `None` means it should be
/// returned home — either the workday ended or the workplace is unreachable
/// (disconnected / bulldozed), which returns the traveler immediately (§5g).
fn step_visiting(
    world: &World,
    networks: &[RoadNetwork],
    phase: SchedulePhase,
    token: &TravelState,
) -> Option<TravelState> {
    if phase != SchedulePhase::Work {
        return None; // workday over → return home
    }
    let dest = token.destination?; // the local workplace in this host region
    let Some(cell) = token.current_cell else {
        return Some(*token); // already parked at work; wait until workday end
    };
    if is_adjacent(world, dest, cell) {
        return Some(TravelState {
            status: TravelStatus::AtWork,
            current_cell: None,
            destination: Some(dest),
            building: Some(dest),
            dwell: 0,
            prev_cell: None,
        });
    }
    // Step toward the workplace; if the cell is off-graph or the workplace is
    // unreachable from here, the trip can't complete → return home now (§5g).
    let network = network_of_cell(networks, cell)?;
    let next = {
        let tree = world.routes_to(dest, network);
        tree.get(&cell).copied()
    }?;
    // P7b dwell: hold the visiting token on a crossing/turn cell for step_cost
    // sub-ticks before advancing, exactly like a local traveller.
    let degree = road_degree_in_network(world, cell, network);
    let cost = step_cost(world, token.prev_cell, cell, Some(next), degree);
    if u32::from(token.dwell) + 1 < cost {
        let mut waiting = *token;
        waiting.dwell += 1;
        Some(waiting)
    } else {
        let mut moved = travelling(next, dest);
        moved.prev_cell = Some(cell);
        Some(moved)
    }
}

/// The target a citizen is headed for. A *local* job routes to the workplace; a
/// remote job routes to a known border-exit cell (P5, reachable direct neighbor)
/// or else falls back home; everything else routes home.
fn resolve_target<'a>(
    world: &'a World,
    region: RegionId,
    home: Entity,
    intent: ScheduleIntent,
) -> Target<'a> {
    match intent {
        ScheduleIntent::Home | ScheduleIntent::Leisure => Target::Building(home),
        ScheduleIntent::Work(workplace) => match workplace.as_local(region) {
            Some(local) => Target::Building(local),
            None => match world.remote_exit_cells.get(&workplace.region()) {
                // Remote but reachable via a direct neighbor → commute to an exit.
                Some(candidates) if !candidates.is_empty() => Target::BorderExit {
                    candidates,
                    workplace,
                    to_region: workplace.region(),
                },
                // Remote and not directly reachable → idle at home (P1–P4 behaviour).
                _ => Target::Building(home),
            },
        },
    }
}

/// The idle "out of region" state for an away citizen.
fn away_state() -> TravelState {
    TravelState {
        status: TravelStatus::Away,
        current_cell: None,
        destination: None,
        building: None,
        dwell: 0,
        prev_cell: None,
    }
}

/// P7b: the cost (in sub-ticks) to traverse the traveller's current cell toward
/// `target`, or `None` when the move must NOT be dwell-gated — the citizen is idle,
/// off the road graph, unreachable, or about to arrive/cross (those are handled
/// instantly by `advance`). The turn at the cell is computed from `prev_cell` (the
/// entry) and the route-cache forward cell (the exit).
fn dwell_cost_for(
    world: &World,
    networks: &[RoadNetwork],
    target: &Target,
    state: TravelState,
) -> Option<u32> {
    let cell = state.current_cell?; // idle → not gated
    let network = network_of_cell(networks, cell)?; // off-graph → not gated
    let dest = match target {
        Target::Building(building) => {
            if is_adjacent(world, *building, cell) {
                return None; // arriving next → advance handles it instantly
            }
            *building
        }
        Target::BorderExit { candidates, .. } => {
            // Only gate against a still-valid committed exit; a stale destination
            // (not in the current candidates) is left ungated so `advance` re-picks
            // immediately rather than dwelling toward the old exit.
            let exit = state.destination.filter(|e| candidates.contains(e))?;
            if cell == exit {
                return None; // crossing next → advance handles it instantly
            }
            exit
        }
    };
    let next = {
        let tree = world.routes_to(dest, network);
        tree.get(&cell).copied()
    }?; // unreachable from here → not gated (advance stays put, no dwell)
    let degree = road_degree_in_network(world, cell, network);
    Some(step_cost(world, state.prev_cell, cell, Some(next), degree))
}

/// Advances one citizen one sub-tick toward `target`.
fn advance(
    world: &World,
    networks: &[RoadNetwork],
    home: Entity,
    target: Target,
    state: TravelState,
) -> Advance {
    match target {
        Target::Building(building) => {
            Advance::Stay(advance_to_building(world, networks, home, building, state))
        }
        Target::BorderExit {
            candidates,
            workplace,
            to_region,
        } => advance_to_exit(
            world, networks, home, candidates, workplace, to_region, state,
        ),
    }
}

/// Walks a remote commuter toward a border-exit cell; once it lands on the exit
/// cell it crosses, carrying the workplace as the token's destination.
///
/// While en route the chosen exit is `state.destination` (a candidate cell). When
/// idle, the first candidate reachable from the origin is chosen — so candidates on
/// a different home road network are skipped (§5d).
fn advance_to_exit(
    world: &World,
    networks: &[RoadNetwork],
    home: Entity,
    candidates: &[Entity],
    workplace: Entity,
    to_region: RegionId,
    state: TravelState,
) -> Advance {
    match state.current_cell {
        None => {
            let origin = state
                .building
                .filter(|building| world.buildings.contains_key(building))
                .unwrap_or(home);
            // Pick the first candidate this origin can actually reach.
            for &exit_cell in candidates {
                if let Some(entry) = depart_to_cell(world, networks, origin, exit_cell) {
                    return Advance::Stay(travelling(entry, exit_cell));
                }
            }
            // No reachable exit → idle at the origin (remote-but-unreachable, §4b).
            let stay_status = if origin == home {
                TravelStatus::AtHome
            } else {
                TravelStatus::AtWork
            };
            Advance::Stay(idle(stay_status, origin))
        }
        Some(cell) => {
            // The committed exit is the destination we were walking to; if it is
            // somehow unset/invalid, re-pick a candidate reachable from here.
            let exit_cell = state
                .destination
                .filter(|exit| candidates.contains(exit))
                .or_else(|| {
                    candidates.iter().copied().find(|&exit| {
                        cell == exit || step_toward_cell(world, networks, cell, exit).is_some()
                    })
                });
            let Some(exit_cell) = exit_cell else {
                return Advance::Stay(state); // unreachable from here → stay put
            };
            if cell == exit_cell {
                // On the exit cell → cross (it already had its one-sub-tick dot here).
                return Advance::Cross {
                    to_region,
                    exit_cell,
                    token: travelling(exit_cell, workplace),
                };
            }
            match step_toward_cell(world, networks, cell, exit_cell) {
                Some(next) => Advance::Stay(travelling(next, exit_cell)),
                None => Advance::Stay(travelling(cell, exit_cell)), // unreachable → stay
            }
        }
    }
}

/// Advances one citizen one sub-tick toward a local `target` building.
fn advance_to_building(
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
        // building actually occupied), defaulting to home on the first sub-tick
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
        // Reachable iff the entry already touches the target (arrive next sub-tick) or
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

/// P5: depart from `origin` toward a road `dest_cell` (a border-exit cell, not a
/// building). Returns the first reachable position-sorted entry road cell.
fn depart_to_cell(
    world: &World,
    networks: &[RoadNetwork],
    origin: Entity,
    dest_cell: Entity,
) -> Option<Entity> {
    for entry in adjacent_roads_sorted(world, origin) {
        let Some(network) = network_of_cell(networks, entry) else {
            continue;
        };
        let reachable = entry == dest_cell || {
            let tree = world.routes_to(dest_cell, network);
            tree.contains_key(&entry)
        };
        if reachable {
            return Some(entry);
        }
    }
    None
}

/// P5: one step from `cell` toward a road `dest_cell`. `None` if off-graph or
/// unreachable (caller holds the current cell, §4b).
fn step_toward_cell(
    world: &World,
    networks: &[RoadNetwork],
    cell: Entity,
    dest_cell: Entity,
) -> Option<Entity> {
    let network = network_of_cell(networks, cell)?;
    let tree = world.routes_to(dest_cell, network);
    tree.get(&cell).copied()
}

fn idle(status: TravelStatus, building: Entity) -> TravelState {
    TravelState {
        status,
        current_cell: None,
        destination: None,
        building: Some(building),
        dwell: 0,
        prev_cell: None,
    }
}

/// A token stepped onto `cell`. `prev_cell`/`dwell` are reset here; the caller
/// (run / step_visiting) patches `prev_cell` to the cell just left so the next
/// turn is known.
fn travelling(cell: Entity, target: Entity) -> TravelState {
    TravelState {
        status: TravelStatus::Traveling,
        current_cell: Some(cell),
        destination: Some(target),
        building: None,
        dwell: 0,
        prev_cell: None,
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

/// P5: accept a token handed in by a neighbor region and place it on `entry_cell`,
/// where it begins walking to its workplace (`token.destination`, local here). The
/// regions layer (P5b) resolves `entry_cell` from the handoff's border link.
#[allow(dead_code)] // P5a; the regions event handler (P5b) calls this.
pub(crate) fn receive_traveler(
    world: &mut World,
    traveler: TravelerId,
    mut token: TravelState,
    entry_cell: Entity,
    return_path: Vec<ReturnHop>,
) {
    token.status = TravelStatus::Traveling;
    token.current_cell = Some(entry_cell);
    token.building = None;
    world
        .visiting_travel
        .insert(traveler, VisitingToken { token, return_path });
}

/// P5: bring an away citizen home — clear its `Away` mark (back to AtHome idle).
///
/// This is the single "trip is over" entry point, used both when a neighbor
/// region returns the token **and** when P5b cannot route an outbound handoff (a
/// stale `exit_cell` no longer maps to a valid border link) and must roll the
/// crossing back. It only acts when the citizen is **still `Away` with a matching
/// generation**, so it is idempotent: a duplicate or stale `Return` — including one
/// arriving after the citizen has already commuted again — is ignored and never
/// resets an active trip.
#[allow(dead_code)] // P5a; the regions event handler (P5b) calls this.
pub(crate) fn apply_traveler_return(world: &mut World, traveler: TravelerId) {
    let still_away = world
        .travel
        .get(&traveler.citizen)
        .is_some_and(|state| state.status == TravelStatus::Away);
    let generation_matches =
        world.away_generation.get(&traveler.citizen) == Some(&traveler.generation);
    if still_away && generation_matches {
        world
            .travel
            .insert(traveler.citizen, TravelState::default());
    }
}

#[cfg(test)]
mod tests {
    use super::step_travel;
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

    /// P7b: a 4-way intersection holds a traveller for 4 sub-ticks (cost 4); the
    /// degree-1 arm cells cost 1 (one sub-tick each).
    #[test]
    fn dwell_holds_a_traveller_on_a_4way() {
        // "+" intersection at X(1,1): arms N(1,0) W(0,1) E(2,1) S(1,2).
        let mut world = World::new(3, 3);
        for (x, y) in [(1, 0), (0, 1), (1, 1), (2, 1), (1, 2)] {
            place_building(&mut world, x, y, BuildingKind::Road);
        }
        place_building(&mut world, 0, 0, BuildingKind::Residential); // touches N & W
        place_building(&mut world, 2, 2, BuildingKind::Commercial); // touches E & S
        let home = world.grid.get(0, 0).expect("home");
        let work = world.grid.get(2, 2).expect("work");
        let x_cell = world.grid.get(1, 1).expect("X");
        let id = add_citizen(&mut world, 100, home, Some(work));

        // Walk the commute and record the cell occupied each sub-tick.
        set_hour(&mut world, 9);
        let mut on_x = 0;
        for _ in 0..12 {
            step_travel(&mut world);
            if world.travel[&id].current_cell == Some(x_cell) {
                on_x += 1;
            }
            if world.travel[&id].status == TravelStatus::AtWork {
                break;
            }
        }
        assert_eq!(on_x, 4, "the 4-way X holds the traveller for 4 sub-ticks");
        assert_eq!(world.travel[&id].status, TravelStatus::AtWork, "arrived");
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
            step_travel(&mut world);
            seen.push(world.travel[&id].current_cell.expect("on a road cell"));
        }
        assert_eq!(seen, roads.to_vec(), "exact route cells r0..r3");

        // Next tick: r3 touches work → arrived, idling AtWork.
        step_travel(&mut world);
        let state = world.travel[&id];
        assert_eq!(state.status, TravelStatus::AtWork);
        assert_eq!(state.current_cell, None);

        // Stays idle at work for subsequent work-hour ticks.
        step_travel(&mut world);
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
            step_travel(&mut world);
        }
        assert_eq!(world.travel[&id].status, TravelStatus::AtWork);

        // Jump to the evening home phase and run until idle.
        set_hour(&mut world, 16); // evening Home phase
        for _ in 0..6 {
            step_travel(&mut world);
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
            step_travel(&mut world);
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
            step_travel(&mut world);
        }
        assert_eq!(world.travel[&id].status, TravelStatus::AtWork);

        // Tear up the middle road r1 (at (1,0)) → home unreachable from work.
        remove_entity(&mut world, roads[1], 1, 0);

        // Evening home phase: no route home → stays AtWork (stranded, no teleport).
        set_hour(&mut world, 16); // evening Home phase
        for _ in 0..6 {
            step_travel(&mut world);
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
            step_travel(&mut world);
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
            step_travel(&mut world);
        }
        assert_eq!(world.travel[&id].status, TravelStatus::AtWork);
        assert_eq!(world.travel[&id].building, Some(work));

        // Bulldoze the workplace the citizen is standing in.
        remove_entity(&mut world, work, 3, 1);
        for _ in 0..6 {
            step_travel(&mut world);
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
            step_travel(&mut world);
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
        step_travel(&mut world);
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
        step_travel(&mut world);
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
        step_travel(&mut world);
        assert!(world.travel.contains_key(&id));

        world.citizens.remove(&id);
        step_travel(&mut world);
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
            step_travel(&mut a);
            step_travel(&mut b);
        }
        let mut ta: Vec<_> = a.travel.iter().map(|(k, v)| (*k, *v)).collect();
        let mut tb: Vec<_> = b.travel.iter().map(|(k, v)| (*k, *v)).collect();
        ta.sort_by_key(|(k, _)| k.0);
        tb.sort_by_key(|(k, _)| k.0);
        assert_eq!(ta, tb);
    }

    // ---- P5: cross-region token handoff (core half) ----

    use super::{apply_traveler_return, receive_traveler};
    use crate::core::components::{PendingHandoff, ReturnHop, TravelState, TravelerId};
    use crate::core::regions::{BorderEdge, BorderLinkId};

    fn a_return_hop() -> ReturnHop {
        ReturnHop {
            region: RegionId(0),
            entry_link: BorderLinkId {
                edge: BorderEdge::East,
                offset: 0,
            },
        }
    }

    /// A remote-but-reachable worker walks to its border-exit cell, then crosses:
    /// the local token becomes `Away` and an Outbound handoff is buffered carrying
    /// the workplace as the token's destination.
    #[test]
    fn remote_worker_walks_to_exit_then_crosses_away() {
        // Road r0..r3 on row 0; home at (0,1) touches r0; exit cell is r3.
        let mut world = World::new(4, 2);
        let mut roads = [Entity::default(); 4];
        for (x, slot) in roads.iter_mut().enumerate() {
            place_building(&mut world, x, 0, BuildingKind::Road);
            *slot = world.grid.get(x, 0).expect("road");
        }
        place_building(&mut world, 0, 1, BuildingKind::Residential);
        let home = world.grid.get(0, 1).expect("home");
        let exit = roads[3];
        let workplace = Entity::new(RegionId(7), 99); // remote (different region)
        world.remote_exit_cells.insert(RegionId(7), vec![exit]);
        let id = add_citizen(&mut world, 1, home, Some(workplace));

        // Walk home → r0 → r1 → r2 → r3, then (on r3) cross.
        set_hour(&mut world, 9);
        for _ in 0..6 {
            step_travel(&mut world);
        }

        assert_eq!(world.travel[&id].status, TravelStatus::Away, "crossed away");
        assert_eq!(world.away_generation.get(&id), Some(&1), "first trip gen");
        assert_eq!(world.outgoing_handoffs.len(), 1, "one outbound crossing");
        match &world.outgoing_handoffs[0] {
            PendingHandoff::Outbound {
                traveler,
                token,
                to_region,
                exit_cell,
            } => {
                assert_eq!(traveler.citizen, id);
                assert_eq!(traveler.generation, 1);
                assert_eq!(*to_region, RegionId(7));
                assert_eq!(*exit_cell, exit);
                assert_eq!(
                    token.destination,
                    Some(workplace),
                    "token aims at workplace"
                );
                assert_eq!(token.current_cell, Some(exit));
            }
            other => panic!("expected Outbound, got {other:?}"),
        }

        // An Away citizen is skipped — no further movement or handoff.
        step_travel(&mut world);
        assert_eq!(world.travel[&id].status, TravelStatus::Away);
        assert_eq!(
            world.outgoing_handoffs.len(),
            1,
            "no new handoff while away"
        );
    }

    /// With exit candidates on two disconnected networks, the commuter walks to the
    /// one reachable from its home network and skips the unreachable candidate (§5d).
    #[test]
    fn remote_commuter_picks_reachable_exit_candidate() {
        // Home at (0,0) touches r_a (1,0) — a one-cell network A. r_b (2,2) is an
        // isolated one-cell network B, unreachable from home.
        let mut world = World::new(3, 3);
        place_building(&mut world, 0, 0, BuildingKind::Residential);
        place_building(&mut world, 1, 0, BuildingKind::Road);
        place_building(&mut world, 2, 2, BuildingKind::Road);
        let home = world.grid.get(0, 0).expect("home");
        let r_a = world.grid.get(1, 0).expect("r_a");
        let r_b = world.grid.get(2, 2).expect("r_b");
        let workplace = Entity::new(RegionId(7), 99);
        // r_b listed first (unreachable) must be skipped in favour of r_a.
        world.remote_exit_cells.insert(RegionId(7), vec![r_b, r_a]);
        let id = add_citizen(&mut world, 1, home, Some(workplace));

        set_hour(&mut world, 9);
        step_travel(&mut world); // departs onto the reachable exit r_a
        assert_eq!(
            world.travel[&id].current_cell,
            Some(r_a),
            "chose the reachable candidate, not r_b"
        );
        step_travel(&mut world); // on r_a → cross
        assert_eq!(world.travel[&id].status, TravelStatus::Away);
        match world.outgoing_handoffs.last().expect("a crossing") {
            PendingHandoff::Outbound { exit_cell, .. } => {
                assert_eq!(*exit_cell, r_a, "crossed via the reachable exit")
            }
            other => panic!("expected Outbound, got {other:?}"),
        }
    }

    /// A matching Return clears the Away mark; a stale (wrong-generation) Return is
    /// ignored.
    #[test]
    fn return_clears_away_only_on_matching_generation() {
        let mut world = World::new(1, 1);
        let id = Entity::new(world.region_id, 1);
        world.travel.insert(id, super::away_state());
        world.away_generation.insert(id, 3);

        // Stale generation → ignored.
        apply_traveler_return(
            &mut world,
            TravelerId {
                citizen: id,
                generation: 2,
            },
        );
        assert_eq!(
            world.travel[&id].status,
            TravelStatus::Away,
            "stale ignored"
        );

        // Matching generation → back home.
        apply_traveler_return(
            &mut world,
            TravelerId {
                citizen: id,
                generation: 3,
            },
        );
        assert_eq!(world.travel[&id].status, TravelStatus::AtHome, "cleared");

        // A duplicate Return (same generation) must NOT reset a citizen that has
        // since started commuting again.
        world.travel.insert(
            id,
            TravelState {
                status: TravelStatus::Traveling,
                current_cell: None,
                destination: None,
                building: None,
                dwell: 0,
                prev_cell: None,
            },
        );
        apply_traveler_return(
            &mut world,
            TravelerId {
                citizen: id,
                generation: 3,
            },
        );
        assert_eq!(
            world.travel[&id].status,
            TravelStatus::Traveling,
            "duplicate Return must not reset an active trip"
        );
    }

    /// A visiting token whose workplace is unreachable (disconnected road) returns
    /// home immediately rather than lingering as a stuck dot until the workday end.
    #[test]
    fn unreachable_visiting_token_returns_immediately() {
        // Two disconnected roads: r0 (entry) and r2 (touches the workplace).
        let mut world = World::new(3, 2);
        place_building(&mut world, 0, 0, BuildingKind::Road);
        place_building(&mut world, 2, 0, BuildingKind::Road);
        place_building(&mut world, 2, 1, BuildingKind::Commercial);
        let r0 = world.grid.get(0, 0).expect("r0");
        let workplace = world.grid.get(2, 1).expect("workplace");

        let traveler = TravelerId {
            citizen: Entity::new(RegionId(2), 5),
            generation: 1,
        };
        let token = TravelState {
            status: TravelStatus::Traveling,
            current_cell: None,
            destination: Some(workplace),
            building: None,
            dwell: 0,
            prev_cell: None,
        };
        // Entry at r0, but the workplace is on the disconnected r2.
        receive_traveler(&mut world, traveler, token, r0, vec![a_return_hop()]);

        set_hour(&mut world, 9); // work hours — yet it can't reach the workplace
        step_travel(&mut world);
        assert!(
            !world.visiting_travel.contains_key(&traveler),
            "unreachable token returned immediately"
        );
        assert!(
            world
                .outgoing_handoffs
                .iter()
                .any(|h| matches!(h, PendingHandoff::Return { .. })),
            "a Return was emitted"
        );
    }

    /// A visiting token walks to its workplace, parks there (dot disappears), and
    /// on the workday end is returned home and removed.
    #[test]
    fn visiting_token_walks_waits_then_returns_at_workday_end() {
        // Road r0..r3; commercial workplace at (3,1) touches r3.
        let mut world = World::new(4, 2);
        let mut roads = [Entity::default(); 4];
        for (x, slot) in roads.iter_mut().enumerate() {
            place_building(&mut world, x, 0, BuildingKind::Road);
            *slot = world.grid.get(x, 0).expect("road");
        }
        place_building(&mut world, 3, 1, BuildingKind::Commercial);
        let workplace = world.grid.get(3, 1).expect("workplace");

        let traveler = TravelerId {
            citizen: Entity::new(RegionId(2), 5),
            generation: 1,
        };
        let token = TravelState {
            status: TravelStatus::Traveling,
            current_cell: None,
            destination: Some(workplace),
            building: None,
            dwell: 0,
            prev_cell: None,
        };
        receive_traveler(&mut world, traveler, token, roads[0], vec![a_return_hop()]);

        // During work hours it walks r0 → r1 → r2 → r3, then parks inside the
        // workplace — no road-cell dot remains.
        set_hour(&mut world, 9);
        for _ in 0..6 {
            step_travel(&mut world);
        }
        let visiting = world
            .visiting_travel
            .get(&traveler)
            .expect("still visiting");
        assert_eq!(
            visiting.token.current_cell, None,
            "parked token must not render a visitor dot"
        );
        assert_eq!(visiting.token.status, TravelStatus::AtWork);
        assert_eq!(visiting.token.building, Some(workplace));

        // Workday ends → the token is returned home and removed (its dot is gone).
        set_hour(&mut world, 16);
        step_travel(&mut world);
        assert!(
            !world.visiting_travel.contains_key(&traveler),
            "token removed at workday end"
        );
        let returns: Vec<_> = world
            .outgoing_handoffs
            .iter()
            .filter(|h| matches!(h, PendingHandoff::Return { .. }))
            .collect();
        assert_eq!(returns.len(), 1, "one Return emitted");
        match returns[0] {
            PendingHandoff::Return {
                traveler: returned,
                return_path,
            } => {
                assert_eq!(*returned, traveler);
                assert_eq!(return_path, &vec![a_return_hop()]);
            }
            _ => unreachable!(),
        }
    }
}
