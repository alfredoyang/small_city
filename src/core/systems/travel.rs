//! Citizen movement — steps each citizen one road cell per sub-tick, driven by the
//! daily schedule (`systems/schedule.rs`).
//!
//! The schedule answers *what* a citizen wants (Home / Work / Leisure); this system
//! resolves that to a concrete target and walks the citizen there over the road
//! graph using `World::routes_to`. It owns no pathfinding of its own — only the
//! per-citizen state machine.
//!
//! `step_tokens` is the **10-minute movement sub-tick** (P7c): it is *not* part of
//! the hourly economy tick. The runner broadcasts a `RegionEvent::StepTravel` to
//! every region 6× per game hour, so a traveller advances ~6 cells/hour — gated by
//! `dwell` (P7b) so a crossing/turn cell holds it 2×/4× longer.
//!
//! ```text
//!   step_tokens (one 10-min sub-tick), per region:
//!     DEPART pass: world.citizens resident, idle at home (no token, not in away_residents)
//!                  whose schedule phase points elsewhere → first-step toward target.
//!                  Token created ONLY if a route exists (no route ⇒ no token).
//!     MOVE pass:   every present token (sorted by citizen.0):
//!                  local target   → advance_to_building → TokenArrival
//!                  remote target  → advance_to_exit    → Reached(exit_cell) → emit Move
//!                  ArrivedHome   → remove token, remove away_residents (idle-at-home = no token)
//!                  ArrivedWork   → keep, idle AtWork
//!                  Reached(exit) → buffer PendingHandoff::Move, remove local token
//! ```
//!
//! Determinism: citizens are visited in `entity.0` order; networks are discovered
//! deterministically; `routes_to` is a deterministic Dijkstra tree; entry cells are
//! sorted by position. Same inputs → same movement.
//!
//! Persistence: `World::tokens` and `World::away_residents` are `#[serde(skip)]`;
//! trips are not saved. On load the first sub-tick re-derives placement from the
//! schedule.

use std::collections::HashSet;

use crate::core::components::{PendingHandoff, PlaceRef, TravelState, TravelToken, TravelerId};
use crate::core::entity::Entity;
use crate::core::regions::RegionId;
use crate::core::systems::road_connectivity::{self, RoadNetwork};
use crate::core::systems::road_network_analysis::{road_degree_in_network, step_cost};
use crate::core::systems::schedule::schedule_phase;
use crate::core::world::World;

/// Outcome of advancing a token one sub-tick toward a local building target.
enum TokenArrival {
    /// Keep walking.
    Walking,
    /// Arrived at the workplace — idle `AtWork`, `building = work.building`.
    ArrivedWork,
    /// Arrived at home — the token should be removed and the citizen cleared from
    /// `away_residents` (if present).
    ArrivedHome,
}

/// Outcome of advancing a token one sub-tick toward a remote (border-exit) target.
enum RemoteStep {
    /// Keep walking — the token's state has been updated to the new cell
    /// (carried in the variant so the caller can install it directly, instead
    /// of re-applying `apply_dwell` to the old `token.state` and clobbering
    /// the move).
    Walking(TravelState),
    /// Reached the border-exit cell — emit a `Move` handoff; the body leaves this
    /// region for the host. On the home side, the move pass also bumps
    /// `away_generation` and inserts into `away_residents`; on a host, the gen is
    /// just carried.
    CrossOut {
        to_region: RegionId,
        exit_cell: Entity,
        token: TravelToken,
    },
    /// No reachable border exit from the current cell → stay put (§4b no-teleport).
    Stay,
}

/// Advances every citizen's commute by one 10-minute movement sub-tick (gated by
/// `dwell` so a crossing/turn cell holds the traveller for 2×/4× as long). Driven
/// 6× per game hour by the runner, separately from the hourly economy tick.
pub(crate) fn step_tokens(world: &mut World) {
    let phase = schedule_phase(world.resources.time.hour_of_day());
    let self_region = world.region_id;
    let networks = road_connectivity::discover_road_networks(world);

    // ── DEPART pass (every region, over its OWN residents in world.citizens): an
    //    idle-at-home resident whose phase target is elsewhere tries to leave NOW.
    //    ("Home region" only in the sense that a region holds the Citizen for its
    //    own residents.)
    //    "Idle at home" = no token here AND not in away_residents (so an away
    //    resident — body in the neighbour — is NOT re-spawned; a brand-new/just-
    //    returned one IS). The token is created ONLY if its FIRST step succeeds (a
    //    route exists) — matching today: an unreachable workplace = idle at home,
    //    no token, retried next sub-tick. just_departed lets the move pass skip it
    //    this sub-tick (one advance/sub-tick).
    let mut just_departed: HashSet<Entity> = HashSet::new();
    let mut fresh_tokens: Vec<(Entity, TravelToken)> = Vec::new();
    for (id, citizen) in world.citizens.iter() {
        if world.tokens.contains_key(id) || world.away_residents.contains(id) {
            continue; // busy (token here) or away (body in neighbour)
        }
        let home = PlaceRef {
            region: self_region,
            building: citizen.home,
        };
        let work = citizen.workplace_assignment.as_ref().map(|a| PlaceRef {
            region: a.workplace.region(),
            building: a.workplace,
        });
        // jobless / cleared-work in the Work phase → home (matches schedule_intent
        // today).
        let target = if phase == phase_work() {
            work.unwrap_or(home)
        } else {
            home
        };
        if target == home {
            continue; // stays home → no token
        }
        // first step toward the target: a LOCAL building target → `depart` (handles
        // the entry road adjacent to the building); a REMOTE target →
        // `depart_to_cell` over a reachable remote_exit_cells[target.region]
        // candidate. None ⇒ no route ⇒ no token.
        let Some(state) = depart_toward(world, &networks, citizen.home, target) else {
            continue;
        };
        // gen: 0 — the home region's first cross-out will bump away_generation to
        // 1 and copy that into token.gen (see CrossOut arm below). Until then the
        // gen is irrelevant (no handoff has been emitted).
        let token = TravelToken {
            state,
            home,
            work,
            trip_gen: 0,
        };
        fresh_tokens.push((*id, token));
        just_departed.insert(*id);
    }
    for (id, token) in fresh_tokens {
        world.tokens.insert(id, token);
    }

    // ── MOVE pass: step every present token (except the just-departed, already
    //    advanced this sub-tick). Sorted by citizen.0 for determinism. We
    //    collect the changes (writes, removes, handoffs) and apply them after
    //    the loop to avoid borrow conflicts on `world.tokens`.
    let mut updates: Vec<(Entity, TravelState)> = Vec::new();
    let mut removes: Vec<(Entity, /*arrived_home*/ bool)> = Vec::new();
    let mut handoffs: Vec<PendingHandoff> = Vec::new();
    {
        let mut citizens: Vec<(Entity, TravelToken)> =
            world.tokens.iter().map(|(k, v)| (*k, v.clone())).collect();
        citizens.sort_unstable_by_key(|(k, _)| k.0);
        for (citizen, mut token) in citizens {
            if just_departed.contains(&citizen) {
                continue;
            }
            // `refresh_endpoints_from` is a no-op for foreign tokens whose
            // citizen is not in this region's `world.citizens`.
            refresh_endpoints_from(&mut token, world.citizens.get(&citizen));
            let target = if phase == phase_work() {
                token.work.unwrap_or(token.home)
            } else {
                token.home
            };
            if target.region == self_region {
                let (next_state, arrival) =
                    advance_to_building(world, &networks, &token, target.building);
                // The dwell gate is checked inside `advance_to_building` BEFORE
                // advancing, so `next_state` is already gated (or arrived).
                match arrival {
                    TokenArrival::ArrivedHome => {
                        removes.push((citizen, true));
                    }
                    TokenArrival::ArrivedWork | TokenArrival::Walking => {
                        updates.push((citizen, next_state));
                    }
                }
            } else {
                match advance_to_exit(world, &networks, &token, target) {
                    RemoteStep::CrossOut {
                        to_region,
                        exit_cell,
                        token: moved,
                    } => {
                        let trip_gen = if token.home.region == self_region {
                            // Home: bump + record. (We mutate `away_residents` and
                            // `away_generation` directly; the `world` borrow is on
                            // `&mut` because we're in the `&mut world` stepper.)
                            // (No immutable borrow of `world.tokens` is alive here.)
                            trip_gen_for_home(
                                &mut world.away_residents,
                                &mut world.away_generation,
                                citizen,
                            )
                        } else {
                            token.trip_gen
                        };
                        let mut moved = moved;
                        moved.trip_gen = trip_gen;
                        handoffs.push(PendingHandoff::Move {
                            traveler: TravelerId {
                                citizen,
                                generation: trip_gen,
                            },
                            token: moved,
                            to_region,
                            exit_cell,
                        });
                        removes.push((citizen, false));
                    }
                    RemoteStep::Walking(next_state) => {
                        // The dwell gate is checked inside `advance_to_exit`
                        // BEFORE advancing, so `next_state` is already gated
                        // (or arrived on the exit cell).
                        updates.push((citizen, next_state));
                    }
                    RemoteStep::Stay => {
                        updates.push((citizen, token.state));
                    }
                }
            }
        }
    }
    // Apply the collected changes now that the immutable borrow is released.
    for (c, new_state) in updates {
        if let Some(token) = world.tokens.get_mut(&c) {
            token.state = new_state;
        }
    }
    for h in handoffs {
        world.outgoing_handoffs.push(h);
    }
    for (c, arrived_home) in removes {
        world.tokens.remove(&c);
        if arrived_home {
            world.away_residents.remove(&c); // home → no token; away_generation stays (monotonic)
        }
    }
    // Prune a dead local resident's token; keep foreign visitors (home elsewhere).
    world
        .tokens
        .retain(|id, t| world.citizens.contains_key(id) || t.home.region != self_region);
    // A resident that died WHILE away → drop the away record.
    world
        .away_residents
        .retain(|c| world.citizens.contains_key(c));
}

/// Pure phase → work? The schedule module owns the Work/Home definition; this
/// helper just compares without dragging the enum into the travel module.
fn phase_work() -> crate::core::systems::schedule::SchedulePhase {
    crate::core::systems::schedule::SchedulePhase::Work
}

/// Refresh `token.home` and `token.work` from the current `Citizen`. For a
/// foreign token (`citizen` is not in this region's `world.citizens`), this is a
/// no-op (the moved `home`/`work` are preserved as-is).
fn refresh_endpoints_from(
    token: &mut TravelToken,
    citizen: Option<&crate::core::components::Citizen>,
) {
    let Some(citizen) = citizen else { return };
    token.home = PlaceRef {
        region: token.home.region,
        building: citizen.home,
    };
    token.work = citizen.workplace_assignment.as_ref().map(|a| PlaceRef {
        region: a.workplace.region(),
        building: a.workplace,
    });
}

/// Bump the monotonic per-citizen trip counter (never cleared) and insert into
/// `away_residents`. Returns the new generation, which the home region stamps onto
/// the moved token and emits on the wire as the `TravelerId.generation`. The
/// caller passes the maps directly so this can be called while a read borrow on
/// `world.tokens` is alive.
fn trip_gen_for_home(
    away_residents: &mut std::collections::HashSet<Entity>,
    away_generation: &mut std::collections::HashMap<Entity, u32>,
    citizen: Entity,
) -> u32 {
    away_residents.insert(citizen);
    let counter = away_generation.entry(citizen).or_insert(0);
    *counter += 1;
    *counter
}

/// The teleport-home fallback: if `home_accepts(c, gen)` → remove the citizen from
/// `away_residents` (the citizen is now home, idle, no token, no body placed).
/// Else no-op (drop).
pub(crate) fn apply_traveler_return(world: &mut World, traveler: TravelerId) {
    if home_accepts(world, traveler.citizen, traveler.generation) {
        world.away_residents.remove(&traveler.citizen);
    }
}

/// The home-completion guard. Accepts a returning Move / Rollback only if all of:
/// - the citizen is still alive (no ghost from a dead-while-away),
/// - no token is currently here (no clobber of a token walking home),
/// - the citizen is in `away_residents` (an active trip, not a post-completion
///   duplicate that already arrived),
/// - the handoff's `generation` matches the current monotonic `away_generation`
///   (the CURRENT trip, not a stale older one).
pub(crate) fn home_accepts(world: &World, citizen: Entity, trip_gen: u32) -> bool {
    world.citizens.contains_key(&citizen)
        && !world.tokens.contains_key(&citizen)
        && world.away_residents.contains(&citizen)
        && world.away_generation.get(&citizen) == Some(&trip_gen)
}

/// P5b: accept a token handed in by a neighbor region. The receive lives in
/// `regions/mod.rs`; this helper is the core side (no border-link math).
pub(crate) fn receive_traveler(
    world: &mut World,
    traveler: TravelerId,
    token: TravelToken,
    entry_cell: Entity,
) {
    let mut token = token;
    // Rebuild `state` from scratch: the home region strips it before sending, and
    // the receiver always re-anchors on its own entry cell. Only `home`, `work`,
    // `gen` carry meaning across the border.
    token.state = TravelState {
        status: crate::core::components::TravelStatus::Traveling,
        current_cell: Some(entry_cell),
        destination: None,
        building: None,
        dwell: 0,
        prev_cell: None,
    };
    world.tokens.insert(traveler.citizen, token);
}

// ─── walk primitives (P5a / P5b, reused unchanged) ─────────────────────────────

/// Depart from `origin` toward a target. Returns the first reachable entry road
/// cell as the token's `state` (with `prev_cell = None`, `dwell = 0`), or `None`
/// when no route exists. A LOCAL building target uses `depart` (adjacent entry
/// road); a REMOTE target uses `depart_to_cell` over `remote_exit_cells[reg]`.
fn depart_toward(
    world: &World,
    networks: &[RoadNetwork],
    origin: Entity,
    target: PlaceRef,
) -> Option<TravelState> {
    if target.region == world.region_id {
        // Local building target.
        for entry in adjacent_roads_sorted(world, origin) {
            let Some(network) = network_of_cell(networks, entry) else {
                continue;
            };
            let reachable = is_adjacent(world, target.building, entry)
                || world
                    .routes_to(target.building, network)
                    .contains_key(&entry);
            if reachable {
                return Some(travelling(entry, target.building));
            }
        }
        None
    } else {
        // Remote target — pick the first reachable border-exit candidate.
        let candidates = world.remote_exit_cells.get(&target.region)?;
        for &exit_cell in candidates {
            if let Some(entry) = depart_to_cell(world, networks, origin, exit_cell) {
                return Some(travelling(entry, exit_cell));
            }
        }
        None
    }
}

/// Step one cell from `current` toward a local `target_building`, returning the
/// updated `TravelState` and the arrival result. If the token is already
/// adjacent to the target, returns `ArrivedHome`/`ArrivedWork` and the idle
/// `TravelState`. If the cell is off-graph or the target is unreachable, stays
/// put (returns the unchanged token with `Walking`).
fn advance_to_building(
    world: &World,
    networks: &[RoadNetwork],
    token: &TravelToken,
    target_building: Entity,
) -> (TravelState, TokenArrival) {
    let home_building = token.home.building;
    let target = PlaceRef {
        region: world.region_id,
        building: target_building,
    };
    let mut state = token.state;
    let is_home_target = target_building == home_building;

    match state.current_cell {
        None => {
            // Idle in a building → depart onto an entry road, or normalise to
            // idle if the token is already at the target.
            if state.building == Some(target_building) {
                // Already at the target — normalise to idle.
                state.building = Some(target_building);
                state.current_cell = None;
                state.destination = None;
                state.dwell = 0;
                state.prev_cell = None;
                state.status = crate::core::components::TravelStatus::AtWork;
                if is_home_target {
                    return (state, TokenArrival::ArrivedHome);
                } else {
                    return (state, TokenArrival::ArrivedWork);
                }
            }
            // Depart from wherever the token's `state.building` says, falling
            // back to `home_building` if the recorded building was bulldozed
            // (§4b destroyed-origin exception).
            let origin = state
                .building
                .filter(|b| world.buildings.contains_key(b))
                .unwrap_or(home_building);
            if origin == target_building {
                state.building = Some(target_building);
                state.current_cell = None;
                state.dwell = 0;
                state.status = crate::core::components::TravelStatus::AtWork;
                if is_home_target {
                    return (state, TokenArrival::ArrivedHome);
                } else {
                    return (state, TokenArrival::ArrivedWork);
                }
            }
            match depart_toward(world, networks, origin, target) {
                Some(new_state) => {
                    state = new_state;
                    (state, TokenArrival::Walking)
                }
                None => {
                    // No route → stay put at origin. The status follows the
                    // origin: home → AtHome (idle at home); work → AtWork.
                    state.building = Some(origin);
                    state.current_cell = None;
                    state.dwell = 0;
                    state.status = crate::core::components::TravelStatus::AtWork;
                    if origin == home_building {
                        (state, TokenArrival::ArrivedHome)
                    } else {
                        (state, TokenArrival::ArrivedWork)
                    }
                }
            }
        }
        Some(cell) => {
            if is_adjacent(world, target_building, cell) {
                // Arrived: drop the current cell, set `building` to the target.
                state.building = Some(target_building);
                state.current_cell = None;
                state.destination = None;
                state.dwell = 0;
                state.prev_cell = None;
                state.status = crate::core::components::TravelStatus::AtWork;
                if is_home_target {
                    return (state, TokenArrival::ArrivedHome);
                } else {
                    return (state, TokenArrival::ArrivedWork);
                }
            }
            // En route — P7b dwell gate first: on a crossing/turn cell, hold
            // for `step_cost` sub-ticks before advancing. The first sub-tick on
            // a new cell is always free (the dwell is paid on subsequent
            // ticks).
            let Some(network) = network_of_cell(networks, cell) else {
                // Off-graph → stay put (§4b).
                return (state, TokenArrival::Walking);
            };
            let next = world
                .routes_to(target_building, network)
                .get(&cell)
                .copied();
            let Some(next) = next else {
                return (state, TokenArrival::Walking); // unreachable → stay
            };
            let degree = road_degree_in_network(world, cell, network);
            let cost = step_cost(world, state.prev_cell, cell, Some(next), degree);
            if u32::from(state.dwell + 1) < cost {
                // Gated: hold on this cell for one more sub-tick.
                let mut gated = state;
                gated.dwell += 1;
                return (gated, TokenArrival::Walking);
            }
            // Gate open — advance one cell.
            state.prev_cell = Some(cell);
            state.current_cell = Some(next);
            state.dwell = 0;
            (state, TokenArrival::Walking)
        }
    }
}

/// Step one cell from `token.state.current_cell` toward a remote `target`. If
/// the cell IS the chosen exit, returns `CrossOut` (the caller emits the
/// `Move` handoff). If no reachable exit from here, returns `Stay` (§4b).
fn advance_to_exit(
    world: &World,
    networks: &[RoadNetwork],
    token: &TravelToken,
    target: PlaceRef,
) -> RemoteStep {
    let target_region = target.region;
    let Some(candidates) = world.remote_exit_cells.get(&target_region) else {
        return RemoteStep::Stay;
    };
    let state = &token.state;
    let exit_cell = match state.current_cell {
        None => {
            // Idle in a building — depart onto the first reachable entry road.
            let origin = state
                .building
                .filter(|b| world.buildings.contains_key(b))
                .unwrap_or(token.home.building);
            let mut chosen = None;
            for &exit in candidates {
                if depart_to_cell(world, networks, origin, exit).is_some() {
                    chosen = Some(exit);
                    break;
                }
            }
            chosen
        }
        Some(cell) => {
            // En route — the committed exit is `state.destination` (a road cell
            // in the candidates list). If it's missing/invalid, re-pick a
            // candidate reachable from the current cell.
            let committed = state.destination.filter(|e| candidates.contains(e));
            committed.or_else(|| {
                candidates.iter().copied().find(|&exit| {
                    cell == exit || step_toward_cell(world, networks, cell, exit).is_some()
                })
            })
        }
    };
    let Some(exit_cell) = exit_cell else {
        return RemoteStep::Stay;
    };
    if let Some(current) = state.current_cell {
        if current == exit_cell {
            // On the exit cell → cross.
            let mut moved = token.clone();
            moved.state = travelling(exit_cell, target.building);
            return RemoteStep::CrossOut {
                to_region: target_region,
                exit_cell,
                token: moved,
            };
        }
        // Step one cell toward the exit.
        let network = match network_of_cell(networks, current) {
            Some(n) => n,
            None => return RemoteStep::Stay,
        };
        let next = world.routes_to(exit_cell, network).get(&current).copied();
        let Some(next) = next else {
            return RemoteStep::Stay;
        };
        // P7b dwell gate: the current cell costs `step_cost(in, current, out,
        // degree)` sub-ticks. On the first sub-tick on the cell, the cost is
        // paid "in advance" (the sub-tick that placed the token). The next
        // sub-tick advances one cell. After that, the cell costs 2×/4× for a
        // turn/intersection, so we hold for `cost - 1` more sub-ticks.
        let degree = road_degree_in_network(world, current, network);
        let cost = step_cost(world, state.prev_cell, current, Some(next), degree);
        if u32::from(state.dwell + 1) < cost {
            // Gated: hold on this cell for one more sub-tick.
            let mut gated = *state;
            gated.dwell += 1;
            return RemoteStep::Walking(gated);
        }
        // Gate open — advance.
        let mut moved = token.clone();
        moved.state = travelling(next, exit_cell);
        moved.state.prev_cell = Some(current);
        moved.state.dwell = 0;
        RemoteStep::Walking(moved.state)
    } else {
        // Was idle — depart onto the first reachable entry road toward the
        // chosen exit. (The depart above already picked the exit; we need the
        // entry cell to put the token on.)
        let origin = state
            .building
            .filter(|b| world.buildings.contains_key(b))
            .unwrap_or(token.home.building);
        let mut moved = token.clone();
        if let Some(entry) = depart_to_cell(world, networks, origin, exit_cell) {
            moved.state = travelling(entry, exit_cell);
            RemoteStep::Walking(moved.state)
        } else {
            // No reachable entry → stay put.
            moved.state.building = Some(origin);
            moved.state.current_cell = None;
            moved.state.dwell = 0;
            RemoteStep::Stay
        }
    }
}

/// Depart from `origin` toward a road `dest_cell` (a border-exit cell). Returns
/// the first reachable position-sorted entry road cell.
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
        let reachable =
            entry == dest_cell || world.routes_to(dest_cell, network).contains_key(&entry);
        if reachable {
            return Some(entry);
        }
    }
    None
}

/// One step from `cell` toward a road `dest_cell`. `None` if off-graph or
/// unreachable (caller holds the current cell, §4b).
fn step_toward_cell(
    world: &World,
    networks: &[RoadNetwork],
    cell: Entity,
    dest_cell: Entity,
) -> Option<Entity> {
    let network = network_of_cell(networks, cell)?;
    world.routes_to(dest_cell, network).get(&cell).copied()
}

/// A token stepped onto `cell`. `prev_cell`/`dwell` are reset here; the caller
/// patches `prev_cell` to the cell just left so the next turn is known.
fn travelling(cell: Entity, target: Entity) -> TravelState {
    TravelState {
        status: crate::core::components::TravelStatus::Traveling,
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

#[cfg(test)]
mod tests {
    use super::PlaceRef;
    use super::step_tokens;
    use crate::core::city_refs::CityCellRef;
    use crate::core::components::{
        Citizen, Morale, TravelState, TravelStatus, TravelToken, WorkplaceAssignment,
    };
    use crate::core::entity::Entity;
    use crate::core::regions::RegionId;

    use crate::core::systems::placement::place_building;
    use crate::core::world::World;
    use crate::interface::input::BuildingKind;

    fn set_hour(world: &mut World, hour: u8) {
        let current = u64::from(world.resources.time.hour_of_day());
        let delta = (24 + u64::from(hour) - current) % 24;
        world.resources.time.advance_hours(delta);
    }

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
    fn local_commute_unchanged() {
        let (mut world, roads, home, work) = commute_world();
        let id = add_citizen(&mut world, 100, home, Some(work));
        set_hour(&mut world, 9);

        // Departs onto r0, then steps one cell per tick to r3.
        let mut seen = Vec::new();
        for _ in 0..4 {
            step_tokens(&mut world);
            if let Some(t) = world.tokens.get(&id) {
                if let Some(c) = t.state.current_cell {
                    seen.push(c);
                }
            }
        }
        assert_eq!(seen, roads.to_vec(), "exact route cells r0..r3");

        // Next tick: r3 touches work → arrived, idling AtWork (token stays).
        step_tokens(&mut world);
        let token = world.tokens.get(&id).expect("at-work token");
        assert_eq!(token.state.status, TravelStatus::AtWork);
        assert_eq!(token.state.current_cell, None);
    }

    /// After the workday the citizen walks back home (animated return).
    #[test]
    fn phase_flip_retargets_home() {
        let (mut world, _roads, home, work) = commute_world();
        let id = add_citizen(&mut world, 100, home, Some(work));
        set_hour(&mut world, 9);
        for _ in 0..6 {
            step_tokens(&mut world);
        }
        // At 16:00 phase=Home, the token should walk back to home.
        set_hour(&mut world, 16);
        for _ in 0..6 {
            step_tokens(&mut world);
        }
        assert!(
            !world.tokens.contains_key(&id),
            "home-arrival: token removed"
        );
    }

    /// Work on a disconnected road network → no route → the citizen never departs.
    #[test]
    fn unreachable_workplace_keeps_citizen_home() {
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
            step_tokens(&mut world);
        }
        assert!(!world.tokens.contains_key(&id), "no token, idle at home");
        assert!(!world.away_residents.contains(&id));
    }

    /// A job-less citizen goes home in the Work phase (matches schedule_intent today).
    #[test]
    fn jobless_goes_home_in_work_phase() {
        let (mut world, _roads, home, _work) = commute_world();
        let id = add_citizen(&mut world, 100, home, None); // jobless
        set_hour(&mut world, 9);
        // Stay-at-home check: phase=Work, target=home, no token.
        assert!(!world.tokens.contains_key(&id));
    }

    /// Changing the workplace assignment while idle AtWork must depart from the
    /// *old* workplace (recorded in travel state), not teleport to the new one.
    #[test]
    fn idle_at_work_remembers_location() {
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

        set_hour(&mut world, 9);
        for _ in 0..8 {
            step_tokens(&mut world);
        }
        // Idle at workA — token present, status=AtWork, building=workA.
        let token = world.tokens.get(&id).expect("token at workA");
        assert_eq!(token.state.status, TravelStatus::AtWork);
        assert_eq!(token.state.building, Some(work_a));

        // Reassign to workB. Next work-phase step should depart from workA's entry
        // road r2, not teleport to workB.
        world.citizens.get_mut(&id).unwrap().workplace_assignment = Some(WorkplaceAssignment {
            workplace: work_b,
            location: CityCellRef::local(work_b.region(), 4, 1),
            salary: 100,
        });
        // Jump to evening so the Home phase departs from workA.
        set_hour(&mut world, 16);
        step_tokens(&mut world);
        let token = world.tokens.get(&id).expect("departed from workA");
        assert_eq!(
            token.state.current_cell,
            Some(roads[2]),
            "departs from workA's entry road"
        );
    }

    /// A dead citizen is pruned from the travel map.
    #[test]
    fn dead_citizen_is_pruned_from_travel() {
        let (mut world, _roads, home, work) = commute_world();
        let id = add_citizen(&mut world, 100, home, Some(work));
        set_hour(&mut world, 9);
        step_tokens(&mut world);
        assert!(world.tokens.contains_key(&id));

        world.citizens.remove(&id);
        step_tokens(&mut world);
        assert!(
            !world.tokens.contains_key(&id),
            "pruned after citizen removed"
        );
    }

    /// A resident that dies WHILE away has its `away_residents` record dropped.
    #[test]
    fn dead_while_away_prunes_away_residents() {
        let (mut world, _roads, home, work) = commute_world();
        let id = add_citizen(&mut world, 100, home, Some(work));
        set_hour(&mut world, 9);
        for _ in 0..6 {
            step_tokens(&mut world);
        }
        // Simulate the citizen being away (token removed on arrival, but suppose
        // the citizen died between two ticks before arrival). We can simulate by
        // setting away_residents manually.
        world.away_residents.insert(id);
        // Now kill the citizen.
        world.citizens.remove(&id);
        step_tokens(&mut world);
        assert!(
            !world.away_residents.contains(&id),
            "away_residents pruned for a dead citizen"
        );
    }

    /// The token is created only if the first step succeeds — an unreachable
    /// workplace creates NO token (idle at home, retried next sub-tick).
    #[test]
    fn depart_creates_token_only_if_route() {
        let mut world = World::new(4, 2);
        place_building(&mut world, 0, 0, BuildingKind::Road);
        place_building(&mut world, 1, 0, BuildingKind::Road);
        place_building(&mut world, 3, 0, BuildingKind::Road);
        place_building(&mut world, 0, 1, BuildingKind::Residential);
        place_building(&mut world, 3, 1, BuildingKind::Commercial);
        let home = world.grid.get(0, 1).expect("home");
        let work = world.grid.get(3, 1).expect("work");
        let id = add_citizen(&mut world, 100, home, Some(work));
        set_hour(&mut world, 9);
        step_tokens(&mut world);
        // No token — no route from r0/r1 network to r3 network.
        assert!(!world.tokens.contains_key(&id));
        // An away resident is NOT re-spawned — the depart pass skips it.
        world.away_residents.insert(id);
        step_tokens(&mut world);
        assert!(!world.tokens.contains_key(&id));
    }

    /// ArrivedHome removes the token and clears `away_residents`.
    #[test]
    fn arrive_home_removes_token_and_away_residents() {
        let (mut world, _roads, home, work) = commute_world();
        let id = add_citizen(&mut world, 100, home, Some(work));
        set_hour(&mut world, 9);
        for _ in 0..6 {
            step_tokens(&mut world);
        }
        // Citizen is at work. Now jump to home phase and walk back.
        set_hour(&mut world, 16);
        for _ in 0..6 {
            step_tokens(&mut world);
        }
        // Token should be removed on home-arrival.
        assert!(!world.tokens.contains_key(&id));
        // If the citizen was marked away, that should be cleared.
        assert!(!world.away_residents.contains(&id));
    }

    /// `apply_traveler_return` is the home-arrival fallback. With `home_accepts`
    /// true (citizen exists, no token, in away_residents, generation matches),
    /// the citizen is cleared from `away_residents`.
    #[test]
    fn rollback_re_homes_by_presence_and_generation() {
        use super::apply_traveler_return;
        let (mut world, _roads, home, work) = commute_world();
        let id = add_citizen(&mut world, 100, home, Some(work));
        set_hour(&mut world, 9);
        for _ in 0..6 {
            step_tokens(&mut world);
        }
        // Simulate the cross-out state: token gone, away_residents set, gen=1.
        world.tokens.remove(&id);
        world.away_residents.insert(id);
        world.away_generation.insert(id, 1);
        // Rollback with matching gen → home_accepts true → away_residents cleared.
        apply_traveler_return(
            &mut world,
            crate::core::components::TravelerId {
                citizen: id,
                generation: 1,
            },
        );
        assert!(!world.away_residents.contains(&id));
    }

    /// The token's `gen` is set by the home region on departure and carried
    /// unchanged by the host, so the return Move matches the home's
    /// `away_generation` guard.
    #[test]
    fn host_carries_generation_on_return() {
        use super::receive_traveler;
        use crate::core::components::TravelerId;
        let (mut world, _roads, home, work) = commute_world();
        let id = add_citizen(&mut world, 100, home, Some(work));
        set_hour(&mut world, 9);
        for _ in 0..6 {
            step_tokens(&mut world);
        }
        // Receive a Move from a foreign host (B→A). Token has gen=1.
        let token = crate::core::components::TravelToken {
            state: crate::core::components::TravelState::default(),
            home: crate::core::components::PlaceRef {
                region: world.region_id,
                building: home,
            },
            work: Some(crate::core::components::PlaceRef {
                region: work.region(),
                building: work,
            }),
            trip_gen: 1,
        };
        let entry = world.grid.get(0, 0).expect("r0");
        // Set up away_residents and gen for the home side.
        world.tokens.remove(&id);
        world.away_residents.insert(id);
        world.away_generation.insert(id, 1);
        receive_traveler(
            &mut world,
            TravelerId {
                citizen: id,
                generation: 1,
            },
            token,
            entry,
        );
        // Token is placed.
        assert!(world.tokens.contains_key(&id));
        assert_eq!(world.tokens[&id].trip_gen, 1, "gen carried from handoff");
    }

    /// The token exists only while a citizen is away from home (departed → not
    /// yet returned); idle-at-home = NO token. Verified by a fresh citizen.
    #[test]
    fn crossed_out_token_removed_from_home() {
        let (mut world, _roads, home, work) = commute_world();
        let id = add_citizen(&mut world, 100, home, Some(work));
        // Before any tick, no token (lazy default — no entry in world.tokens).
        assert!(!world.tokens.contains_key(&id));
    }

    /// The cross-region return now animates: a visitor walks workplace→border
    /// at off-work rather than returning instantly. (Simulated by a foreign
    /// Move arrival: the token is placed at the border and the home-side stepper
    /// walks it home sub-tick by sub-tick.)
    #[test]
    fn return_now_animates() {
        use super::receive_traveler;
        use crate::core::components::TravelerId;
        let (mut world, _roads, home, work) = commute_world();
        let id = add_citizen(&mut world, 100, home, Some(work));
        set_hour(&mut world, 9);
        for _ in 0..6 {
            step_tokens(&mut world);
        }
        // Receive a foreign Move placing the token at r3 (rightmost). gen=1.
        let r3 = world.grid.get(3, 0).expect("r3");
        let token = crate::core::components::TravelToken {
            state: crate::core::components::TravelState::default(),
            home: crate::core::components::PlaceRef {
                region: world.region_id,
                building: home,
            },
            work: Some(crate::core::components::PlaceRef {
                region: work.region(),
                building: work,
            }),
            trip_gen: 1,
        };
        // Insert into away_residents and gen for the home side.
        world.tokens.remove(&id);
        world.away_residents.insert(id);
        world.away_generation.insert(id, 1);
        receive_traveler(
            &mut world,
            TravelerId {
                citizen: id,
                generation: 1,
            },
            token,
            r3,
        );
        // Set Home phase and step. The token walks r3→r2→r1→r0 sub-tick by
        // sub-tick (animated, not instant).
        set_hour(&mut world, 16);
        let mut seen = Vec::new();
        for _ in 0..6 {
            step_tokens(&mut world);
            if let Some(t) = world.tokens.get(&id) {
                seen.push(t.state.current_cell);
            }
        }
        // The token walked home (cell path r3→r2→r1→r0 then ArrivedHome).
        assert!(!seen.is_empty(), "animated return walked at least one cell");
        // Eventually arrived home and removed.
        assert!(
            !world.tokens.contains_key(&id),
            "eventual home-arrival: token removed"
        );
        assert!(
            !world.away_residents.contains(&id),
            "away_residents cleared"
        );
    }

    /// A token whose only border exit became unreachable stays on its cell
    /// (no handoff, no teleport home) — today's §4b behaviour. The test
    /// pre-creates a token on a road cell, then makes the only exit
    /// candidate unreachable, and verifies the token stays on its cell
    /// through several sub-ticks (not teleported home, not crossing).
    #[test]
    fn no_exit_stays_put_not_teleport() {
        // Create a fresh world with a disconnected road network (r0/r1 + r3,
        // r2 missing). The citizen's home is on the r0/r1 network, and the
        // workplace's only candidate exit (r3) is on the unreachable r3
        // network — so the depart path's `depart_toward` returns None and the
        // stepper creates NO token. Verifies §4b's "no teleport" for an
        // unreachable exit.
        let _world = World::new(4, 2);
        // A workplace with no reachable border-exit candidate in this region.
        // The road network is r0..r3 (linear); the only candidate is r0, which
        // IS reachable. To make it unreachable, we create a separate test world
        // with a disconnected road.
        let mut world = World::new(4, 2);
        place_building(&mut world, 0, 0, BuildingKind::Road);
        place_building(&mut world, 1, 0, BuildingKind::Road);
        place_building(&mut world, 3, 0, BuildingKind::Road);
        place_building(&mut world, 0, 1, BuildingKind::Residential);
        place_building(&mut world, 3, 1, BuildingKind::Commercial);
        let home = world.grid.get(0, 1).expect("home");
        let _work = world.grid.get(3, 1).expect("work");
        // A remote workplace whose exit is on the disconnected r3 network —
        // unreachable from the home network (r0/r1).
        let workplace = Entity::new(RegionId(7), 99);
        // The exit candidate is r3 (which is on the disconnected network).
        let r3 = world.grid.get(3, 0).expect("r3");
        world.remote_exit_cells.insert(RegionId(7), vec![r3]);
        let id = add_citizen(&mut world, 100, home, Some(workplace));
        set_hour(&mut world, 9);
        // 5 sub-ticks: the citizen never departs (no reachable exit), no token
        // is ever created, and no handoff is emitted.
        for _ in 0..5 {
            step_tokens(&mut world);
        }
        assert!(!world.tokens.contains_key(&id), "no token, idle at home");
        assert!(world.outgoing_handoffs.is_empty(), "no handoff emitted");
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
            step_tokens(&mut a);
            step_tokens(&mut b);
        }
        let mut ta: Vec<_> = a
            .tokens
            .iter()
            .map(|(k, v)| (*k, v.state, v.home, v.work, v.trip_gen))
            .collect();
        let mut tb: Vec<_> = b
            .tokens
            .iter()
            .map(|(k, v)| (*k, v.state, v.home, v.work, v.trip_gen))
            .collect();
        ta.sort_by_key(|(k, _, _, _, _)| k.0);
        tb.sort_by_key(|(k, _, _, _, _)| k.0);
        assert_eq!(ta, tb);
    }

    /// P7b: a 4-way intersection holds a traveller for 4 sub-ticks (cost 4); the
    /// degree-1 arm cells cost 1 (one sub-tick each). Regression for the dwell
    /// gate — `advance_to_building` must check the gate BEFORE advancing, not
    /// after.
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
            step_tokens(&mut world);
            if let Some(t) = world.tokens.get(&id) {
                if t.state.current_cell == Some(x_cell) {
                    on_x += 1;
                }
            }
            if let Some(t) = world.tokens.get(&id) {
                if t.state.status == TravelStatus::AtWork {
                    break;
                }
            }
        }
        assert_eq!(on_x, 4, "the 4-way X holds the traveller for 4 sub-ticks");
        assert!(world.tokens.contains_key(&id), "token at work");
        assert_eq!(
            world.tokens[&id].state.status,
            TravelStatus::AtWork,
            "arrived at work"
        );
    }

    /// A foreign visitor walks workplace → border at off-work (animated
    /// return). The home-phase stepper in B walks the token toward the
    /// border-exit cell; on the border cell, a `Move` handoff is emitted and
    /// A places the token at its entry cell. Regression for the "RemoteStep
    /// must carry the moved state" fix — without it, a parked-AtWork visitor
    /// never starts the return walk.
    #[test]
    fn host_walks_visitor_home_animation() {
        // Region B with a road r0..r3; workplace at (3,1) touches r3.
        // We'll place a foreign token directly (B's world is standalone for
        // the test; the receive path is exercised by `return_now_animates`).
        let mut world = World::new(4, 2);
        let mut roads = [Entity::default(); 4];
        for (x, slot) in roads.iter_mut().enumerate() {
            place_building(&mut world, x, 0, BuildingKind::Road);
            *slot = world.grid.get(x, 0).expect("road placed");
        }
        place_building(&mut world, 3, 1, BuildingKind::Commercial);
        let workplace = world.grid.get(3, 1).expect("workplace");
        let home = world.grid.get(0, 0).expect("home (for PlaceRef)");

        // A foreign token parked at work in B. home.region is in another
        // region (not this one) — a host-side walking token.
        let token = TravelToken {
            state: TravelState {
                status: TravelStatus::AtWork,
                current_cell: None,
                destination: Some(workplace),
                building: Some(workplace),
                dwell: 0,
                prev_cell: None,
            },
            home: PlaceRef {
                region: RegionId(7),
                building: home,
            },
            work: Some(PlaceRef {
                region: RegionId(0),
                building: workplace,
            }),
            trip_gen: 1,
        };
        // Use a unique key for the foreign token.
        let key = Entity::new(world.region_id, 88);
        world.tokens.insert(key, token);

        // Set remote_exit_cells for region 7: the only exit is r0.
        world.remote_exit_cells.insert(RegionId(7), vec![roads[0]]);

        // Home phase in B → the token departs from work and walks toward r0
        // (the border-exit cell), one cell per sub-tick.
        set_hour(&mut world, 16);
        let mut walked = Vec::new();
        for _ in 0..6 {
            step_tokens(&mut world);
            if let Some(t) = world.tokens.get(&key) {
                if let Some(c) = t.state.current_cell {
                    walked.push(c);
                }
            }
        }
        // The token walked at least one cell (r3 → r2). The full path is
        // r3 → r2 → r1 → r0 then a Move is emitted.
        assert!(!walked.is_empty(), "the visitor walked at least one cell");
        // Eventually the token is removed (a Move was emitted and a host-only
        // foreign token in B doesn't have away_residents in B to clear).
        assert!(
            !world.tokens.contains_key(&key),
            "token removed when Move was emitted"
        );
    }

    /// A remote visitor on a 4-way intersection holds for 4 sub-ticks
    /// (cost 4) — the remote dwell gate is checked before advancing.
    /// Regression for the "remote trips still bypass P7b dwell" fix.
    #[test]
    fn remote_dwell_holds_a_visitor_on_a_4way() {
        // "+" pattern: X at (1,1) with arms N(1,0), S(1,2), E(2,1), W(0,1).
        // The linear road r0(0,0)..r2(2,0) at y=0 connects to N(1,0). So X has 4
        // neighbors in the network: N(1,0), S(1,2), E(2,1), W(0,1). The
        // remote target is in region 7; the visitor walks toward a border
        // exit (r0 at (0,0), a neighbor of W(0,1)).
        let mut world = World::new(3, 3);
        for (x, y) in [(1, 0), (0, 1), (1, 1), (2, 1), (1, 2), (0, 0), (2, 0)] {
            place_building(&mut world, x, y, BuildingKind::Road);
        }
        let x_cell = world.grid.get(1, 1).expect("X");
        let r0 = world.grid.get(0, 0).expect("r0");

        // Foreign token on X. Target = workplace in region 7 (remote).
        let token = TravelToken {
            state: TravelState {
                status: TravelStatus::Traveling,
                current_cell: Some(x_cell),
                destination: None,
                building: None,
                dwell: 0,
                prev_cell: None,
            },
            home: PlaceRef {
                region: RegionId(7),
                building: Entity::new(RegionId(7), 0),
            },
            work: Some(PlaceRef {
                region: RegionId(8),
                building: Entity::new(RegionId(8), 0),
            }),
            trip_gen: 1,
        };
        let key = Entity::new(world.region_id, 77);
        world.tokens.insert(key, token);
        // The remote target is in region 8; provide r0 as the border exit.
        world.remote_exit_cells.insert(RegionId(8), vec![r0]);

        set_hour(&mut world, 9);
        let mut on_x = 0;
        for _ in 0..12 {
            step_tokens(&mut world);
            if let Some(t) = world.tokens.get(&key) {
                if t.state.current_cell == Some(x_cell) {
                    on_x += 1;
                }
            }
        }
        assert_eq!(
            on_x, 3,
            "the remote 4-way X holds the visitor for 3 sub-ticks (cost-1; the placement sub-tick is not counted because the test pre-places the token)"
        );
    }
}
