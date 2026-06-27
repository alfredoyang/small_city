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

### One rule, both directions — a distance gradient on the ROAD-CONNECTED region graph

This is the unweighted v1 of `traffic-pathfinding-plan.md` §5f's **Layer 1** ("which
regions to cross?"). Layer 2 (the road path *within* each region) already exists —
`road_predecessors` (P1) + `route_cache` (P2) — and we reuse it untouched.

**Use the road-connectivity graph, not region adjacency.** Two regions can share a map
border with **no road crossing it**, so the routing graph must be the one §5f's Layer-1
Dijkstra uses: regions linked where a `NetworkBorderLink` pair (a road) actually crosses,
i.e. the directory's `CrossRegionDiscovery.road_crossings` (§3). Routing on raw
`RegionNeighborLink` *adjacency* would compute distances along roadless borders and
dead-end (see the failure diagram).

```text
  Region adjacency (RegionNeighborLink)        Road connectivity (NetworkBorderLink pairs)
  ─────────────────────────────────────        ───────────────────────────────────────────
  A ── B ── C   (all share borders)            A ══ B ── C    A-B road crosses; B-C border
                                                              has NO road
                                               → the routing graph has edge A-B only;
                                                 C is NOT reachable from B by road
```

Build a **region-level road graph** `G`: node = region; edge `R—N` iff some local
`NetworkBorderLink` of `R` pairs with one of `N` (a road crosses that border). Then for a
mover in region `R` heading to target region `T`:

```text
  dist_to_T[X] = BFS hops from X to T over G   (reverse-BFS seeded at T)
  next hop     = an edge R—N in G with  dist_to_T[N] < dist_to_T[R]   (STRICT progress)
  exit cands   = local road cells whose border link crosses to such an N
  ties (several N at the shorter distance) → deterministic (by RegionId, then exit pos)
```

Because `G` already contains only road-connected edges, there is **no separate
reachability filter** — connectivity is the graph. Strict monotone decrease of
`dist_to_T` guarantees termination (no A→B→A loop). Outbound `T = workplace.region()`;
return `T = traveler.citizen.region()`. **Same machinery, opposite target.**

```text
Distance field for target T = C  (numbers = dist_to_T over the ROAD graph G):

        ┌─────┐   road   ┌─────┐   road   ┌─────┐
        │  A  │══════════│  B  │══════════│  C  │
        │  2  │ ───────► │  1  │ ───────► │  0  │ = T
        └─────┘  descend └─────┘  descend └─────┘
   every step strictly decreases dist_to_T → always reaches C, never loops.
   Return simply re-seeds the field at T = A (so C=2, B=1, A=0) and descends the other way.
```

Why it must be the road graph, not adjacency — the dead-end:

```text
   A ── B ·· C        '··' = shared border but NO road (roadless)
   │         │        a SEPARATE road loops A—D—C
   D ════════┘
  Adjacency BFS:  dist(B,C)=1 via B··C → B picks the roadless B/C border → DEAD END.
  Road-graph BFS: B··C is not an edge; the only road path is A—D—C, so B is not even
                  on a progressing route; A descends A(2)→D(1)→C(0) correctly.
```

### The two layers (this plan vs. §5f)

```text
  LAYER 1  "which regions to cross?"            LAYER 2  "which road cells in this region?"
  ─────────────────────────────────            ──────────────────────────────────────────
  graph  = road-connected region graph G       graph  = road cells (one region's World)
  weight = 1 per hop   (THIS PLAN, v1)          weight = step_cost / crossing penalty (P7)
         = road cost   (§5f, deferred)          algo   = Dijkstra → came_from tree
  algo   = BFS distance field, descend          status = ALREADY EXISTS — route_cache (P2)
  output = next-hop exit toward T               output = walk entry → exit (or → workplace)
  runs   = worker (discovery snapshot)          runs   = each region, share-nothing

        A ─Layer1─► B ─Layer1─► C          (pick the region corridor by descending dist_to_T)
        │           │           │
      Layer2      Layer2      Layer2        (walk the road path inside each region)
   (home→exit) (entry→exit) (entry→work)

  Neither layer ever crosses a region boundary — the token message (handoff) does.
  THIS PLAN = §5f with Layer 1 unweighted (hop count). Swapping BFS→weighted Dijkstra
  on the SAME graph G (border_crossing_cost) is §5f's deferred quality upgrade; nothing
  else changes.
```

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

**Directory change first (small, required).** `CrossRegionDiscovery` today keeps only
`components: Vec<Vec<RegionRoadNetworkId>>` (`directory.rs:38`) — `build_component_graph`
(`directory.rs:229`) unions matched border links via `BorderLinkIndex` and then
**discards the crossing edges**. A component `{A_net, B_net, C_net}` cannot tell whether
A crosses *directly* to C, nor which local `BorderLinkId` reaches B vs C — so `G` is not
derivable from `components` alone. Preserve the edges the union loop already has
(`directory.rs:244`, where `left` is a `NetworkBorderLink` of `R` and the matched
`topology` neighbour is `N`):

```rust
pub struct RoadCrossing { pub region: RegionId, pub link: BorderLinkId, pub neighbour: RegionId }
// CrossRegionDiscovery gains: pub road_crossings: Vec<RoadCrossing>   (sorted, deterministic)
// components stays as-is for existing resource reachability.
```

The travel helper then BFS-distances over the **road-connected region graph** `G` built
from `road_crossings` (node = region; edge `R—N` for each crossing), *not* raw
`RegionNeighborLink` adjacency (§2). This is §5f's Layer 1, unweighted.

The worker has only links/topology/discovery — **no `World`/grid `Entity` cells** — so it
returns *links*, and `RegionState` converts them to road cells (as
`refresh_remote_exit_cells` already does):

```rust
/// Worker side: for each destination region T road-connected from `region_id`, the local
/// border links crossing to a STRICTLY-closer neighbour on the ROAD graph G
/// (dist_to_T[N] < dist_to_T[R]).
fn travel_destination_exits_for_region(
    discovery: &CrossRegionDiscovery,         // .road_crossings → road graph G (edges + dist_to_T)
    region_id: RegionId,
    border_links: &[NetworkBorderLink],
    is_owned: impl Fn(RegionId) -> bool,
) -> HashMap<RegionId, Vec<ExitLink>>         // T → sorted { link, to_region }
```

Contract: deterministic (BFS over sorted road-graph edges; each `Vec<ExitLink>` sorted +
deduped). A destination appears only when a strictly-closer owned neighbour exists on `G`
— connectivity is the graph, so there is no separate reachability filter.

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

Token lifecycle across a 2-hop commute (A home, B transit, C work):

```text
  local citizen (A.world.travel)                 visiting token (X.world.visiting_travel)
  ──────────────────────────────                 ─────────────────────────────────────────
  Target::BorderExit → walk to A/B exit
        │ Cross (mark Away, PendingHandoff::Outbound)
        ▼ handoff to B
                                          B: VisitingPurpose::TransitOutbound → walk to B/C exit
                                                │ reached exit → PendingHandoff::Outbound
                                                ▼ handoff to C
                                          C: VisitingPurpose::Work → walk to workplace, park
                                                │ workday ends → depart building
                                                ▼ VisitingPurpose::TransitReturn → walk to C/B exit
                                                │ reached exit → PendingHandoff::Return
                                                ▼ handoff to B
                                          B: VisitingPurpose::TransitReturn → walk to B/A exit
                                                │ reached exit → PendingHandoff::Return
        ┌───────────────────────────────────────┘ handoff to A
        ▼ A is home: receive_traveler_return → walk border → home, clear Away
  (any region, no progressing exit / stale entry → PendingHandoff::Rollback → home clears Away)
```

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
        &directory.discovery_snapshot(),    // .road_crossings → road graph G + dist_to_T (§ below)
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
fn progressing_exit_links(discovery, region R, target T, border_links, is_owned)
    -> Vec<ExitLink>
{
    // Build the ROAD-connected region graph G from discovery.road_crossings (each is a real
    // road crossing region—neighbour via a specific local link). Node = region; edge R—N
    // for each crossing. Roadless borders aren't crossings → not edges → no dead-end.
    let g = region_graph_from(&discovery.road_crossings);   // edges R—N
    let dist_to_t = bfs(&g, T);                              // hops to T over G (unweighted; §5f weights later)
    let mut out = Vec::new();
    for c in discovery.road_crossings.where(region == R) {   // each gives the local link AND neighbour
        if is_owned(c.neighbour)
           && dist_to_t.get(c.neighbour).zip(dist_to_t.get(R)).is_some_and(|(dn, dr)| dn < dr) { // STRICT
            out.push(ExitLink { link: c.link, to_region: c.neighbour });   // connectivity is the graph
        }
    }
    out.sort_by_key(|e| (e.to_region.0, e.link.0));   // determinism
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
- `travel_exits_skip_roadless_border` — **graph correctness**: A and B share a map border
  with NO road crossing, but a road path A–D–C exists. A's map for C routes via D (not the
  roadless A/B border); the gradient never dead-ends on the roadless edge.

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

- **Relation to `traffic-pathfinding-plan.md` §5f.** This plan *is* §5f's multi-hop
  two-layer routing, with **Layer 1 unweighted** (BFS hop count) for v1; Layer 2 (the
  per-region road walk) already exists. The §5f road-cost weighting
  (`border_crossing_cost` / `border_route_hint`, BFS→Dijkstra on the *same* graph `G`)
  stays the deferred quality upgrade. §5f should be reconciled: it still assumes the
  `return_path` stack and adjacency — this plan replaces both (dynamic routing on the
  road graph). Cross-link the two when this lands.
- **Loop-safety is the load-bearing invariant**: next-hop must STRICTLY decrease
  `dist_to_T` **on the road-connected graph `G`** (edges = real road crossings), so the
  gradient can never descend a roadless border or loop. Computing distances on raw
  `RegionNeighborLink` adjacency would dead-end; `G` makes connectivity intrinsic (no
  separate filter). The `travel_exits_pick_only_strictly_closer_neighbour` test guards
  the no-backward-hop case.
- Determinism: BFS over sorted `G` edges; every `Vec<ExitCandidate>` sorted + deduped;
  deterministic tie-breaks. Cross-region remains one-sub-tick-stale, never
  non-deterministic.
- Do not expose `World`/topology to the UI; **no new worker command/protocol**; no new
  production dependency; do not store full routes in traveller state. (The discovery
  snapshot gaining `road_crossings` is *data preserved from an existing computation*, not
  a new message or command.)
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
