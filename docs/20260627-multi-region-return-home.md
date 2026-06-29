# 20260627 Multi-region commuting — the Layer-1 routing registry

Status: **plan** (not implemented). Builds on P5 cross-region tokens + P7 sub-tick movement.

> **Scope (after the token refactor was split out).** This plan now covers **only the
> multi-hop routing layer** — the central L1 registry that makes `remote_exit_cells`
> multi-hop, and the per-region Dijkstra pricing that feeds it. The **token model, the one
> stepper, the move-not-convert handoff, and `Rollback`** moved to
> `docs/20260629-unify-travel-tokens.md` (the behaviour-preserving refactor) — a
> **prerequisite** that lands first. This plan assumes a single `TravelToken` whose stepper
> routes to a remote endpoint via `remote_exit_cells`; here we only change *what that map
> contains* (direct-neighbour → cost-routed multi-hop `RouteExit`s).

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

**Goal.** One symmetric, **loop-safe, dynamic** routing rule for both directions, on a
**single `TravelToken`** (local citizens and visitors unified — see the refactor) carried region-to-region
by the existing `PendingHandoff`/`TravelerHandoff` + `StepTravel` barrier. Remove
`return_path` / the two-token split. Stable facts the token already carries:

```text
traveler.citizen.region() = home region  (the return target)
token.destination         = final workplace (the outbound target)
directory region_routes   = the Layer-1 next-hop map (central snapshot, read each step)
current road graph        = the Layer-2 local path (per-region route cache)
```

### Scope — first remove the adjacency-only ROUTING, keep the crossing TRANSPORT

The current cross-region travel splits cleanly into two parts; this plan **replaces the
routing** and **keeps the transport**. Audit and excise the direct-neighbour-only routing
*before* layering on multi-hop, so there is one routing path, not two.

```text
  REMOVE (direct-neighbour-only ROUTING — "adjacency")     KEEP (token crossing TRANSPORT)
  ────────────────────────────────────────────────────     ─────────────────────────────────
  refresh_remote_exit_cells: remote_exit_cells keyed by     RegionEvent::ReceiveTraveler / StepTravel
    the DIRECT border_neighbor_map only (regions/mod.rs)     step_travel_city barrier (FIFO, 1-subtick stale)
  resolve_target: Target::BorderExit.to_region =            PendingHandoff / TravelerHandoff carriers
    workplace.region() (assumes direct neighbour)             (shape changes: drop return_path, add
  return_path / ReturnHop stack + drain/receive popping       exit_link / Rollback — flow unchanged)
    it (the stale stored route — components.rs, mod.rs)      drain_traveler_handoffs flow + OutboundMessage::
  border_neighbor_map as a ROUTING source (its only            TravelerHandedOff / drained_*_messages
    consumer was the direct exit map)                        cell_at_border_link / exit_link_for /
                                                               matching_neighbor_link / border_road_links
                                                             apply_traveler_return (clears the Away marker)
                                                             world.{outgoing_handoffs}, the Away marker
                                                               (token state now unified — §3)
```

So `remote_exit_cells` survives **as the field**, but its *meaning* and *producer*
change: from "direct neighbour → its border cells" (direct map) to "destination region →
cost-routed `RouteExit`s" (from `region_routes`). The token crossing event and all the
border-link plumbing are untouched — only *which region is next* is now answered by the
Layer-1 registry instead of the adjacency map / stored stack.

## 2. Proposal

### Two-layer routing — weighted Dijkstra on the road-cost region graph (full §5f)

This implements `traffic-pathfinding-plan.md` §5f **in full**: **Layer 1** = Dijkstra on a
**road-cost-weighted region graph** ("which regions to cross, by lowest road cost?");
**Layer 2** = the per-region road Dijkstra that already exists (`road_predecessors` P1 +
`route_cache` P2). Neither layer ever crosses a region boundary — the token handoff does.

```text
  LAYER 1  "which regions to cross?"            LAYER 2  "which road cells in this region?"
  ─────────────────────────────────            ──────────────────────────────────────────
  graph  = road-cost region graph               graph  = road cells (one region's World)
  weight = per-region road-crossing cost        weight = step_cost / crossing penalty (P7)
           (each region's own Dijkstra)         algo   = Dijkstra → came_from tree
  algo   = Dijkstra (cost to each region T)     status = ALREADY EXISTS — route_cache (P2)
  output = next-hop exit toward T (min cost)    output = walk entry → exit (or → workplace)
  runs   = directory (from PUBLISHED costs)     runs   = each region, share-nothing

        A ─Layer1─► B ─Layer1─► C          (pick the region corridor by lowest road cost)
        │           │           │
      Layer2      Layer2      Layer2        (Dijkstra the road path inside each region)
   (home→exit) (entry→exit) (entry→work)
```

**The cost comes from the regions, not the worker (share-nothing).** Each region owns its
roads; only it can price a crossing. So:

1. **Each region runs its own road Dijkstra** (reusing `route_cache`/`road_predecessors`)
   to price every *border-entry → border-exit* traversal of itself (its `crossing_costs`),
   and knows which neighbour each border link faces (its `border_links`).
2. **Each region publishes** that `RegionRoadReport` up to `CrossRegionDiscovery`, exactly
   as it already publishes `availability_hints` (spare resources).
3. **The directory runs Layer-1 Dijkstra** over the assembled weighted region road graph
   (intra-region edges = published `crossing_costs`; inter-region edges = matched
   `border_links` crossings) → `region_routes`. **Destination-keyed**: one Dijkstra seeded
   at each destination `T` fills a whole `RouteField` — every region's min-cost next-hop
   exit toward `T` — in a single run (`region_routes.to[T].from[R]`).
4. **A transiting token in region `R` reads `region_routes.to[T].from[R]`** for its next
   hop toward its destination `T`, runs **Layer-2 Dijkstra** locally to that exit cell, then
   crosses via the existing token handoff event.

> This corrects §5f, which had the *worker* precompute the crossing cost — but the worker
> has no `World`/road graph. Pricing is a per-region (share-nothing) job; the directory
> only assembles the published `RegionRoadReport`s.

**Use the road-connectivity graph, not region adjacency.** Two regions can share a map
border with **no road crossing it**, so its edges are the regions' published
`border_links` (a road actually crosses), never raw `RegionNeighborLink` adjacency:

```text
  Region adjacency (RegionNeighborLink)        Road connectivity (NetworkBorderLink pairs)
  ─────────────────────────────────────        ───────────────────────────────────────────
  A ── B ── C   (all share borders)            A ══ B ── C    A-B road crosses; B-C border
                                                              has NO road
                                               → the graph has edge A-B only; C is NOT reachable
                                                 from B by road
```

**Layer-1 is a cost-distance gradient** — for a mover in region `R` heading to target `T`,
`cost_to_T[X]` = min road cost from `X` to `T` over the region road graph (Dijkstra seeded at `T`); the next
hop is an edge `R—N` with `cost_to_T[N] < cost_to_T[R]`. Strict decrease of a Dijkstra
distance guarantees termination (**no A→B→A loop**), and it being road-connected means a
roadless border is never an edge (**no dead-end**). Outbound `T = workplace.region()`,
return `T = traveler.citizen.region()` — **same machinery, opposite target.**

```text
Cost field for target T = C  (numbers = cost_to_T over the region road graph; edge labels = road-crossing cost):

   ┌─────┐  cost 4  ┌─────┐  cost 3  ┌─────┐
   │  A  │══════════│  B  │══════════│  C  │
   │  7  │ ───────► │  3  │ ───────► │  0  │ = T
   └─────┘  descend └─────┘  descend └─────┘
  every step strictly decreases cost_to_T → always reaches C, never loops.
  Return re-seeds the field at T = A and descends the other way.
```

Lowest road COST, not fewest hops — a fast 3-hop corridor can beat a slow 2-hop one:

```text
   A ══ B ══ C     2 hops, but B is a congested grid: crossing cost 15 → total 18
   ║         ║
   D ══ E ══ ╝     3 hops on motorways: 4 + 5 + 4 = 13  → Dijkstra picks A-D-E-C
  (Layer-1 edge weights ARE Layer-2 Dijkstra distances, so the two share one cost model.)
```

And why it must be the road graph — the roadless dead-end:

```text
   A ── B ·· C        '··' = shared border but NO road (roadless)
   │         │        a SEPARATE road loops A—D—C
   D ════════┘
  Adjacency:   B··C looks like a 1-hop edge → B picks the roadless B/C border → DEAD END.
  Road graph:  B··C is not an edge; the only road path is A—D—C, so A descends its
               cost field A→D→C correctly and B is simply not on a progressing route.
```

### Multi-region flow (A → B → C, then home)

```text
Home A                   Transit B                  Work C
------                   ---------                  ------
phase=Work → target=C; next hop=B
walk A→A/B exit
MOVE token ────────────> target=C; next hop=C
                         walk B/A → B/C exit
                         MOVE token ─────────────> target=C reached: walk to workplace, idle
                                                    phase flips → Home: target=A
                         MOVE token <───────────── next hop=B (the SAME stepper, opposite endpoint)
                         target=A; next hop=A
                         walk B/C → B/A exit
MOVE token <──────────── home region: walk border → home, idle
```

### Road / topology change behaviour

```text
Stored return_path:  fixed C→B→A; a link change strands the token unless every token
                     is rewritten.
Gradient (this plan): the directory precomputes region_routes once per change
                     (rebuild_discovery); each ReceiveTraveler/StepTravel READS the
                     current snapshot's next-hop (no per-step recompute). Road removed →
                     directory rebuilds the routes AND the local exit re-pick
                     (advance_to_exit) chooses another reachable exit; topology changed →
                     the snapshot's gradient points elsewhere. If no progressing exit
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

### `src/core/regions/directory.rs` — the central Layer-1 routing registry

The region routing map is **cross-region state**, so it lives where the other
cross-region collectors live: the coordinator-owned `CrossRegionDiscovery` snapshot
(`directory.rs:38`), built once per change in `rebuild_discovery` (`directory.rs:160`)
and read lock-free via `discovery_snapshot()`. This is the **cross-region sibling of
`resource_registry.rs`** — `resource_registry` is region-*local* (per-`World`); the
directory snapshot is the central one (it already holds `components` for resource
reachability + `availability_hints` for every region's spare capacity). Layer-1 routing
belongs in the same place, built at the same chokepoint, snapshotted to all readers —
the build-once-cache-until-change pattern, lifted to the cross-region layer. Workers/
regions **read** it; nobody recomputes a region graph per pass.

Exactly two kinds of structure, with one direction of flow — **INPUT** each region
publishes, and **OUTPUT** the directory computes from all inputs and stores in the
snapshot. Nothing is shared both ways; the only overlap value (a region id) is named for
its role on each side.

```text
  per region (share-nothing)            CrossRegionDiscovery (central snapshot)
  ────────────────────────────          ───────────────────────────────────────────────
  RegionRoadReport            ──publish──►  reports: Vec<RegionRoadReport>   (INPUT, raw)
   ├ border_links: Vec<BorderLink>           │
   │   {link, neighbour}                      │ build_region_routes (in rebuild_discovery):
   └ crossing_costs: Vec<CrossCost>           │   1. assemble the region road graph from every report's
       {entry, exit, cost}                    │      border_links (edges) + crossing_costs (weights)
                                              │   2. Dijkstra per destination T
                                              ▼
                                         region_routes: RegionRoutes        (OUTPUT, answer)
                                           to[T]: RouteField
                                             from[R]: RouteHop { exits, cost }

  token in R heading to T ──read──► region_routes.to[T].from[R].exits   (one lookup)
```

**INPUT — what a region knows about itself** (computed in its own `World`, published on
the existing `availability_hints` path):

```rust
pub struct RegionRoadReport {                  // one per region, published each change
    pub region: RegionId,
    pub border_links: Vec<BorderLink>,         // an edge of the region road graph: my border link → the neighbour it reaches
    pub crossing_costs: Vec<CrossCost>,        // an edge weight: my own Dijkstra, border entry → border exit
}
pub struct BorderLink { pub link: BorderLinkId, pub neighbour: RegionId }
pub struct CrossCost  { pub entry: BorderLinkId, pub exit: BorderLinkId, pub cost: u32 }
```

**OUTPUT — what the directory computes** (one Layer-1 Dijkstra per destination):

```rust
pub struct CrossRegionDiscovery {
    pub components: Vec<Vec<RegionRoadNetworkId>>,         // existing — resource reachability
    pub availability_hints: Vec<RegionalAvailabilityHint>,// existing — spare resources
    pub region_routes: RegionRoutes,                      // NEW — the Layer-1 route table
}

pub struct RegionRoutes {                      // outer key = DESTINATION region
    to: HashMap<RegionId /*T*/, RouteField>,   // region_routes.to[T] = the field toward T
}
pub struct RouteField {                        // one Dijkstra-at-T tree: every source's answer
    from: HashMap<RegionId /*R*/, RouteHop>,   // region_routes.to[T].from[R]
}
pub struct RouteHop {                          // R's answer for "how do I get toward T?"
    pub exits: Vec<ExitLink>,                  // min-cost next-hop crossing link(s), sorted
    pub cost: u32,                             // R's total road cost to T
}
pub struct ExitLink { pub link: BorderLinkId, pub to_region: RegionId }  // the next crossing

impl RegionRoutes {
    // for region R: each reachable destination T → R's next-hop exits toward T.
    fn exits_from(&self, r: RegionId) -> HashMap<RegionId, Vec<ExitLink>> {
        self.to.iter()
            .filter_map(|(t, field)| field.from.get(&r).map(|hop| (*t, hop.exits.clone())))
            .collect()
    }
}
```

The edges of the region road graph come from the regions' `border_links` (not from `components`, which
`build_component_graph` builds by unioning and then *discarding* the edges). `RegionRoadReport`
is the travel analogue of `RegionalAvailabilityHint` (per-region published input);
`RegionRoutes` is the travel analogue of `components` (directory-computed output) and is the
destination-keyed form of §5f's `border_route_hint`. Determinism: Dijkstra over sorted
`border_links` with cost tie-breaks (cost, then `RegionId`, then `BorderLinkId`); every
`Vec<ExitLink>` sorted + deduped. Connectivity is intrinsic to the graph — no reachability filter.

### `src/core/regions/worker.rs` / `RegionState` — publish costs, read routes

- **Publish (step 1–2):** `RegionState::road_report()` builds its `RegionRoadReport` (the
  per-region Dijkstra, §4) and the worker forwards it to the directory alongside the
  existing availability publish — *the worker carries the report, the region computes it*
  (the worker has no `World`/road graph).
- **Read (step 3):** the worker reads `discovery.region_routes.exits_from(R)` (a lookup, no
  graph search) and hands `RegionState` the `ExitLink`s;
  `RegionState::refresh_remote_exit_cells` resolves each `ExitLink` → road cell(s) as today.

```rust
let exit_links: HashMap<RegionId, Vec<ExitLink>> =
    discovery.region_routes.exits_from(R);          // destination T → R's cost-sorted exits toward T
runtime.set_travel_destination_exits(exit_links);   // RegionState resolves links → cells
```

### Build, caching & locking (how `region_routes` is published)

`region_routes` is built and published exactly like the directory's existing central state
(`components`, `availability_hints`) — see the decision record
`docs/20260628-l1-map-build-locking.md` for the full analysis. In short:

```text
  publish_state (Mutex)   — held across update + WHOLE-map rebuild   (the build)
  active_snapshot (Mutex) — held only to STORE the new Arc           (the swap, brief)
                          — routing reads take it only to Arc::clone (brief, never blocks the build)
```

- The worker whose region's `RegionRoadReport` changed calls `publish_region`, which
  **rebuilds the whole `region_routes`** (a full Dijkstra-per-destination recompute from
  all reports — not an incremental patch), under `publish_state`. Same flow as
  `build_component_graph` today; idempotent (`directory.rs:150`): an unchanged report → no
  rebuild.
- It then swaps the new `Arc<CrossRegionDiscovery>` into `active_snapshot` (a brief write).
  Routing reads (`discovery_snapshot()`, every sub-tick) just `Arc::clone` it — **never
  blocked by the rebuild**.
- The **heavy** road-cell Dijkstra is the per-region `road_report()` (each region's own
  `World`, **off the directory lock**, share-nothing). The directory runs only the **small
  region-level** Dijkstra (nodes = `(region, border-link)`), so the rebuild is bounded by
  region count and only happens on a road-graph change.

> **v1: keep the build under `publish_state`** (correct + simplest; small + rare). It holds
> the write lock only against other *publishers*, never readers. **Do not** shrink to a
> naive lock-only-swap — it loses a concurrent publisher's update. If profiling later shows
> publisher contention, move the rebuild to a single owner / a generation-CAS swap (the two
> escape hatches in the decision record).

### `RouteExit` — "to reach region T, leave HERE through this exit" (`regions/mod.rs`)

A **`RouteExit`** is one region's local answer to *"a token wants to reach final region T —
where does it leave me, and what's the next region?"* It is the cell-resolved form of a
Layer-1 next-hop: **walk to `cell`, which crosses via `link` into next-hop `to_region`.**
Every region on the route except the final one holds one (or a few, cost-ordered) per
destination; the token follows a *chain* of them, one per region.

```text
  Token's final destination = C.   Each region looks up ITS OWN RouteExit toward C:

   Region A                     Region B                     Region C
   ┌───────────────┐            ┌───────────────┐            ┌───────────────┐
   │ remote_exit_  │            │ remote_exit_  │            │  (final — no   │
   │  cells[C] =    │            │  cells[C] =    │            │   RouteExit;   │
   │  RouteExit{    │  AB_link   │  RouteExit{    │  BC_link   │   walk to the  │
   │   cell ───────────┐         │   cell ───────────┐         │   workplace)   │
   │   link: AB_link│  │ cross   │   link: BC_link│  │ cross   │                │
   │   to_region: B │  ▼ to B    │   to_region: C │  ▼ to C    │                │
   └───────────────┘            └───────────────┘            └───────────────┘
        │ Layer-2 walk to cell        │ Layer-2 walk to cell
        └──── handoff ────────────────┘──── handoff ───────────►

   keyed by FINAL destination (C) · `to_region` is the IMMEDIATE next hop (A→B, B→C)
```

It exists because the bare `HashMap<RegionId, Vec<Entity>>` (cells only) is **not enough**:
`drain_traveler_handoffs` needs the immediate next-hop neighbour *and* link, and a
border/corner road cell can carry multiple links — so `exit_link_for(cell, region)` would
be ambiguous. `RouteExit` keeps the `link` and `to_region` the Layer-1 router already chose,
so nothing is re-derived. `RegionState::refresh_remote_exit_cells` resolves the directory's
link-level `ExitLink`s into cell-level `RouteExit`s:

```rust
pub struct ExitLink  { pub link: BorderLinkId, pub to_region: RegionId }            // directory output (link-level)
pub struct RouteExit { pub cell: Entity, pub link: BorderLinkId, pub to_region: RegionId } // region-local (cell-resolved)
// world.remote_exit_cells: HashMap<RegionId /*final destination T*/, Vec<RouteExit>>
```

`resolve_target`/`advance_to_exit` walk to `route_exit.cell`; `drain` reads
`route_exit.link`/`route_exit.to_region` directly (no ambiguous `exit_link_for`).

### Token model & movement — see the refactor (prerequisite)

The single `TravelToken`, the one stepper, the move-not-convert handoff (`kind: {Move,
Rollback}`), the home-region front-end, and `Rollback` all live in
**`docs/20260629-unify-travel-tokens.md`** and land first. This plan assumes that stepper:
it routes a token toward a *remote* endpoint by walking to `remote_exit_cells[target.region]`
and emitting `PendingHandoff::Move`. **The only thing this plan changes is what
`remote_exit_cells` contains** — direct-neighbour cells become cost-routed multi-hop
`RouteExit`s, and `RouteExit.to_region` is the immediate *next hop* (not the final region).

### `src/core/regions/mod.rs` (routing wiring only)

- `refresh_remote_exit_cells` — instead of the direct `border_neighbor_map`, resolve the
  worker's `region_routes.exits_from(self.id)` (`ExitLink`s) into cell-level `RouteExit`s via
  `cell_at_border_link` / `border_road_links` (sorted). The stepper and handoff are unchanged.
- `drain_traveler_handoffs` — a `Move` already carries `RouteExit.{link, to_region}` from the
  stepper, so drain emits the `TravelerHandoff` directly (no `exit_link_for(cell, region)`
  re-derivation, which is ambiguous on a multi-link cell). Unchanged otherwise.

### `src/core/regions/runtime/mod.rs`, `src/core/regional_game_runner.rs`

- Reuse `RegionEvent::ReceiveTraveler` / `StepTravel`, `drained_traveler_handoff_messages`,
  and `step_travel_city` unchanged.

## 4. Pseudocode / integration

### Step 1 — each region prices its own crossings (Layer-2 Dijkstra, share-nothing)

```rust
// RegionState — reuses route_cache / road_predecessors (the SAME Dijkstra Layer 2 uses).
fn road_report(&self) -> RegionRoadReport {
    let links: Vec<BorderLinkId> = self.border_road_links();           // my border links (sorted)
    let mut crossing_costs = Vec::new();
    for entry in &links {
        let tree = self.world.routes_from(self.cell_at_border_link(*entry));  // one Dijkstra
        for exit in &links {
            if exit == entry { continue }
            if let Some(cost) = tree.cost_to(self.cell_at_border_link(*exit)) {
                crossing_costs.push(CrossCost { entry: *entry, exit: *exit, cost }); // cross me entry→exit
            }
        }
    }
    RegionRoadReport {
        region: self.id,
        border_links: self.border_link_neighbours(),   // Vec<BorderLink { link, neighbour }>
        crossing_costs,                                 // Vec<CrossCost>, sorted, deterministic
    }
}
// O(border_links) single-source Dijkstra runs — fine for small border counts. Recomputed
// when this region's roads change (same dirty signal as availability), then published.
```

### Step 3 — directory assembles the region road graph and runs Layer-1 Dijkstra (in `rebuild_discovery`)

```rust
// directory.rs rebuild_discovery — the single chokepoint, alongside build_component_graph.
fn build_region_routes(reports: &[RegionRoadReport], is_owned: impl Fn(RegionId)->bool)
    -> RegionRoutes
{
    // Region road graph: node = (region, border_link). Intra-region edge entry→exit =
    // the report's crossing_costs; inter-region edge = matched border_links pair (cost 0).
    // Only owned, road-connected edges are added.
    let g = weighted_region_graph(reports, &is_owned);
    let mut to = HashMap::new();
    for t in g.target_regions() {                       // ONE Dijkstra per DESTINATION T
        let cost_to_t = dijkstra(&g, t);                // min road cost from every node to T
        let mut from: HashMap<RegionId, RouteHop> = HashMap::new();
        for report in reports {
            for bl in &report.border_links {            // candidate crossing R=report.region → bl.neighbour
                if is_owned(bl.neighbour) && progresses(&cost_to_t, report.region, bl) {  // strictly lower
                    let hop = from.entry(report.region).or_default();
                    hop.exits.push(ExitLink { link: bl.link, to_region: bl.neighbour });
                    hop.cost = cost_to_t[&node(report.region)];   // R's cost to T
                }
            }
        }
        for hop in from.values_mut() {                  // cost-ordered, deterministic
            hop.exits.sort_by_key(|x| (cost_at(&cost_to_t, x), x.to_region.0, x.link.0));
            hop.exits.dedup();
        }
        to.insert(t, RouteField { from });              // the whole field toward T, one run
    }
    RegionRoutes { to }
}
// For destination C: to[C].from[A] → A/B link (min-cost corridor); to[C].from[B] → B/C.
// For destination A: to[A].from[C] → C/B; to[A].from[B] → B/A. A congested region is
// skipped for a cheaper detour (the motorway example in §2).
```

### Worker reads the registry (a lookup, no graph search)

```rust
let discovery = directory.discovery_snapshot();          // Arc<CrossRegionDiscovery>
for runtime in regions {
    runtime.set_travel_destination_exits(                // destination T → Vec<ExitLink>, precomputed
        discovery.region_routes.exits_from(runtime.region_id()),
    );                                                   // replaces the old direct border_neighbor_map map
}
// RegionState::refresh_remote_exit_cells maps each ExitLink → RouteExit via
// cell_at_border_link / border_road_links (sorted) — unchanged.
```

### Movement (stepper / receive / drain) — in the refactor

The stepper, the place-and-continue receive, the `Move`/`Rollback` drain, and the
`entry_link` convention are specified in `docs/20260629-unify-travel-tokens.md`. The only
seam this plan adds is **what `refresh_remote_exit_cells` reads** — the worker's
`region_routes` (above) instead of the direct `border_neighbor_map` — producing multi-hop
`RouteExit`s that the unchanged stepper walks to and the unchanged drain hands off.

## 5. Tests

`src/core/regions/directory.rs` (the central `region_routes` Dijkstra build)
- `region_routes_map_multihop_destination_to_first_hop` — A–B–C road graph: `to[C].from[A]`
  → A/B link (next hop B); `to[A].from[B]` → B/A link.
- `region_routes_pick_only_cost_decreasing_neighbour` — **loop-safety**: `to[C].from[B]`
  does NOT include the B/A link (`cost_to_C(A)` is not < `cost_to_C(B)`), even though A,B,C
  share one road component. Regression for the old component-membership bug.
- `region_routes_prefer_lower_cost_corridor` — **weighting**: A→B→C (2 hops, B crossing
  cost 15) vs A→D→E→C (3 hops, cost 4+5+4); `to[C].from[A]` routes via D — lowest road cost,
  not fewest hops. Drives the per-region `crossing_costs` end to end.
- `region_routes_skip_roadless_border` — **graph correctness**: A and B share a map border
  with NO road crossing, but a road path A–D–C exists; `to[C].from[A]` routes via D, never
  the roadless edge.
- `road_report_prices_entry_to_exit` (`regions/mod.rs`) — a region's `road_report` reports
  the Layer-2 Dijkstra `crossing_costs` between its border links (a longer/congested
  internal path costs more than a short one).
- `discovery_assembles_region_routes_from_published_costs` — `rebuild_discovery` builds
  `region_routes` from the published `RegionRoadReport`s; a road change reprices and
  rebuilds.

`src/core/regions/mod.rs` (routing wiring — the token/movement tests are in the refactor doc)
- `remote_exit_cells_routes_multihop_via_region_routes` — with an A–B–C registry,
  `A.remote_exit_cells[C]` resolves to a `RouteExit` crossing toward B (next hop), and
  `B.remote_exit_cells[C]` toward C — replacing the direct `border_neighbor_map` map.
- `multihop_handoff_uses_route_exit_link_not_re_derivation` — a `Move` at a multi-link border
  cell hands off via the carried `RouteExit.link`, not an ambiguous `exit_link_for`.

`src/core/regional_game_runner.rs` / `runtime/mod.rs`
- `multi_region_handoff_arrives_next_subtick` — the StepTravel barrier still gives
  one-sub-tick staleness across a 2-hop trip.

No UI test needed: rendering is unchanged (the token/adapter tests live in the refactor doc).

## 6. Risks / non-goals

- **Layer 1 is a central registry fed by per-region prices.** Each region prices its own
  crossings (Layer-2 Dijkstra over its `route_cache`, share-nothing) and publishes a
  `RegionRoadReport`; the coordinator-owned `CrossRegionDiscovery` snapshot
  (`directory.rs`) assembles them and runs **Layer-1 Dijkstra** in `rebuild_discovery`,
  read lock-free via `discovery_snapshot()`. It is the **cross-region sibling of
  `resource_registry.rs`** (region-local): build-once-cache-until-change; workers/regions
  read `region_routes`, never recompute a graph per pass.
- **Relation to `traffic-pathfinding-plan.md` §5f.** This plan *is* §5f's multi-hop
  two-layer routing **in full** — Layer 1 = weighted Dijkstra on the road-cost region graph,
  Layer 2 = the existing per-region road Dijkstra. `region_routes` is the destination-keyed
  form of §5f's `border_route_hint`, and the published `crossing_costs` = §5f's
  `border_crossing_cost`. It also **corrects** §5f:
  §5f had the *worker* compute `border_crossing_cost`, but the worker has no road graph —
  pricing must be per-region (share-nothing), which is steps 1–2 here. §5f still assumes the
  `return_path` stack and adjacency; this plan replaces both. Reconcile/cross-link §5f when
  this lands.
- **Loop-safety is the load-bearing invariant**: next-hop must STRICTLY decrease
  `cost_to_T` (a Dijkstra distance) **on the road-connected region road graph** (edges = real road
  crossings), so it can never descend a roadless border or loop. Computing on raw
  `RegionNeighborLink` adjacency would dead-end; the graph makes connectivity intrinsic (no
  separate filter). The `region_routes_pick_only_cost_decreasing_neighbour` test guards
  the no-backward-hop case.
- Determinism: Dijkstra over sorted region-road-graph edges with deterministic cost tie-breaks (cost,
  then `RegionId`, then `BorderLinkId`); every `Vec<ExitLink>` sorted + deduped. Cross-region
  remains one-sub-tick-stale, never non-deterministic.
- Do not expose `World`/topology to the UI; **no new worker command/protocol**; no new
  production dependency; do not store full routes in traveller state. (Each region's
  `RegionRoadReport` rides the **existing availability publish path**, and the snapshot
  is assembled in the existing `rebuild_discovery` chokepoint — no new message or command.)
- Do not rewrite away tokens on road/topology change — dynamic routing re-plans each
  step. A token with no progressing+reachable exit emits `PendingHandoff::Rollback`,
  routed to the home region **by id** — this needs **no new worker capability**:
  `route_traveler_handoff` (`worker.rs:642`) already routes any handoff by
  `handoff.to_region` via `owners.owner_of(target_region)`, regardless of adjacency, so a
  `Rollback` just sets `to_region = traveler.citizen.region()` and reuses that path. The
  home region's receive handles `Rollback` → `apply_traveler_return` before any border
  placement. `apply_traveler_return` is home-region-only and must never be relied on to
  un-strand from a transit/work region.
- Remove `return_path` in a small follow-up if deleting it in the same patch is noisy;
  first make behaviour stop depending on it.

## 7. P-a implementation record (per-region pricing)

P-a added per-region pricing. The region owns its road graph, so it computes
its own crossing costs (one Layer-2 Dijkstra per border-link pair) and
publishes a `RegionRoadReport` alongside the existing availability hint.

### What changed

- `src/core/regions/mod.rs` — new types `RegionBorderLink { link,
  neighbour }`, `RegionCrossCost { entry, exit, cost }`, `RegionRoadReport
  { region, border_links, crossing_costs }`. New `RegionState::road_report`
  method (one Layer-2 Dijkstra per border cell entry, priced against every
  other border cell exit). New `World::road_distance_to` (pure helper that
  uses `road_predecessors_with_dist` and does NOT touch `route_cache` — so
  it's safe to call from within another route-cache compute path).
- `src/core/regions/directory.rs` — `CrossRegionDiscovery.road_reports`
  field. New `RegionDirectory::publish_region_road_report` (idempotent
  rebuild guard, same pattern as the existing `publish_region`).
- `src/core/regions/worker.rs` — `add_region` and `process_region_events_with_mode`
  now also publish the road report. The worker recomputes the report from
  the **post-event** road graph (a build/bulldoze in this pass would have
  changed the topology, so a pre-event report would be stale).
- `src/core/regions/worker.rs` — `export_routing_reads_published_directory_without_rebuilding`
  updated to expect 1 rebuild (the initial road report publish from
  `add_region`).

### Test

`road_report_prices_entry_to_exit` — a 2×2 region with two adjacent
border-road cells: West/0 (r0) and East/0 (r1). The road report
contains symmetric West↔East crossing costs at 1 hop each, and
self-pairs (entry == exit) are filtered out.

## 8. P-b implementation record (Layer-1 Dijkstra)

**Committed:** not yet (in the working branch `multi-region-return`).
**What landed in the worktree:**

### Types (`src/core/regions/mod.rs`)

```text
RegionRoutes { to: HashMap<RegionId, RouteField> }
RouteField  { from: HashMap<RegionId, RouteHop> }
RouteHop    { exits: Vec<ExitLink>, cost: u32 }
ExitLink    { link: BorderLinkId, to_region: RegionId }
```

`RegionRoutes::exits_from(r) -> HashMap<RegionId, Vec<ExitLink>>` is the
ergonomic accessor P-c will use; right now nothing else in the code base
consumes `RegionRoutes` (deferred YAGNI until P-c).

### `RegionDirectory` ownership wiring (`src/core/regions/directory.rs`,
`src/core/regional_game_runner.rs`)

- `RegionDirectory` gained an `owners: Arc<RegionOwnerDirectory>` field and
  a `with_owners(topology, owners)` constructor.
- `regional_game_runner.rs:159–162` now creates the owner directory
  FIRST and passes it into `RegionDirectory::with_owners(...)` — same
  `Arc`, so the worker, the directory, and the route rebuild all see the
  same ownership state. The previous `RegionDirectory::new(topology)`
  stored a fresh empty owner dir, which would have filtered every region
  out of `region_routes` in production.
- `register_region` is now `pub(crate)` in `worker.rs` so the test helper
  in `directory.rs` can register regions for the owner filter.

### `build_region_routes` (`src/core/regions/directory.rs`)

```text
build_region_routes(reports, owners) -> RegionRoutes
  1. For every (r, n) pair joined by a published border link (a road
     actually crosses — adjacency alone is not enough), compute the
     intra-region border-exit cost from r and the intra-region border-
     entry cost from n. Weight = max(1, r_cost + n_cost). Keep ALL
     (weight, BorderLinkId) candidates per (r, n) — a multi-corridor
     border can have several exits.
  2. Destination-seeded Dijkstra (one per owned destination T).
     cost_to_t[r] = shortest path from r to T in the original directed
     graph; we relax INCOMING edges of each popped node (the reversed
     graph), so the distance is correct for asymmetric / directed
     edge weights. cost_to_t[T] = 0; everything else starts at u32::MAX.
  3. Next-hop selection: for each source region r, find every neighbour
     n with cost_to_t[n] < cost_to_t[r] (strict decrease = loop safety),
     keep the minimum total = w(r, n) + cost_to_t[n], expand ALL
     (weight, exit) candidates that share the minimum, sort the result
     by (total, to_region.0, edge, offset), emit each as an ExitLink.
     Sort+dedup on the final exits list guarantees deterministic output.
```

#### The patch (data flow)

```text
                  +-------------------------------------+
   P-a snapshot   |  reports: Vec<RegionRoadReport>     |
  (per region)    |   - border_links (which neighbour)  |
                  |   - crossing_costs (entry->exit)    |
                  +-----------------+-------------------+
                                    |
                                    v
                  +-------------------------------------+
                  |  build_region_routes(reports,       |
                  |                     owners)         |
                  |  1. r_to_n: HashMap<(r,n),          |
                  |       Vec<(weight, BorderLinkId)>>  |
                  |  2. for t in owned:                 |
                  |       cost_to_t via reversed-Dijkstra|
                  |  3. next-hop for each r             |
                  +-----------------+-------------------+
                                    |
                                    v
                  +-------------------------------------+
                  |  CrossRegionDiscovery.region_routes |
                  |  = RegionRoutes { to: HashMap<...> }|
                  +-------------------------------------+
                                    |
                          (P-c reads exits_from(r))
                          to drive remote_exit_cells
```

#### The problem the patch solves

Before P-b, the cross-region routing layer could not answer "where do I go
to reach a non-adjacent region?" — the only available structure was
`border_neighbor_map`, which is the *direct* (1-hop) adjacency. A
3-region commute A→B→C had no way to plan the first hop (A→B) because
nothing in the system knew C was reachable from A at all.

After P-b, `region_routes` is a complete multi-hop route map:
`from[A].to[C].exits` lists every BorderLinkId in A that begins a
shortest path toward C, with `to_region` set to the first hop region
(B). The P-c stepper reads this map to populate
`RegionState::remote_exit_cells`, replacing the direct-neighbour
producer.

### Determinism, loop-safety, and the `ponytail:` ceiling

- **Loop-safety**: edge weights are forced strictly positive via
  `max(1, r_cost + n_cost)`, so the strict-decrease next-hop rule
  (`n_cost >= r_cost → continue`) is always meaningful. The Dijkstra
  frontier is a `BTreeSet<(u32, RegionId)>` with lazy deletion, so
  tie-breaking on distance is by RegionId.
- **Tie determinism on next hops**: the next-hop selector collects
  every minimum-total candidate, sorts by `(total, to_region.0,
  edge, offset)`, and emits all of them. Final exit list gets a
  `sort_by_key + dedup` for a stable snapshot.
- **ponytail — node-graph double-counts interior regions.** The edge
  weight is the SUM of r's "interior→border" cost and n's
  "border→interior" cost, both measured by the producer's
  `crossing_costs`. In a line A-B-C with B's traversal cost 10 the
  algorithm reports `cost_to_C(A) = 20` (A→B = 10, B→C = 10) even
  though the true A→C interior cost is 10. The next-hop direction
  is still correct because the inflation is symmetric per-edge;
  only the absolute `RouteHop.cost` is approximate. P-c consumes
  `from[r].exits`, not `.cost`. The exact fix is a border-node /
  line-graph Dijkstra keyed on `(region, border_link)`; deferred
  until the cost is used for a budget.

### Tests added (`src/core/regions/directory.rs` tests module)

| Test | What it locks down |
|---|---|
| `region_routes_map_multihop_destination_to_first_hop` | A→B→C: A's hop for destination C is B (cost 4 = 1+1+1+1). B's hop for destination A is A (cost 2). |
| `region_routes_pick_only_cost_decreasing_neighbour` | Edges are forced strictly positive; a leaf B's only neighbour A gets selected via strict-decrease. |
| `region_routes_prefer_lower_cost_corridor` | A→B→C (cost 8) loses to A→D→E→C (cost 4) when the corridors differ. |
| `region_routes_skip_roadless_border` | A and B share a map border but no road crosses; the route A→D→C skips the roadless edge entirely. |

`owned(&[ids])` is a `pub(crate)` test helper that registers the given
region ids in a fresh `RegionOwnerDirectory` and wraps it in an `Arc`
— the routes builder filters by `owner_of(r)`, so tests must register
the regions they use.

### Known limitations (carried into P-c)

- The `ponytail:` heuristic above: `RouteHop.cost` is approximate, not
  exact. P-c reads `from[r].exits` and the `BorderLinkId` it carries,
  not the cost.
- Tests use a self-pair `crossing_cost(entry, exit)` data shape, which
  the real `road_report` producer does NOT emit (it skips `entry ==
  exit` pairs). The arithmetic is validated against the formula; the
  real-shape producer data is exercised by integration tests (P-c).

## 9. P-c implementation record (wiring)

**Committed:** not yet (in the working branch `multi-region-return`).
**What landed in the worktree:**

### `RegionDirectory::exits_from` (`src/core/regions/directory.rs`)

A reader-side accessor over the active snapshot:

```text
exits_from(&self, region: RegionId) -> Option<HashMap<RegionId, Vec<ExitLink>>>
    None  - snapshot has no report yet for this region, or the region has
            no reachable targets.
    Some  - for every reachable target T, the first-hop ExitLinks
            `region` should use to head toward T.
```

`None` on empty: callers (the worker) use this to decide whether to
call `RegionState::set_region_routes` at all on this pass.

### `RegionState::set_region_routes` (`src/core/regions/mod.rs`)

```text
set_region_routes(&mut self,
                  exits_from: &HashMap<RegionId, Vec<ExitLink>>)
    1. Build a BorderLinkId -> local-cell index from `border_road_links()`.
    2. For every (target, exit) in `exits_from`, push the local cells
       matching `exit.link` into `map[target]`. (Multi-hop and direct
       neighbours are handled uniformly — both are recorded under the
       FINAL target region.)
    3. Gap-fill: for every direct neighbour in `border_neighbor_map`
       that is NOT mentioned in `exits_from`, push its border cells.
       (A direct neighbour absent from routes has no published report
       yet, so the direct cell is the only exit available.)
    4. sort + dedup the exit cell lists; assign to remote_exit_cells.
```

The gap-fill is **gap-fill only** — it does NOT inject a direct cell
when routes mention a target but resolve to no local cell. The
distinction matters because a route's first hop may pick a non-direct
border (a cheaper detour) and a direct-cell injection would defeat
that choice for the consumer's `depart_toward` step (which picks the
first reachable candidate, not the cheapest).

### `RegionRuntime::set_region_routes` (`src/core/regions/runtime/mod.rs`)

Thin wrapper: `runtime.state.set_region_routes(exits_from)`. The
`pub(crate)` API is the same shape as `set_border_neighbor_map` and
`set_importable_remote_jobs` — runtime owns the write path, state
holds the data.

### Worker wiring (`src/core/regions/worker.rs`)

After `publish_region_road_report` rebuilds the directory's discovery
(so the snapshot is fresh), the worker:

```rust
if let Some(exits) = self.directory.exits_from(source_region) {
    runtime.set_region_routes(&exits);
}
```

Ordering is synchronous: `publish_region_road_report` swaps the active
snapshot before returning (`directory.rs:255–258`), so the
subsequent `exits_from` read sees the latest state. Cross-pass
staleness against *other* regions' reports is the accepted one-tick-
stale barrier model (relaxed across regions, strict within).

If `exits_from` returns `None` (no report published yet for THIS
region), `set_region_routes` is skipped and `remote_exit_cells`
retains the direct build from the earlier `set_border_neighbor_map`
call — preserving the old behaviour for the moment between the two
calls.

#### The patch (data flow)

```text
   (per region, after the route report is published)
                  +-------------------------------------+
                  |  publish_region_road_report(report) |
                  +-----------------+-------------------+
                                    |
                                    v
                  +-------------------------------------+
                  |  directory.rebuild_discovery:       |
                  |    build_region_routes              |
                  |    -> RegionRoutes { to: ... }      |
                  |    (active_snapshot = Arc::swap)    |
                  +-----------------+-------------------+
                                    |
                                    v
                  +-------------------------------------+
                  |  directory.exits_from(source_region)|
                  |    -> HashMap<T, Vec<ExitLink>>     |
                  |       (T = final target,            |
                  |        ExitLink = first hop)        |
                  +-----------------+-------------------+
                                    |
                                    v
                  +-------------------------------------+
                  |  RegionState::set_region_routes     |
                  |    -> remote_exit_cells             |
                  |       (target T -> local cells,     |
                  |        with gap-fill from direct)   |
                  +-----------------+-------------------+
                                    |
                                    v
                  +-------------------------------------+
                  |  travel::resolve_target reads       |
                  |  remote_exit_cells[target.region]   |
                  |  and departs to a local cell.       |
                  +-------------------------------------+
```

#### The problem the patch solves

Before P-c, the only producer of `remote_exit_cells` was
`border_neighbor_map` — a direct (1-hop) neighbour map. A 3-region
commute A→B→C had no entry for C in `remote_exit_cells[A]` because
A does not border C, so the stepper saw `None`, fell through to
`Target::Building(home)`, and the citizen idled at home forever.

After P-c, `remote_exit_cells` is sourced from the multi-hop
`region_routes` map (P-b output). For every reachable final target
T, the first-hop border cells are recorded. A→B→C works because
`region_routes[A].to[C].from[A].exits[0] = (A's_B-border_link, B)`
and the local cells matching that link end up in
`remote_exit_cells[A][C]`. The stepper departs to one of those
cells, the existing move-not-convert handoff crosses into B, and
B's analogous map drives the second hop.

### Determinism, gap-fill correctness, and stale-barrier notes

- **Determinism**: `remote_exit_cells` values are `sort()`+`dedup()`;
  map keys are target regions, value content is a set-union over
  route exits + direct cells. HashMap iteration order is irrelevant.
- **Gap-fill correctness**: keyed on `exits_from.contains_key(&neighbor)`,
  not `map.contains_key(&neighbor)`. A target the routes mention but
  whose BorderLinkId has no local cell still has its key absent from
  `map`; the route's choice must be respected anyway.
- **One-tick staleness**: the worker publishes THIS region's road
  report, the directory rebuilds, then `exits_from` reads the new
  snapshot. Other regions' road reports from prior passes may be
  visible — that is the cross-region relaxed-barrier contract, and
  P-c's correctness does not require fresh reports from neighbours.
- **No-routes path**: `exits_from` returns `None` on empty →
  `set_region_routes` skipped → `remote_exit_cells` keeps the
  direct build from `set_border_neighbor_map`. The two state
  producers compose without conflict.

### Tests added (`src/core/regions/mod.rs` tests module)

| Test | What it locks down |
|---|---|
| `set_region_routes_populates_remote_exit_cells` | The 1-hop happy path: routes mention region 2 with the East exit, the local East-edge cell ends up in `remote_exit_cells[2]`. |
| `set_region_routes_falls_back_to_border_neighbor_map` | Empty `exits_from`: only the direct neighbour in `border_neighbor_map` contributes; the gap-fill works. |
| `set_region_routes_does_not_inject_direct_when_routes_cover` | Routes cover region 2 via a non-East detour link (no local cell resolves); the East cell from `border_neighbor_map` is NOT injected — the fallback is gap-fill only, never re-injection. |

### Known limitations (carried forward)

- A multi-hop commute that requires the first hop to use a non-direct
  border will work as long as the route's first hop's BorderLinkId has
  a local road cell. If a region's report picks a border exit that has
  no local road (e.g. a brand-new detour), `map[target]` ends up
  empty and the consumer falls through to "stay put". This is
  acceptable because the report comes from the region's own road
  graph — the border cell exists by construction.
- The cross-region one-tick staleness contract still applies: a
  region A's `remote_exit_cells[T]` reflects the routes snapshot at
  the end of A's pass, which may include a neighbour's report from
  a prior pass. The P-b producer re-prices reports after every
  build/bulldoze (`worker.rs:550–565`), so the staleness is bounded
  to one regional pass.
## Suggested patch split

**Prerequisite:** the token refactor (`docs/20260629-unify-travel-tokens.md`) lands first —
one `TravelToken`, one stepper, `Move`/`Rollback` handoff, `RouteExit` shape. This plan then
only swaps what feeds the stepper's `remote_exit_cells`:

- **P-a (per-region pricing):** `road_report` (Layer-2 Dijkstra over `route_cache` →
  `border_links` + `crossing_costs`) published via the availability path; `RegionRoadReport`
  (input) on `CrossRegionDiscovery`. Region tests (`road_report_prices_entry_to_exit`).
- **P-b (the L1 registry):** `RegionRoutes` (output) + `build_region_routes` (Layer-1
  Dijkstra) in `rebuild_discovery`; build-under-`publish_state`, swap the snapshot
  (`docs/20260628-l1-map-build-locking.md`). Directory tests (multihop, cost-corridor,
  loop-safety, roadless-border).
- **P-c (wire it in):** **repoint `refresh_remote_exit_cells`** from the direct
  `border_neighbor_map` producer to `region_routes` (→ multi-hop `RouteExit`s), retiring
  `border_neighbor_map`'s routing use; worker reads `region_routes`. Region tests
  (`remote_exit_cells_routes_multihop_via_region_routes`). Direct neighbours keep working —
  they're the 1-hop case of the new map; the stepper/handoff are unchanged from the refactor.
