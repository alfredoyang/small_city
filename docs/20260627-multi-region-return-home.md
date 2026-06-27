# 20260627 Multi-region commuting — dynamic both-way routing (incl. return home)

Status: **plan** (not implemented). Builds on P5 cross-region tokens + P7 sub-tick
movement. Pairs with `docs/travel-subtick-plan.md`.

## 1. Introduction / Problem

Direct-neighbour (2-region) commuting animates both ways: a citizen in A with a
workplace in neighbour B walks A→A/B exit, crosses, walks to work; at workday end a
`Return` handoff walks it B→border→home.

**Multi-region (3+) commuting does not work at all — in either direction.** Grounding
the current code:

```text
resolve_target (travel.rs:253):
  Work(workplace) not local
    → world.remote_exit_cells.get(workplace.region())   // DIRECT neighbours only
        Some → Target::BorderExit { to_region: workplace.region() }
        None → Target::Building(home)                   // idles at home, never commutes
```

`remote_exit_cells` is filled only for direct neighbours (`refresh`-side,
`regions/mod.rs:495`). So a citizen in A assigned a workplace in C (2 hops away)
**idles at home and never leaves** — there is no multi-hop outbound, hence nothing to
return. The original framing ("a citizen crossed multiple regions to work and can't get
home") does not match the code: the real gap is **multi-hop commuting in both
directions**.

The stored `return_path` stack (`ReturnHop`, `components.rs:324`) is also the wrong
long-term source of truth: if roads, border links, or topology change while a citizen
is away, every stored path can go stale, and rewriting every away/visiting token is
brittle.

**Goal.** One symmetric, **loop-safe, dynamic** routing rule for both directions,
reusing the existing structures (`remote_exit_cells`, `advance_to_exit`,
`Target::BorderExit`, `PendingHandoff`/`TravelerHandoff`, `VisitingToken`, the
`StepTravel` barrier). Remove/deprecate `return_path`. Stable facts the token already
carries:

```text
traveler.citizen.region() = home region  (the return target)
token.destination         = final workplace (the outbound target)
current region topology   = the routing graph, recomputed each step
current road graph        = the local path, recomputed each step
```

## 2. Proposal

### One rule, both directions — a region-topology distance gradient (loop-safe)

The previous draft of this plan picked the next hop from `CrossRegionDiscovery::
component_of` (connected-component **reachability**). That is **not loop-safe**: in
A–B–C the B/A border network is in the same component as C, so B looking for C could
pick the exit back toward A and ping-pong A→B→A. Reachability is not progress.

Routing instead follows a **BFS shortest-hop gradient on the region-topology graph**
(`RegionNeighborLink { region, edge, neighbor }`, `regions/mod.rs:132`, owned by the
directory). For a mover in region `R` heading to target region `T`:

```text
  next hop = a neighbour N with  dist(N, T) < dist(R, T)         (strict progress)
  exit candidates = local border cells whose link crosses to such an N
  component_of stays only as a REACHABILITY FILTER: keep T iff the road networks are
    actually connected R→…→T, so the citizen can physically walk the whole way.
  ties (several N at the same shorter distance) broken deterministically (by RegionId,
    then by exit cell position) so movement is reproducible.
```

`dist_to_T[X]` = hops from `X` to `T`, computed by BFS from `T` over the **reverse**
topology edges (`RegionNeighborLink` is directed; a mirrored layout makes forward and
reverse coincide, but reverse-edge BFS is correct either way). The next hop is any
outgoing edge `R → N` with `dist_to_T[N] < dist_to_T[R]`; strict monotone decrease
guarantees termination (no loops). Outbound `T = workplace.region()`; return
`T = traveler.citizen.region()`. Identical machinery, opposite target.

### Multi-region flow (A → B → C, then home)

```text
Home A                   Transit B                  Work C
------                   ---------                  ------
T=C; next hop=B (dist B<dist A)
walk A→A/B exit
Outbound ──────────────> T=C; next hop=C
                         walk B/A → B/C exit
                         Outbound ───────────────> T=C reached: walk to workplace
                                                    workday ends → T=A
                         Return <───────────────── next hop=B (dist B<dist C)
                         T=A; next hop=A
                         walk B/C → B/A exit
Return <──────────────── final home region: walk border → home
```

### Road / topology change behaviour

```text
Stored return_path:  fixed C→B→A; a link change strands the token unless every token
                     is rewritten.
Gradient (this plan): each ReceiveTraveler/StepTravel recomputes dist(·,T) and the
                     reachable exits from the CURRENT snapshot. Road removed → the
                     local exit re-pick (advance_to_exit) chooses another reachable
                     exit toward a still-closer neighbour; topology changed → the
                     gradient points elsewhere. If no progressing+reachable exit
                     exists, emit PendingHandoff::Rollback → routed directly to the home
                     region by id → apply_traveler_return clears Away (never strand,
                     cosmetic teleport only when the road route home is severed).
```

### Worker / barrier behaviour is unchanged

```text
RegionalGame::advance → RegionalGameRunner::step_travel_city
  → broadcast RegionEvent::StepTravel
  → process_region_events_for_barrier(usize::MAX)   FIFO: ReceiveTraveler (prev sub-tick),
                                                          then StepTravel (this sub-tick)
  → deliver forwarded TravelerHandedOff for the NEXT sub-tick
```

No new worker command. Each crossing still lands one sub-tick later (the existing
one-sub-tick-stale guarantee); a 2-hop trip simply takes more sub-ticks.

## 3. Important functions and structures

### `src/core/regions/worker.rs` — the gradient map (the one genuinely new piece)

Add, beside `border_neighbor_map_for_region` (the direct map, `worker.rs:~860`) and
modelled on `importable_remote_jobs_for_region` (`worker.rs:871`) for the discovery
plumbing — but using a **BFS gradient**, not bare `component_of`:

The worker has only links/topology/discovery — **no `World`/grid `Entity` cells** — so it
returns *links*, and `RegionState` converts them to road cells (as
`refresh_remote_exit_cells` already does):

```rust
/// Worker side: for each destination region T reachable+road-connected from `region_id`,
/// the local border links that cross to a STRICTLY-closer neighbour (dist_to_T[N] < dist_to_T[R]).
fn travel_destination_exits_for_region(
    topology: &[RegionNeighborLink],          // region graph → dist_to_T (BFS, §4)
    discovery: &CrossRegionDiscovery,         // component_of → reachability filter only
    region_id: RegionId,
    border_links: &[NetworkBorderLink],
    is_owned: impl Fn(RegionId) -> bool,
) -> HashMap<RegionId, Vec<ExitLink>>         // T → sorted { link, to_region }
```

Contract: deterministic (BFS over sorted edges; each `Vec<ExitLink>` sorted + deduped).
Includes a destination only when its road networks are connected (mover can walk the
whole way) **and** a strictly-closer owned neighbour exists.

### `ExitCandidate` — carry the crossing, not just a cell (`regions/mod.rs`)

`remote_exit_cells: HashMap<RegionId, Vec<Entity>>` (bare road cells) is **not enough**:
`drain_traveler_handoffs` must know the immediate next-hop neighbour and link, and a
border/corner road cell can carry multiple links. `RegionState::refresh_remote_exit_cells`
converts the worker's `ExitLink`s to:

```rust
pub struct ExitLink      { pub link: BorderLinkId, pub to_region: RegionId }   // worker output
pub struct ExitCandidate { pub cell: Entity, pub link: BorderLinkId, pub to_region: RegionId }
// world.remote_exit_cells: HashMap<RegionId, Vec<ExitCandidate>>   // keyed by destination T
```

`resolve_target`/`advance_to_exit` walk to `candidate.cell`; `drain` reads
`candidate.link`/`candidate.to_region` directly (no `exit_link_for(cell, region)`
re-derivation, which is ambiguous on multi-link cells).

### `src/core/components.rs`

- `ReturnHop`, `return_path` — **remove** (first stop depending on them, delete in a
  small follow-up to keep the diff readable). They encode stale route state.
- `TravelerHandoff` — keep as the crossing carrier; drop `return_path`. Make
  `entry_link: Option<BorderLinkId>`. For `Outbound`/`Return` it is `Some(sender-side
  link)` (the immediate crossing); for the new `Rollback` purpose it is `None`. `to_region`
  is the immediate next-hop neighbour (or, for `Rollback`, the home region directly).
- `TravelPurpose` — add a third variant `Rollback` (alongside `Outbound`/`Return`), the
  by-id un-strand delivered to home; the receiver handles it before any border placement.
- `PendingHandoff::Outbound` — keep; carries `{ traveler, token, to_region, exit_link }`
  (the immediate next-hop neighbour and its crossing link, from the chosen
  `ExitCandidate` — not a bare `exit_cell`, which is ambiguous on a multi-link road).
  Final workplace stays in `token.destination`.
- `PendingHandoff::Return` — replace the `return_path` field with `{ traveler, to_region,
  exit_link }` (the same immediate-crossing shape as Outbound).
- `PendingHandoff::Rollback { traveler }` — NEW, the never-strand escape hatch. When a
  transit/work region finds **no progressing+reachable exit toward home**, it emits this;
  the worker routes it **directly to `traveler.citizen.region()` by id** (not by border
  topology), and the home region calls `apply_traveler_return` to clear `Away` (a cosmetic
  teleport home — only happens when the road route home is severed mid-trip).
- `VisitingToken` — keep the container; add a small purpose tag (no new token type):

```rust
pub enum VisitingPurpose {
    Work,                                                   // at the final workplace region
    TransitOutbound { final_workplace: Entity, to_region: RegionId, exit_link: BorderLinkId },
    TransitReturn  { to_region: RegionId, exit_link: BorderLinkId },   // toward home
}
pub struct VisitingToken { pub token: TravelState, pub purpose: VisitingPurpose }
```

`TravelState.destination` is the **local** movement target while transiting (the exit
cell); `final_workplace` keeps the true workplace for `TransitOutbound`.

### `src/core/systems/travel.rs`

- `resolve_target` — keep using `remote_exit_cells[workplace.region()]`; with the map
  now multi-hop, local-citizen outbound needs no new logic beyond reading
  `ExitCandidate.cell`. **Fix `Target::BorderExit.to_region`**: today it is
  `workplace.region()` (the FINAL region); it must become the chosen
  `candidate.to_region` (immediate next hop) — otherwise multi-hop crossings route to
  the wrong region.
- `advance_to_exit` (`travel.rs:340`) — reuse; it already re-picks reachable candidates
  when they change.
- `depart_to_cell` (`travel.rs:516`, `(world, networks, origin, dest_cell) -> Option<Entity>`
  returning the entry road cell) / `advance_to_building` (`travel.rs:416`) — reuse for the
  **workday-end departure**: a final-region work visitor is parked off-road
  (`current_cell = None`) at the workplace, so the return must first depart the building
  to a reachable exit using the existing building→road departure (wrapping the returned
  entry cell into a `TravelState`), not a shortcut.
- `step_visiting_tokens` / `step_visiting` — extend by `VisitingPurpose`: Work (current
  behaviour) · TransitOutbound (walk to exit → `PendingHandoff::Outbound`) · TransitReturn
  (walk to exit → `PendingHandoff::Return`). Keep the P7b dwell gate (already applied to
  visiting tokens).
- `receive_traveler_return` — keep for the final home-region leg.
- `apply_traveler_return` — clears the `Away` mark **in the home region only** (it mutates
  the home `world.travel` entry; it is a no-op in a transit/work region, which has no
  local entry for that citizen). So it is the fallback *at home*, not everywhere — see the
  `Rollback` path below for the non-home failure case.

### `src/core/regions/mod.rs`

- `receive_traveler_handoff` — branch by whether `self.id` is the final target:
  - Outbound, `self == workplace.region()` → create a Work visitor.
  - Outbound, transit → create a `TransitOutbound` token toward the current
    gradient exit.
  - Return, `self == home region` → `receive_traveler_return`.
  - Return, transit → create a `TransitReturn` token toward the current gradient exit.
- `drain_traveler_handoffs` — stop reading/writing `return_path`; for both kinds, take
  `ExitCandidate.link`/`to_region` and emit the immediate `TravelerHandoff`.
- `cell_at_border_link`, `exit_link_for`, `border_road_links`, `refresh_remote_exit_cells`
  — reuse (the last now stores `ExitCandidate`s).

### `src/core/regions/runtime/mod.rs`, `src/core/regional_game_runner.rs`

- Reuse `RegionEvent::ReceiveTraveler` / `StepTravel`, `drained_traveler_handoff_messages`,
  and `step_travel_city` unchanged.

## 4. Pseudocode / integration

### Worker builds the gradient destination-exit map

```rust
for runtime in regions {
    runtime.set_border_neighbor_map(border_neighbor_map_for_region(...)); // direct, unchanged
    let exit_links = travel_destination_exits_for_region(
        directory.topology(),               // RegionNeighborLink edges → dist_to_T (§ below)
        &directory.discovery_snapshot(),    // component_of → reachability filter only
        runtime.region_id(),
        &runtime.state().network_border_links(),
        |region| owners.owner_of(region).is_some(),
    );                                       // T → Vec<ExitLink { link, to_region }>
    runtime.set_travel_destination_exits(exit_links);  // RegionState resolves links → cells
}
// For A-B-C: A maps C → A/B links (next hop B, dist B<dist A); B maps C → B/C links;
//            C maps A → C/B links; B maps A → B/A links. Never the backward link.
```

### Next-hop selection (the gradient, shared by outbound and return)

```rust
// Worker side, once per pass, deterministically (returns LINKS — no grid access here):
fn progressing_exit_links(topology, region R, target T, border_links, is_owned)
    -> Vec<ExitLink>
{
    // dist_to_T[X] = hops from X to T. RegionNeighborLink is DIRECTED, so BFS from T over
    // REVERSE edges (edges whose `neighbor == frontier`); a mirrored layout makes the two
    // coincide, but reverse-edge BFS is correct regardless.
    let dist_to_t = bfs_reverse(topology, T);
    let mut out = Vec::new();
    for link in border_links.where(network.region == R) {
        let n = neighbor_across(link);                       // an outgoing R -> N edge
        if is_owned(n)
           && dist_to_t.get(n).zip(dist_to_t.get(R)).is_some_and(|(dn, dr)| dn < dr) // STRICT progress
           && road_connected(discovery, link, T) {           // reachability filter
            out.push(ExitLink { link: link.id, to_region: n });
        }
    }
    out.sort_by_key(|e| (e.to_region.0, e.link.0));           // determinism
    out.dedup();
    out
}
// RegionState::refresh_remote_exit_cells then maps each ExitLink → ExitCandidate by
// resolving link → road cell(s) via cell_at_border_link / border_road_links (sorted).
```

### Outbound local citizen (unchanged shape, multi-hop via the map)

```rust
ScheduleIntent::Work(workplace) if workplace.as_local(region).is_none() => {
    match world.remote_exit_cells.get(&workplace.region()) {     // now multi-hop
        Some(exits) if !exits.is_empty() => Target::BorderExit {
            candidates: exits,        // ExitCandidate { cell, link, to_region }
            workplace,                // final workplace (stays in token.destination)
            // to_region is the chosen candidate.to_region (immediate next hop),
            // NOT workplace.region().
        },
        _ => Target::Building(home),  // truly unreachable → idle (unchanged)
    }
}
```

### Receive (outbound or return) — same branch shape

```rust
fn receive_traveler_handoff(h) -> Vec<TravelerHandoff> {
    // Rollback arrives at home by id — clear Away before any border work, never strand.
    if h.purpose == Rollback { travel::apply_traveler_return(world, h.traveler); return Vec::new(); }

    let target = match h.purpose {
        Outbound => h.token.destination?.region(),       // final workplace region
        Return   => h.traveler.citizen.region(),         // home region
        Rollback => unreachable!(),                       // handled by the top guard
    };
    // Place at the local entry cell (sender-side link → local). If the entry road
    // vanished since the sender emitted (one-sub-tick stale), DO NOT drop — roll home.
    let Some(entry_cell) = h.entry_link.and_then(|l| cell_at_border_link(l.matching_neighbor_link())) else {
        return rollback_or_clear(self.id, target, h.traveler);   // home → apply_traveler_return; else Rollback
    };
    if self.id == target {
        match h.purpose {
            Outbound => travel::receive_traveler_work(world, h.traveler, entry_cell, h.token.destination?),
            Return   => travel::receive_traveler_return(world, h.traveler, entry_cell),
        }
        return Vec::new();
    }
    // transit: pick the current gradient exit toward `target`, create a transit token
    let Some(cand) = current_gradient_exit_toward(target, entry_cell) else {
        // no progressing+reachable exit → never strand: roll the citizen home by id.
        world.outgoing_handoffs.push(PendingHandoff::Rollback { traveler: h.traveler });
        return Vec::new();
    };
    travel::receive_transit_traveler(world, h.traveler, entry_cell, cand.cell, match h.purpose {
        Rollback => unreachable!(),   // handled at the top
        Outbound => VisitingPurpose::TransitOutbound { final_workplace: h.token.destination?,
                                                       to_region: cand.to_region, exit_link: cand.link },
        Return   => VisitingPurpose::TransitReturn  { to_region: cand.to_region, exit_link: cand.link },
    });
    Vec::new()
}

// Never-strand helper: at home clear Away locally; elsewhere route a Rollback to home by id.
fn rollback_or_clear(self_id, target, traveler) -> Vec<TravelerHandoff> {
    if self_id == traveler.citizen.region() { travel::apply_traveler_return(world, traveler); }
    else { world.outgoing_handoffs.push(PendingHandoff::Rollback { traveler }); }
    Vec::new()
}
```

### Transit token reaches its border exit (step_visiting)

```rust
if token.current_cell == token.destination {                 // arrived at the exit cell
    let pending = match purpose {
        TransitOutbound { final_workplace, to_region, exit_link } => PendingHandoff::Outbound {
            traveler, token: travelling(token.current_cell, final_workplace), to_region, exit_link },
        TransitReturn { to_region, exit_link } => PendingHandoff::Return { traveler, to_region, exit_link },
    };
    world.outgoing_handoffs.push(pending);
    remove visiting token;
}
```

### Workday end in the final region (parked departure reused)

```rust
if purpose == Work && schedule_phase(hour) != Work {
    let target = traveler.citizen.region();                   // home
    let Some(cand) = current_gradient_exit_toward(target, work_building_cell) else {
        world.outgoing_handoffs.push(PendingHandoff::Rollback { traveler });  // no road home
        remove visiting token; return;
    };
    // depart the parked off-road visitor via the existing building→road path; the entry
    // road may itself be unreachable now → roll home rather than strand:
    let Some(entry) = depart_to_cell(world, networks, work_building, cand.cell) else {
        world.outgoing_handoffs.push(PendingHandoff::Rollback { traveler });
        remove visiting token; return;
    };
    visiting.token = travelling(entry, cand.cell);   // walk entry → exit cell
    visiting.purpose = VisitingPurpose::TransitReturn { to_region: cand.to_region, exit_link: cand.link };
}
```

### Drain handoffs (no return_path)

```rust
match pending {
    Outbound { traveler, token, to_region, exit_link }
        => TravelerHandoff { token, traveler, to_region, entry_link: Some(exit_link), purpose: Outbound },
    Return  { traveler, to_region, exit_link }
        => TravelerHandoff { token: TravelState::default(), traveler, to_region,
                             entry_link: Some(exit_link), purpose: Return },
    Rollback { traveler }                 // route to home BY ID; clears Away on arrival
        => TravelerHandoff { token: TravelState::default(), traveler,
                             to_region: traveler.citizen.region(), entry_link: None,
                             purpose: Rollback },
}
// Outbound/Return entry_link is Some(sender-side link), carried straight from the
// ExitCandidate — no ambiguous exit_link_for(cell, region) re-derivation on a multi-link
// road cell. Rollback's to_region is the home region directly (worker routes it by id);
// its receive handles apply_traveler_return before any border placement.
```

### entry_link convention (unified, sender-side)

Today Outbound receive uses `entry_link.matching_neighbor_link()` (sender-side → local)
while Return receive uses `handoff.entry_link` directly (home-side, because
`return_path` stored a home-side link). Removing `return_path` makes `entry_link`
**`Some(sender-side link)` for both `Outbound` and `Return`** (both receivers call
`matching_neighbor_link()`), and **`None` only for `Rollback`** (which clears `Away` at
home without any border placement). The existing direct-return tests must be re-baselined
to this convention.

## 5. Tests

`src/core/regions/worker.rs`
- `travel_exits_map_multihop_destination_to_first_hop` — topology A→B→C: A maps C to its
  A/B links (next hop B); B maps A to its B/A links.
- `travel_exits_pick_only_strictly_closer_neighbour` — **loop-safety**: in A–B–C, B's
  map for destination C does NOT include the B/A link (dist A to C is not < dist B to C),
  even though A,B,C share one road component. The regression for the old
  component-membership bug.

`src/core/regions/mod.rs`
- `receive_outbound_for_remote_workplace_creates_transit_token` — B receives outbound
  for a C workplace → a `TransitOutbound` token toward a B/C exit.
- `transit_outbound_forwards_without_return_path` — B transit reaches its B/C exit →
  drain emits `TravelerHandoff::Outbound` to C; assert no `return_path`.
- `workday_end_creates_dynamic_return_to_home_region` — parked Work visitor in C, work
  phase ends → C departs the building and creates a `TransitReturn` toward A.
- `transit_return_routes_by_current_topology` — B receives Return from C for home A →
  picks the current B/A exit and forwards Return to A.
- `return_replans_after_exit_road_removed` — B has two progressing exits toward A; remove
  the preferred road → return takes the remaining reachable one.
- `severed_route_home_rolls_back_not_strands` — a transit/work region with NO progressing
  exit toward home emits `PendingHandoff::Rollback`; the home region clears `Away` via
  `apply_traveler_return` (citizen ends home, never stuck `Away`).
- `receive_return_starts_homebound_walk` — keep the existing final-leg test (border→home
  still animates).

`src/core/regional_game_runner.rs` / `runtime/mod.rs`
- `multi_region_return_handoff_arrives_next_subtick` — the StepTravel barrier still gives
  one-sub-tick staleness across a 2-hop trip.

No UI test needed: transit tokens stay in `world.visiting_travel`, so dots render through
the existing adapter path.

## 6. Risks / non-goals

- **Loop-safety is the load-bearing invariant**: next-hop must STRICTLY decrease
  `dist(·, target)` on the topology graph. `component_of` is only the reachability
  filter, never the routing rule. The `travel_exits_pick_only_strictly_closer_neighbour`
  test guards this.
- Determinism: BFS over sorted topology edges; every `Vec<ExitCandidate>` sorted +
  deduped; deterministic tie-breaks. Cross-region remains one-sub-tick-stale, never
  non-deterministic.
- Do not expose `World`/topology to the UI; no new worker command; no new production
  dependency; do not store full routes in traveller state.
- Do not rewrite away tokens on road/topology change — dynamic routing re-plans each
  step. A token with no progressing+reachable exit emits `PendingHandoff::Rollback`,
  routed to the home region **by id** (a small worker addition: deliver to
  `owners.owner_of(home)` directly, not via border topology), where
  `apply_traveler_return` clears `Away`. `apply_traveler_return` is home-region-only and
  must never be relied on to un-strand from a transit/work region.
- Remove `return_path` in a small follow-up if deleting it in the same patch is noisy;
  first make behaviour stop depending on it.

## Suggested patch split

- **P-a (core map):** `ExitCandidate` + `travel_destination_exits_for_region` (BFS
  gradient) + `refresh_remote_exit_cells` storing candidates + worker wiring +
  worker tests (incl. the loop-safety regression). No movement change yet.
- **P-b (movement):** `VisitingPurpose`, transit branches in `receive_traveler_handoff` /
  `step_visiting` / drain, `Target::BorderExit.to_region` fix, parked-departure reuse,
  `entry_link` unification; re-baseline direct-return tests + the multi-hop tests.
- **P-c (cleanup):** delete `return_path` / `ReturnHop` and their construction sites.
