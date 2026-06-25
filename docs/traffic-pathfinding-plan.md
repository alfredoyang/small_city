# Traveling Citizens — pathfinding, movement, and animation

Status: **Greenfield re-implementation; P1–P6 unbuilt.** Targets `master`'s
`Entity(u64)` model (post-EC1/EC2: `Entity::region()` / `as_local()` carry the
local-vs-remote distinction via the packed birth region). Core/simulation feature with
a thin UI surface. Supersedes the earlier "traffic via path reconstruction" draft; the
headline goal is **citizens that visibly move to their destinations**, so the
architecture is built around per-citizen movement, not an aggregate load scalar. (A
heatmap can still fall out as an optional consumer — see P6.)

§5 covers cross-region commuting via a token handoff using `as_local()` for remote
detection and a worker-provided `border_neighbor_map` hint for border-exit cell
selection; v1 = direct neighbors only.

## Key terms (glossary)

Terms used throughout this doc, defined here for quick reference. Each is
explained in detail where first introduced; this is a one-page cheat sheet.

| Term | Meaning |
|---|---|
| **`came_from`** | A `HashMap<Entity, Entity>` of parent pointers — the data structure that *is* a path. `came_from[X] = Y` means "to reach the destination, go from X to Y" (the pointer points *toward* the destination). One tree per (destination, road network) pair; shared by all citizens going to that destination. See §2a. |
| **Dijkstra predecessor tree** | Same as `came_from`. The *result* of running Dijkstra from the destination's entry cells — a tree rooted at the destination, expanded outward to every reachable cell. "Predecessor" = parent pointer. |
| **`route_cache`** | `RefCell<HashMap<(dest_entity, road_network_id), HashMap<Entity, Entity>>>` — the region's collection of `came_from` trees, keyed by destination and road network. P2, see §2 + §3. |
| **`route_epoch`** | A *hypothetical* `u64` counter on `World` that would track "which version of the road graph each cache entry was built against." **Not built** — the plan rejected it in favor of wholesale `clear()`. See §3. |
| **destination-rooted search** | Dijkstra starts from the *destination's* entry cells and expands outward to every origin. One search serves all origins on that road network. Per-citizen origin-rooted search (e.g. A*) is not used — it would break the shared-cache design. See §2. |
| **`crossing_penalty`** | The extra weight added to a road cell that has > 2 road neighbors (T-junction or 4-way intersection). Default = 2. `edge_weight = 1 + crossing_penalty`. See §7b P1. |
| **`TravelState`** | The per-citizen travel *token* — `{ status, current_cell, destination, building }` (`building` = the building occupied while idle, i.e. the next departure origin; `None` while travelling). The citizen entity never moves; the token's `current_cell` advances along roads. Stored in `world.travel: HashMap<Entity, TravelState>`. P3 (`components.rs`), see §2 + §5. |
| **`TravelStatus`** | `AtHome | AtWork | Traveling` (P3); P5 adds `Away` (token is in another region). |
| **`TravelerId`** | `(citizen: Entity, generation: u32)` — round-trip identity for a cross-region token. `citizen.region()` IS the home region. See §5c. |
| **`TravelerHandoff`** | The cross-region message: `TravelState` + `TravelerId` + `to_region` + `entry_link` + `return_path` + `purpose` (Outbound/Return). Routed by the worker like an export request. See §5c. |
| **`return_path`** | `Vec<ReturnHop>` — stack of region hops for the return trip. Pushed one hop at a time on outbound, popped one hop at a time on return. See §5c, §5f. |
| **`ReturnHop`** | `{ region: RegionId, entry_link: BorderLinkId }` — one hop in the return path. |
| **`border_neighbor_map`** | A worker-provided hint: `HashMap<BorderLinkId, RegionId>` — "this border link faces this neighbor region." v1 direct-neighbor only; multi-hop extends to weighted Dijkstra (§5d, §5f). |
| **`border_route_hint`** | Multi-hop extension of `border_neighbor_map`: `HashMap<BorderLinkId, HashMap<RegionId, u32>>` — weighted Dijkstra distance (road-cost-weighted) to each reachable region through each border link. See §5f. |
| **`border_crossing_cost`** | Multi-hop worker-side table: `HashMap<(RegionId, BorderLinkId, BorderLinkId), u32>` — road-level Dijkstra cost to cross a region from one border link to another. Precomputed by the worker. See §5f. |
| **`TravelPurpose`** | `Outbound | Return` — whether a handoff is going to the workplace or coming home. See §5c. |
| **Token / movement token** | Informal name for `TravelState` (and its `visiting_travel` variant) — the thing that visibly moves on the map. The citizen entity is the *identity*; the token is the *moving representation*. |
| **layer 1 / layer 2** | The two layers of cross-region routing. **Layer 1:** weighted Dijkstra on the cross-region component graph (region-level, worker-side). **Layer 2:** Dijkstra with crossing penalty (road-level, per-region, share-nothing). See §5f. |
| **`SchedulePhase`** | `Work | Home | Leisure` — pure phase from the hour alone. See `docs/citizen-schedule-plan.md`. |
| **`ScheduleIntent`** | `Home | Work(Entity) | Leisure` — semantic intent for a local citizen; the movement system resolves it to a target. See `docs/citizen-schedule-plan.md`. |
| **`resolve_intent`** | Movement-side: `ScheduleIntent` → `Entity` target. `Work(remote)` → idle (P3) or border-exit (P5). See `docs/citizen-schedule-plan.md`. |
| **`Away` (TravelStatus)** | P5: the token is out in a neighbor region; the home side neither draws nor dispatches the citizen until `Return`. See §5. |
| **fire-and-forget** | A handoff or event is emitted with no acknowledgement, grant/deny, reservation, or paused continuation. The handoff rides on the existing event flow, one-tick-stale. See §5c. |
| **token location (unbuilt)** | "Which region holds this token?" — no current UI feature asks this, so the mechanism is YAGNI. Approach when needed: fan out to all workers, scan `visiting_travel` directly. See §5i. |
| **coarse clear** | `route_cache.clear()` on a **road** topology change. "Coarse" = drops every entry, not just the affected ones. For building changes, the strategy is per-destination eviction (only the affected building's entry is removed). See §3. |
| **derived cache** | A cache stored on `World` at runtime (`#[serde(skip)]` so it's not persisted) and recomputed from the world on demand. Invalidated when the world changes. The route cache and the `ResourceRegistryCache` are both derived caches. |
| **destination entry road cell(s)** | The road cell(s) touching a destination building's footprint. These are the Dijkstra sources. |

## Goal

Citizens travel along roads from origin to destination (home↔work, home→shop) and
the map shows them moving. Pathfinding is the *enabler*; the deliverable is the
moving citizens.

```text
 ┌ Region 2 ────────────────────────┐
 │ 🏠 ·· •→ ·· ·· 🏪                 │   • = a citizen on a road cell, facing →
 │ ·· ·· •→ ·· ·· ··                 │   it advances cell-by-cell toward 🏪
 └───────────────────────────────────┘
```

## Architecture at a glance

```text
CORE (deterministic, owns ALL computation)        UI (renders only)
──────────────────────────────────────────        ─────────────────
Region (World):                                    adapter ─► CitizenTravelView
  route cache (derived, #[serde(skip)]):               { x, y }  (no direction/status)
    RefCell<HashMap<(dest, net_id),                    TUI: draw '•·' per traveler cell
      HashMap<Entity,Entity>>>                         (Normal overlay only); never
    cleared wholesale on road change /          sees paths or the graph
      per-destination on building change
      (chokepoint-specific dispatch — see §3)
  per-citizen travel state:
    world.travel: HashMap<Entity, TravelState {
      status, current_cell, destination, building }>
  movement system: advance travelers each tick
  cross-region (P5): the SAME token crosses on a
    RegionEvent to the neighbor's visiting_travel map
    (entity never migrates; no proxy) — §5
    TravelerId { citizen: Entity, generation: u32 }
      // citizen.region() IS home region
```

Two hard rules this design exists to honour:

1. **Strict layering.** The UI knows only a citizen's *cell position* — everything
   else (paths, the road graph, progress math) is computed in core and exposed through
   a view model. The Traffic/animation UI never reads ECS.
2. **Determinism + the one-tick-stale boundary.** Movement is deterministic core
   state (saved-or-derived, replayable). Cross-region effects read the neighbour's
   *previous-tick published snapshot*, never its live `World` — like power/jobs/goods.

---

## 1. What already exists (reuse, don't rebuild)

`src/core/systems/road_network_analysis.rs` has the deterministic road-graph
machinery:

- **`road_distances(world, network, sources) -> HashMap<Entity, u32>`** — multi-source
  BFS, FIFO queue, neighbours sorted by `road_connectivity::sort_entities_by_position`,
  **first-visit-wins**. Returns hop-counts only (no paths). **Stays plain BFS** — its
  output feeds `commute_distance`/`nearest_shop_distance` → economy/happiness formulas,
  so no crossing penalty here (would change balance).
- **`road_predecessors` (P1, new) — weighted Dijkstra** with a crossing penalty. Same
  destination-rooted, multi-source shape, but a min-heap instead of a FIFO queue and
  `edge_weight = 1 + crossing_penalty(current)` (destination-rooted: the penalty
  charges the cell being entered in the forward direction, see §7b) instead
  of `+ 1`. Returns `came_from` (path reconstruction), not distances. Its output
  feeds the travel/movement system only (display-only), so the penalty affects
  path *quality*, not economy. See §7b.
- **`adjacent_road_entities` / `adjacent_cells`** — already **footprint-aware**: they
  enumerate every footprint cell of a building and collect all surrounding road cells.
- `RoadNetwork` (connected road component), `sort_entities_by_position` (stable order).

Trip endpoints exist: `Citizen { home, workplace_assignment }` (commute). On master,
`workplace_assignment.workplace` is a city-wide `Entity` (packed `u64` = birth region
`<< 32 | local`); a job is **local iff `workplace.region() == world.region_id`** — no
separate `Local`/`Remote` tag. This is the **EC1/EC2 collapse** (see
`docs/collapse-city-refs-into-entity-plan.md`).

The cross-region scaffolding exists: `RegionNeighborLink` / `BorderLinkId` /
`NetworkBorderLink` topology, `network_border_links()`, the `RegionDirectory`
component graph, and the `RegionEvent` / `OutboundMessage` event flow that carry
power/jobs/goods export. The producer-owned `ExportAllocations<Entity>` engine
(CR3R) routes job requests from consumer to producer; `JobExportGrant` carries the
producer's region-tagged `Entity` workplace + `CityCellRef` location. The consumer
stores it as `WorkplaceAssignment { workplace, location, salary }` and never
dereferences the remote entity.

**The gap:** `road_distances` returns distances, not *paths*; there is no per-citizen
movement and no route cache. P1 adds `road_predecessors` (weighted Dijkstra with a
crossing penalty) for path reconstruction; P2–P6 build the rest (cross-region needs
the token handoff — see §5).

---

## 2. Where the data lives — region-owned route cache, per-citizen trip state

A path is a pure function of `(road graph, origin cell, destination cell)`; the
citizen's identity is irrelevant. Storing it per-citizen would duplicate identical
routes (15 residents → 15 copies). So split by what each piece actually depends on:

```text
REGION (one cache, shared)                       CITIZEN (per-entity, tiny)
  the "pathfinding table" = route geometry         status:       TravelStatus
  keyed by (DESTINATION, ROAD NETWORK)             current_cell: Option<Entity>  (road cell now)
  destination-rooted Dijkstra predecessor trees    destination:  Option<Entity>
  (crossing-penalty weighted; see §7b)             (steps via the region's came_from,
  RefCell<HashMap<(dest, net_id),                    O(1) per tick — no stored path)
    HashMap<Entity,Entity>>>
  cleared wholesale on road change / per-destination on building change (§3)
```

### 2a. How a path is represented — the `came_from` predecessor tree

A path is never stored as a `Vec<Entity>`. It's stored as a
`HashMap<Entity, Entity>` of parent pointers — the `came_from` tree built by
destination-rooted Dijkstra. The citizen reconstructs the path by walking the tree
inward, one HashMap lookup per tick.

**Example — path A → B → C → D (A = origin, D = destination entry road cell):**

```text
  In this simplified example, D is a destination entry road cell (a road cell adjacent
  to the destination building's footprint). The real Dijkstra sources are the
  building's adjacent road cells; the building itself is not on the road graph.

  road graph:  A ─=─ B ─=─ C ─=─ D       (one connected road network)

  destination-rooted Dijkstra (sources = [D], the destination entry road cell)
  records came_from[child] = parent as it expands outward:

    HashMap<Entity, Entity> = {
      B: C,        ← B was reached from C
      C: D,        ← C was reached from D
      A: B,        ← A was reached from B
    }
    (D has no parent — it's a source.)

  Walking the tree from A (one lookup per tick):
    current_cell = A
    current_cell = came_from[A] = B      ← step 1
    current_cell = came_from[B] = C      ← step 2
    current_cell = came_from[C] = D      ← step 3
    arrived: came_from[D] is None (D is a source)
      current_cell = None   ← arrived at the destination root; status = AtWork
```

**Why the pointer points TOWARD the destination:** the search is destination-rooted
(sources = destination's entry cells, expand outward). `came_from[X] = Y` means
"to reach the destination, go from X to Y" — the pointer points at the destination.
The citizen follows `came_from` inward until it hits a source (the destination's
entry cell), at which point it has arrived.

**Why a tree (not a `Vec`):**

- **Storage:** 15 residents → same workplace on the same network = 1 shared tree,
  not 15 copies of the identical path.
- **Step:** `current_cell = came_from[current_cell]` — O(1) HashMap lookup, no
  index, no re-walk.
- **Cache:** clear the whole tree on topology change (§3); the `ponytail:` note
  names per-cell-scoped eviction as the upgrade path only if profiling says so.

**Where the weight lives:** the edge weight (`1 + crossing_penalty`) is a pure
function of the road graph at the cell being entered during relaxation (in
destination-rooted search, that's `current` — the cell closer to the
destination, see §7b P1 for the exact formula) — *not* stored. It's called on
the fly during Dijkstra for every
neighbor expansion. The `came_from` tree is the *result* of running Dijkstra with
these weights; that's what's stored in the route cache.

- **Region owns the route table.** Best form: **destination-rooted Dijkstra** — one
  search per `(destination building, road network)` covers *all* origins on that road
  network. Cache
  `RefCell<HashMap<(dest_entity, road_network_id), HashMap<Entity,Entity>>>`.
  The `road_network_id` matters because a multi-cell destination can touch roads
  from more than one disconnected local road network; those trees must not collapse
  into one entry.
  A citizen reconstructs its route by walking `came_from` from its origin entry cell
  — O(path length), zero per-citizen geometry storage.
- **Citizen owns only trip state** (in `world.travel`, not a `Citizen` field): its
  status, the road cell it is on **now**, and its destination. Because the search is
  destination-rooted, `came_from` already points *toward* the destination, so
  movement is **O(1) per tick** with no per-citizen path `Vec` and no re-walk:

  ```text
  store   current_cell: Option<Entity>          (None = inside a building, not on a road)
  step    current_cell = came_from[current_cell] (one cell toward the destination)
  arrive  current_cell is a destination root  ─►  status = AtHome/AtWork, current_cell = None
  ```

Why region (not per-citizen, not city-global):

|                     | Per-citizen                  | Region                                              | City-global               |
|---------------------|------------------------------|-----------------------------------------------------|---------------------------|
| Dedup of shared routes | No (N copies)              | Yes (one tree/dest)                                 | Yes                       |
| Dirtying            | Walk every citizen            | One `clear()` per topology change                   | Coarse global             |
| Search work         | Risk per-citizen recompute    | One Dijkstra/(dest, network) serves all origins     | Same                      |
| Threading fit       | ok                            | Matches per-region World ownership                  | ✗ shared cross-thread cache |
| Cross-region        | awkward                       | Each region routes its own segment                  | ✗ mixes region graphs     |

City-global is rejected: it breaks the share-nothing region model (regions own their
`World`/derived state per thread) and a path never spans graphs anyway.

This mirrors the existing **`ResourceRegistryCache` (R5)**: a `#[serde(skip)]`
derived cache recomputed on change and invalidated at the `attach_*`/`remove_entity`/
upgrade chokepoints. The route cache is another such region-owned derived cache.

---

## 3. Dirty mechanism — coarse clear for roads, per-destination eviction for buildings

The route cache invalidation strategy is **chokepoint-specific** — it depends on
*what* the chokepoint mutates:

```text
ROAD change (build::road, bulldoze(road), or a network-component rebuild
             caused by changed topology during a build/bulldoze — NOT a load):
    ─► route_cache.clear()                       (coarse — any tree might be affected)
    note: load is excluded — `route_cache` is `#[serde(skip)]` and starts empty
          on a freshly-loaded world; a redundant load-invalidation path would
          be dead code. This section's "rebuild path" refers to the in-game
          topology rebuild triggered by build/bulldoze.
    reason: a new road can connect previously-disconnected areas, a removed
            road can disconnect them; the affected set isn't computable from
            a single road change.

BUILDING change (bulldoze(building), upgrade, or any operation that
                changes an existing building's footprint):
    ─► evict every (building_entity, _) entry from the route_cache.
    (The building may have been in road networks that are no longer reachable
     from its footprint after the change — e.g. an upgrade grew the footprint
     and disconnected from one network, or a removal deleted the footprint
     entirely. Scanning only the currently-touching networks would miss
     entries the building used to be in.)
    reason: a building change affects only this building's destination entry
            roads. Other destinations' trees are unaffected — their entry
            cells and reachability don't change.
    note: a new building doesn't need invalidation — the cache just doesn't
          have the entry yet, first access will miss and compute.

POWER / POPULATION / BUILDING-KIND change (attach_power_provider,
        attach_power_consumer, attach_building, etc.):
    ─► no route cache invalidation.
    reason: these don't change the road graph or building footprints. The
            ResourceRegistryCache still invalidates at the resource chokepoints
            (its concern), but the route cache doesn't piggyback.

citizen needs route to B (on a given road network):
    entry = route_cache[(B, network)]
    if entry is None  ─► recompute (dest-rooted Dijkstra via road_predecessors), insert
    else              ─► reuse
```

### Chokepoint dispatch

System-level route-cache invalidation lives at exactly three sites
(see the table below for what each does):

```text
build::road       (systems/build.rs →           ─► dispatch at placement::place_building:
                 placement::place_building       road   ─► coarse route_cache.clear()
                 with kind == Road)              building ─► (no invalidation;
                                                                    a new building is a
                                                                    cache miss, not a
                                                                    stale entry)
replace           (systems/replace.rs)         ─► demolish old + place new:
                                                  on the demolish step, dispatch at
                                                  entity_cleanup::remove_entity (branches
                                                  on saved kind: road → coarse clear,
                                                  building → per-destination eviction);
                                                  on the place step, same dispatch as
                                                  build (see above).
bulldoze          (systems/bulldoze.rs →        ─► dispatch at
                 entity_cleanup::remove_entity)    entity_cleanup::remove_entity
                                                    (branches on saved kind: road →
                                                    coarse clear, building →
                                                    per-destination eviction)
upgrade           (systems/upgrade.rs::upgrade  ─► dispatch at
                   → grow_to_level)                entity_cleanup::remove_entity for
                                                  each absorbed neighbour building
                                                  + per-destination eviction for the
                                                  surviving building after its footprint
                                                  changes (grow_to_level grew it)
```

**The dispatch is centralised at two shared dispatch points plus one explicit
`upgrade::grow_to_level` eviction** — `placement::place_building` (the only
place that creates new topology; branches on kind: road → coarse clear, building
→ no invalidation) and `entity_cleanup::remove_entity` (the only place that
removes topology; branches on kind: road → coarse clear, building →
per-destination eviction), plus the explicit per-destination eviction for the
surviving building after its footprint changes. All command-layer entry points
(`build::road`, `build::building`, `replace`, `bulldoze`, `upgrade`) flow
through one of these dispatch points. This is the *shortest* implementation —
no per-command plumbing, no duplicated dispatch, no kind-checking duplicated
across commands.

`World::attach_position` and `World::attach_building` do not invalidate
routes. System-level invalidation uses the three sites above.

### Why roads and buildings are different

The route cache stores `came_from` trees, one per `(dest, network)`. Each tree
is **destination-rooted** — Dijkstra expands outward from the destination's
entry cells to every reachable cell on that road network.

**A road change** can affect *any* tree: a new road can connect previously
disconnected areas, a removed road can disconnect them. The affected set
isn't computable from a single `attach_position` call — the road might
bridge two road networks, or add an entry road to a destination anywhere.
Coarse clear is the only safe option.

**A building change** affects only this building's destination entry roads
(the cells touching the building's footprint). If the building *is* a
destination (e.g. a residential home that is someone's `home`, a power plant
that someone commutes to), its own tree is stale and must be evicted. If the
building is a *new* destination, the cache just doesn't have the entry yet —
first access will miss and compute (no eviction needed). If the building
*isn't* a destination, the eviction is a no-op (`HashMap.remove` returns
`Option::None`). Either way, **other destinations' trees are unaffected** —
their entry cells and reachability don't change when a different building
moves or is removed.

**A power / population change** doesn't touch the road graph or building
footprints at all. It might change the resource registry (power resolution
depends on which buildings are producers/consumers), but the route cache
cares about roads and buildings, not power. The route cache doesn't
piggyback on `attach_power_provider` / `attach_power_consumer` / similar
chokepoints.

### Note on the older plan's "wider than 'road changed'" statement

The earlier draft of this plan said: *"building placement/removal and
footprint growth can change destination roots / entry roads even if the road
tiles themselves did not change — all handled by the coarse clear."* That
was correct in *consequence* (a stale tree would be served) but overly coarse
in *mechanism* (clearing every tree when only one needs eviction). The refined
strategy above does per-destination eviction for building changes and
reserves coarse clear for road changes — less wasted work, same correctness.

### 3a. Destination removal — self-heals via the schedule

The P3 implementation **dropped the explicit removal scan** from the original plan
(§3a). Instead, a removed destination **self-heals**: the per-tick schedule
recompute + per-destination eviction (§3) + §4b (unreachable → stay put) handle
it without a dedicated chokepoint scan. `travel::run` prunes dead citizens from
`world.travel` each tick, and the adapter filters to live ones. The `ponytail:`
note on this decision: it trades an O(citizens) scan at removal time for an
O(citizens) re-derive each tick (which P3 already does). If removal-frequency ×
citizen-count ever makes the re-derive hot, add the explicit scan back.

---

## 4. Movement (core, deterministic)

### 4a. Daily schedule — see `docs/citizen-schedule-plan.md`

The schedule (which intent a citizen emits by hour) is defined in
[`docs/citizen-schedule-plan.md`](citizen-schedule-plan.md). v1: commute-only
(09–15 work, else home); free-time/leisure deferred. Remote workers idle at home
until P5 (§5d).

The movement system consumes the schedule's intent and resolves it to a target each
tick:

```text
at target building?  ─► status = AtHome / AtWork ; current_cell = None (idle)
target changed?      ─► depart: current_cell = an entry road cell of the current
                          building; status = Traveling; destination = target
en route?            ─► step current_cell = came_from[current_cell] (one cell);
                          arrived when current_cell is a destination root
```

The **desired phase** changes at 09:00 (home→work) and 15:00 (work→home); the
movement system reconciles per-tick (a trip may start later after load, a delayed
handoff, or an assignment change). Remote commuters depart at 09 toward the
border-exit cell (§5).

### 4b. Degenerate routes → stay at current location (no teleport)

If the target has **no road route** from the origin (different `RoadNetwork`, or
disconnected), the citizen does **not** teleport: it **stays at its current location**
(if idle in a building) or **remains on its current road cell** (if en route). The
schedule re-derives the intent each tick; the movement system resolves it and retries
routing as soon
as a road reconnects. A citizen stranded at work when the road home is torn up stays
`AtWork`, not `AtHome` — it routes home when the road reconnects. (A *cross-region*
workplace is not unreachable — it targets the border-exit cell, then hands a token
across the border; see §5. If the workplace region is not a direct neighbor, v1
treats it as unreachable → stay at current location.)

**Exception — destroyed origin (not a degenerate route).** The no-teleport rule
above applies when the origin building *still exists* but cannot reach the target.
If the building a citizen is idling in is **bulldozed/replaced**, the origin itself
is gone — there is no "current location" to stay at — so the displaced citizen
returns **home** (`travel::run` falls back to `home` when `state.building` no longer
exists in `world.buildings`). This is intentionally distinct from §4b's stranded
case: a stranded citizen's origin still exists and it stays put; a displaced
citizen's origin was deleted. Relocating onto the building's adjacent road instead
would require capturing the road at the removal chokepoint (coupling movement into
`entity_cleanup`); the home fallback avoids that for a corner case.

### 4c. Granularity / determinism / persistence

- **v1 granularity: tick-cadence (accepted).** A traveler advances one cell per tick;
  the UI shows discrete steps; paused sim ⇒ frozen. A smooth sub-cell tween is the
  *only* thing that could ever live in the UI (off `anim_frame`, between two
  core-provided positions) — deferred (P6), add only if the stepping looks too coarse.
  **Travel runs on ticks only** — `travel::run` is wired into the tick path
  (at `simulation.rs:199`, after `happiness::run` at `:196`, before `turn += 1`
  at `:200`), not into the paused-config re-derive (`ensure_derived_state` at
  `simulation.rs:256` calling `refresh_derived_state_for_world` at `:269`). A
  build while paused won't restep travelers until the next tick. This is
  acceptable: travel state is `#[serde(skip)]` display-only, so a one-tick
  re-derive is free.
- **Determinism:** integer math; travelers iterated in a fixed order
  (citizens are off-grid, sorted by `entity.0`); Dijkstra min-heap with entity-id
  tie-break (see §7b); first-dequeue-wins → identical `came_from` and movement for
  identical inputs.
- **Persistence:** travel state is transient derived runtime state
  (`#[serde(skip)]`). Save/load does **not** preserve in-flight trips; on load the
  movement system rebuilds placement from the current hour and each citizen's
  assignments. No save-format change.

`TravelStatus` (v1): `AtHome | AtWork | Traveling`. P5 adds `Away` (the
token is out in a neighbor region; the home side neither draws nor dispatches W until
`Return`). **No `Direction`/facing** — P4 draws a plain dot `•·`, not an arrow; facing
is deferred (the `TravelState` carries no heading).

---

## 5. Cross-region travel — token handoff over a crossing message

A cross-region commuter must **cross**: one logical traveler leaves region A at the
border, its **token** is handed to region B and walks to the workplace, where it
**waits**; when the workday ends the token is removed and a **return** message tells A
the traveler is home again — all carried by an explicit **crossing message**.

### 5a. How remote jobs are detected

On master, `workplace_assignment.workplace` is a city-wide `Entity` whose packed
high-32 bits carry the birth region. The travel system's local-vs-remote test is a
one-liner:

```rust
fn local_workplace(citizen: &Citizen, world: &World) -> Option<Entity> {
    citizen.workplace_assignment?.workplace.as_local(world.region_id)
}
```

`as_local` returns `Some(local_entity)` for a local job, `None` for a remote one
(`workplace.region() != world.region_id`).

In P1–P4, a remote worker (`local_workplace` → `None`) idles at home all day. P5
changes this: during work hours, if the workplace is remote **and reachable** (the
citizen's home road network shares a cross-region component with the workplace
region), the target becomes the **border-exit cell** instead of home. Otherwise, the
citizen stays home.

### 5b. Share-nothing: one token, no proxy, entity never migrates

The citizen **entity never migrates**, and there is no separate "proxy" — there is just
the one **token** (the `TravelState` from §2 — `{ status, current_cell, destination, building }`).
What crosses is that token, routed as a `RegionEvent` over the *same* border topology
(`RegionNeighborLink` / `BorderLinkId`) and worker routing that already carry
power/job/goods exports. While the token is visiting B it is the **single
representation** of the away traveler and the only thing drawn as a dot; the home side
keeps no parked movement state — while the token is out it simply does not dispatch or
draw W.

```text
   REGION A  (home — owns W's entity)         border          REGION B  (hosts the token while away)
   ┌─────────────────────────────────┐    RegionNeighborLink  ┌─────────────────────────────────┐
   │ W's token: home ─route─► exit    │       (topology)       │ ReceiveTraveler(token, Outbound)│
   │ on exit cell ─► EMIT ───────────┼──►  TravelerHandoff ──► ┼─► insert token AT entry cell    │
   │   handoff{ token, id, link }    │     (crossing msg,      │   token: entry ─route─► workplace│
   │ remove local token, mark W away │      one-tick-stale)    │   token WAITS at workplace (dot)│
   │   no dot, schedule skips W       │                         │   ── workday ends ──            │
   │                                 │                         │   EMIT return ─┐  remove token  │
   │ ReceiveTraveler(Return) ◄───────┼──  TravelerHandoff   ◄──┼────────────────┘  (dot gone)    │
   │ clear away mark ─► W = AtHome   │     (crossing msg)      │                                 │
   └─────────────────────────────────┘                        └─────────────────────────────────┘
   owns W's entity the whole time;       entity NEVER copied across;    the token IS the dot;
   the token is the only thing that crosses   token is owned + skip      removed on return ⇒ no dot
```

### 5c. The crossing message and its types

The crossing message wraps the token with the routing it needs. On master, `Entity`
already packs the home region, so `TravelerId` is simpler than the earlier draft:

```text
TravelerId {                              // owned data — neighbor never dereferences the Entity
    citizen:   Entity,                    // W's entity; citizen.region() IS the home region
    generation: u32,                      // disambiguates trips; stale Returns ignored
}

TravelerHandoff {                         // a RegionEvent payload, routed by the worker
    token:       TravelState,             // THE token — same type used for local travel
    traveler:    TravelerId,              // round-trip identity (entity + generation)
    to_region:   RegionId,                // routed by RegionNeighborLink, like an export request
    entry_link:  BorderLinkId,            // sender's exit link; receiver maps via matching_neighbor_link()
    return_path: Vec<ReturnHop>,          // stack: push on each outbound hop, pop on each return hop
    purpose:     TravelPurpose,           // Outbound | Return
}

ReturnHop { region: RegionId, entry_link: BorderLinkId }
TravelPurpose { Outbound, Return }
```

`TravelerId.citizen` is an `Entity` whose `region()` is the home region — the neighbor
echoes it back on `Return` so the home region finds exactly which citizen to clear.
The neighbor **never dereferences it as an ECS key**; it is opaque id data. This is the
same trust boundary as `JobExportGrant.workplace: Option<Entity>` (the consumer stores
it but never looks it up in its own `World`).

Two new enum variants, mirroring the existing export plumbing:

- `RegionEvent::ReceiveTraveler(TravelerHandoff)` — inbox event on the receiving region.
- `OutboundMessage::TravelerHandedOff(TravelerHandoff)` — the worker routes it to
  `to_region` by the same `RegionNeighborLink` topology used for job export.

**Key simplification vs. power/jobs/goods:** a handoff is **display-only and
fire-and-forget**. It does **not** pause the tick — there is no `WaitingFor…`
continuation (`TickState` stays at 4 phases: power, power-settlement, jobs, goods), no
grant/deny, no reservation ledger. A emits it and finishes its tick; B consumes it on
its next event pass (**one-tick-stale**, exactly the cross-region boundary the rest of
the model uses). Nothing blocks, so no deadlock surface.

### 5d. Picking the border-exit cell — the worker-provided border-neighbor hint

The home region needs to know **which border link leads toward the workplace's region**
to route the token to the right exit cell. Today a `RegionState` knows its
`network_border_links()` (which road networks touch which `BorderLinkId`s) but **not
which neighbor region is across each border** — that knowledge lives in the worker's
`RegionNeighborLink` topology. The job-export flow sidesteps this: the region emits a
request and the worker routes it. Travel needs the inverse: the region picks a border
cell to walk to, so it needs the neighbor mapping locally.

**Solution:** the worker provides a **border-neighbor hint** to each region, refreshed
alongside `importable_remote_jobs` (the worker already refreshes hints before each
region's processing slice, `worker.rs:501-506`):

```text
border_neighbor_map: HashMap<BorderLinkId, RegionId>
    // "this border link faces this neighbor region"
    // built from the RegionNeighborLink topology the worker already owns
```

**This is layer 2 (the per-region road-level walk).** The v1 border-exit selection
below is a plain `border_neighbor_map` lookup (direct-neighbor map, no Dijkstra) —
it picks *which* border link. The Dijkstra with crossing penalty is the **road walk**
from the current cell to that border-exit cell (step 6 below, using the P2 route
cache). Multi-hop (A→B→C) adds **layer 1** (weighted Dijkstra on the cross-region
component graph, worker-side — edge weights are road-level crossing costs) to know
*which* border link has the lowest road cost to the destination — see §5f.

The travel system's border-exit selection (deterministic):

```text
citizen W has remote workplace in region B (workplace.region() == B):
  1. find W's home road network (the RoadNetwork containing W's home entry cells)
  2. border_links = network_border_links() filtered to W's home network
  3. exit_links  = border_links where border_neighbor_map[link] == B
  4. if exit_links is empty ─► unreachable: stay at current location (§4b)   // v1: direct neighbors only
  5. exit_cell  = the road cell at exit_links[0] (sorted by sort_entities_by_position)
  6. target = exit_cell  // route cache steps the token home ─► exit_cell (Dijkstra, §7b)
```

**v1 P5 scope: direct neighbors only.** If the workplace region directly borders the
home region, commute. If it requires an intermediate (A→B→C), the citizen stays home
(as P1–P4 does for all remote workers). Multi-hop is a stated extension (§5f) — it
requires the worker to provide road-reachability info (which regions are reachable
through each border link's cross-region component), which extends this hint. Deferred
until direct-neighbor commuting is proven.

### 5e. Lifecycle — the round trip (direct-neighbor v1)

The token crosses on the way out and is simply **removed** on the way back (no return
walk — the `Return` is a routing message, not a walking token).

**Operational requirement:** cross-region travel requires `tick_city` (all regions
tick together) so clocks stay synchronized — a host region's `schedule_phase(hour)`
must agree with the home region's. `tick_region` (single region) must not be used
when cross-region tokens are in flight; it can desynchronize clocks and cause a host
to emit `Return` at the wrong hour.

```text
REGION A  (owns W's entity; entity never moves)        REGION B  (hosts the token while away)
────────────────────────────────────────────          ───────────────────────────────────────
09:00 schedule: W has remote job in B (direct neighbor)
       target = border-exit cell toward B              (no token yet)
       (§5d: border_neighbor_map picks the exit link)
token steps home ─► exit cell  (local route cache)
token on exit cell:
  emit TravelerHandoff(Outbound, token, to_region=B,
    entry_link=exit_link,                          ── worker routes ──►  ReceiveTraveler(Outbound):
    return_path=[ReturnHop{A, exit_link}])                                  insert token at entry cell
  remove local token from world.travel                                        (matching_neighbor_link(entry_link))
  mark W Away: world.travel[W] =                                         token steps entry ─► workplace
    TravelState{Away, None, None}                                          (B's route cache)  — drawn as a dot
  (schedule skips Away citizens; no dot)                                  token WAITS at workplace (dot stays)
                                                       ── workday ends (hour ≥ 15) ──
ReceiveTraveler(Return): ◄── worker routes ──            emit TravelerHandoff(Return, traveler,
  return_path empty ─► final hop                             to_region=return_path.last().region,
  clear W's Away mark ─► W = AtHome                          return_path=[] (popped the last hop));
  (next work hour, schedule sends it out again)              REMOVE the token from visiting_travel
                                                             (its dot disappears)
```

**The "away" mark** is a new `TravelStatus::Away` variant. While `Away`, the schedule
skips the citizen (no depart, no step, no dot). The away mark + visiting token are both
`#[serde(skip)]` transient — on load, an away W simply restarts its commute from home.

**The visiting-token map** is a new `World` field:

```text
World {
    travel: HashMap<Entity, TravelState>,            // local citizens (P3, unchanged)
    visiting_travel: HashMap<TravelerId, TravelState>, // tokens handed in by neighbors (P5, #[serde(skip)])
}
```

`travel::run` steps both maps each tick. The adapter's `traveler_views` emits dots from
both (a visiting token's `current_cell` is a road cell in the host region, rendered the
same way).

### 5f. Multi-hop extension — two-layer routing (deferred after v1)

For A→B→C (workplace in C, not a direct neighbor of A), the route is **stitched from
per-region local searches**. There is no single pathfinding algorithm that spans
regions — instead two layers cooperate:

```text
LAYER 1 (region-level): weighted Dijkstra   LAYER 2 (road-level): Dijkstra
  ────────────────────────────────────────     ─────────────────────────────────
  "which regions to cross?"                    "which road cells within this region?"

  nodes  = (region, road_network)              nodes  = road cells
  edges  = NetworkBorderLink pairs             edges  = road adjacency
  edge   = road cost to cross a region         weight = 1 + crossing penalty (§7b)
          (Dijkstra border→border within
          that region, with crossing penalty)
  algo   = Dijkstra on the weighted             algo   = Dijkstra + crossing penalty (§7b)
          component graph                      output = came_from tree (entry → exit/workplace)
  output = A → D → E → C (lowest road cost)
  runs   = worker (RegionDirectory +            runs   = each region's own World (share-nothing)
          border_crossing_cost table)
  exists = component graph YES;                 exists = YES — road_predecessors (P1) + route_cache (P2)
          crossing-cost table NO (new)                  (§7b)
```

```
A ─Dijkstra─► D ─Dijkstra─► E ─Dijkstra─► C   "the route crosses these regions"  (layer 1)
│              │             │              │     (weighted by road cost, not hop count)
Dijkstra       Dijkstra       Dijkstra       Dijkstra  "the road path within each region"  (layer 2)
(A: home→exit) (D: entry→exit) (E: entry→exit) (C: entry→workplace)
```

Neither algorithm ever crosses a region boundary — they hand off via the token
message (§5c).

#### Layer 1: how B knows there's a road B→C

Region adjacency ≠ road connectivity. The worker's `RegionDirectory` already
computes road connectivity across borders: `build_component_graph`
(`directory.rs:219-246`) unions road networks when their border links match,
producing `CrossRegionDiscovery.components: Vec<Vec<RegionRoadNetworkId>>` — each
component is a set of `(region, local_road_network)` pairs that are road-connected
across borders.

```text
   Region A          Region B          Region C
   ┌────────┐        ┌────────┐        ┌────────┐
   │ net 0  │════════│ net 1  │════════│ net 2  │   ← roads cross borders
   │ (home) │  link  │(transit)│  link  │ (work) │      (NetworkBorderLink pairs)
   └────────┘  AB    └────────┘  BC    └────────┘

   component = [A.net0, B.net1, C.net2]   ← one cross-region road component
   "if your road network is in this component, you can drive to any other member"
```

This is already how `importable_remote_jobs` finds reachable job slots. For travel,
extend §5d's `border_neighbor_map` with two worker-precomputed tables:

```text
border_crossing_cost: HashMap<(RegionId, BorderLinkId, BorderLinkId), u32>
    // "road cost to cross region R from border link X to border link Y"
    // = Dijkstra(entry_cell_X → exit_cell_Y) with crossing penalty, within R's roads
    // precomputed by the worker on topology change
    // O(border_links) single-source Dijkstra runs per region (one per entry link;
    // read distances to all exit links) — fine for small border counts

border_route_hint: HashMap<BorderLinkId, HashMap<RegionId, u32>>
    // "through this border link, region X is Y road-cost away"
    // Dijkstra distance on the weighted region-level graph
    //   (nodes = (region, network), edge weight = border_crossing_cost)
    // built by the worker from CrossRegionDiscovery.components + border_crossing_cost
    // (refreshed alongside importable_remote_jobs, same chokepoint)
```

The two layers now share the **same cost model**: layer 1's edge weights are literally
layer 2's Dijkstra distances (with crossing penalty). A motorway crossing (few cells,
no crossings) costs less than a congested-city crossing (many cells, many crossings),
so the region corridor picks the motorway even if it's more hops.

#### Which path when there are multiple? — lowest road cost, deterministic tie-break

When multiple region-level paths exist (e.g. A→B→C and A→D→E→C), the token takes the
**lowest total road cost** path — not necessarily the fewest hops. Each region-level
edge is weighted by the road-level Dijkstra cost (with crossing penalty) of crossing
that region, so a 3-hop motorway path can beat a 2-hop congested path:

```text
   A ── B ── C          2 hops, but B has 10 crossings, narrow roads
   │                    road cost = 12 + 15 = 27
   │
   └── D ── E ── C      3 hops, but D/E are motorways, 0 crossings
                        road cost = 3 + 2 + 3 = 8  ← chosen (lowest cost)
```

The hint carries Dijkstra distances so each region can make a **greedy locally-optimal**
decision: pick the border link with the lowest remaining road cost to the destination.
Greedy with Dijkstra distances is optimal for weighted graphs with non-negative edges
(picking the neighbor with the lowest distance to the target = following the Dijkstra
tree = shortest path). No precomputed forward route needs to be carried in the token —
each region reads its hint and decides.

Ties (same road cost through two links) are broken by **border cell position**
(`sort_entities_by_position`), the same deterministic order used everywhere else in
the pathfinding.

#### How a transit region (B) routes the token

```text
B receives token destined for C:
  1. find B's current road network (the network containing the entry cell)
  2. candidates = B's border links on that network where
       border_route_hint[link].contains_key(C)
  3. if candidates is empty  ─►  unreachable: emit Return (§5g)
  4. best_link = candidates sorted by (route_cost[link][C], border_cell_position)[0]
                                                               ─► lowest road cost, then position
  5. Dijkstra within B: entry_cell ──► best_link's exit_cell (local, B's route_cache, §7b)
  6. hand off to the next region through best_link (push ReturnHop onto return_path)
```

#### The failure case: road torn up in B

```text
   Region A          Region B          Region C
   ┌────────┐        ┌────────┐        ┌────────┐
   │ net 0  │════════│ net 1  │  ✗✗✗  │ net 2  │   ← B↔C road destroyed
   │ (home) │  link  │(transit)│       │ (work) │
   └────────┘  AB    └────────┘        └────────┘

   component splits: [A.net0, B.net1]  and  [C.net2]

   B receives token for C:
     border_route_hint[any link on net1] does NOT contain key C
     ─► unreachable ─► emit Return ─► A clears Away mark ─► W = AtHome (§5g)
     (no dot lingers; the token is removed from visiting_travel)
```

The `CrossRegionDiscovery` is refreshed by the worker on topology change (it already
is — `importable_remote_jobs` depends on it). So a road teardown in B updates the
components, which updates the `border_route_hint`, which makes B correctly detect the
disconnection on its next token-processing tick.

#### Outbound and return

- **Outbound:** each intermediate region receives the token, sees
  `token.destination.region() != self.region_id` (it's transit), and routes to its own
  border-exit toward the destination — picking the link with **lowest total road cost**
  to the destination (tie-break by border cell position). Each hop pushes a `ReturnHop`
  onto `return_path`.
- **Return:** the `Return` message pops one `ReturnHop` per hop. Each intermediate
  region receives the `Return`, routes it one hop back (no walk — it's a routing
  message, not a token). When `return_path` is empty, the home region clears the away
  mark.

#### Why deferred

The pathfinding mechanism (per-region Dijkstra, layer 2) is already built by P1–P2.
The new work for multi-hop is **layer 1**: the `border_crossing_cost` table
(O(border_links) Dijkstra runs per region, worker-side) and the `border_route_hint`
with weighted Dijkstra distances (extending §5d's `border_neighbor_map`), plus the
transit-region logic ("I'm not the destination, route to the next border"). Deferred
until direct-neighbor commuting (v1) is proven.

### 5g. Edge cases

- **Token can't reach the workplace** in B (road torn up mid-trip): B removes the token
  from `visiting_travel` and emits `Return` at once — A clears the away mark and W is
  `AtHome` (an **intentional exception** to §4b's stay-put rule: the token is a
  display-only abstraction, so "removing" it is not a teleport — the citizen was never
  physically in B, only the token was). The next tick's schedule re-derives the intent
  and W retries the commute if the road reconnects. (No dot lingers.)
- **W's remote job ends** while away: the next daily job phase clears
  `workplace_assignment`; with no remote target the schedule keeps W home. A token still
  out in B self-heals on its own unreachable/return path; the `Return` clears the away
  mark. No cross-region "cancel" message needed.
- **Stale `Return`:** accept `Return` only when `(TravelerId.citizen, generation)`
  matches W's current away token; ignore stale returns from an old trip. The
  `generation` counter bumps each time W departs for a remote workplace.
- **B's road network disconnects from the entry cell** while the token is en route to
  the workplace: same as "can't reach" — B removes the token and emits `Return`.

### 5h. Determinism, persistence, balance

- **Determinism:** the handoff is a `RegionEvent` processed in FIFO order like every
  other event; `TravelerId.citizen.region()` IS the home region (packed in the entity,
  no separate field); border-exit cell, entry link, and return-path hops are chosen by
  `sort_entities_by_position`. Same inputs → same crossing.
- **One-tick-stale:** B sees the token the tick after A emits it — the standard
  cross-region latency, not within-tick sync. The handoff does not add a `TickState`
  phase; it rides on the existing event flow.
- **Persistence:** the token in flight, the home "away" mark, and any visiting token a
  neighbor holds are **transient runtime state** (`#[serde(skip)]`), just like local
  travel state. Save/load drops in-flight crossings; on load the schedule rebuilds
  placement from the hour and each citizen's assignments (an away W simply restarts its
  commute). No save-format change.
- **Balance:** the crossing is **display-only**. Salary, workplace tax, and job
  identity stay entirely on the existing job-export flow; the token moves no money and
  feeds no formula. Removing the animation would not change a single economic number.

### 5i. Locating a token across regions — known ceiling (not designed yet)

A token is always in exactly one place: a local `world.travel`, a host region's
`visiting_travel`, or in transit (a `TravelerHandoff` message being routed —
one-tick-stale, not in any World). The home region's `TravelState` only says
`Away, None, None` — it does not store where the token went. **There is no
`away_region` field on `TravelState`** and no per-worker `token_registry`.

**Rendering doesn't need cross-region lookup** — each region renders its own
`world.travel` + `visiting_travel` (the adapter reads the local World only). An away
citizen has no dot on the home side; the host side draws it from `visiting_travel`.

**Cross-region token lookup is unbuilt.** No current UI feature asks "where is
citizen W right now?" across regions, so the lookup mechanism is YAGNI. When a
consumer appears (e.g. inspecting an away citizen from the home region's UI), the
approach is **fan-out on demand** — the runner queries each worker like
`remote_workers_at` (`regional_game_runner.rs:315-350`), each worker scans its
regions' `visiting_travel: HashMap<TravelerId, TravelState>` directly (no registry
needed), and the first hit resolves to `(region_id, current_cell → x, y)`. No hit →
"in transit" (one-tick-stale) or home (not `Away`).

**`TravelerId` must be UI-safe if inspect needs it** — `CitizenDetailView`
deliberately exposes no entity identity (`view.rs:67`), so the lookup key can't be
`TravelerId { citizen: Entity, generation }` as-is. An opaque UI-safe id plus
generation ownership must be defined. This is part of the inspect design, not the
travel design — deferred to that mission.

**Upgrade path** (if a city-wide roster needs *proactive* location without fan-out):
a `TokenForwarded { traveler, new_region }` message emitted on each forward hop so
the home region stays current. YAGNI.

---

## 6. UI surface (thin)

- adapter ─► `CitizenTravelView { x, y }` (no direction/status; identity
  stays in ECS). P5 adds visiting-token dots through the **same** adapter path (a
  visiting token's `current_cell` is a road cell in the host region, rendered the same
  way). **No path/graph leaks to UI.**
- TUI: draw a yellow bold `•·` dot per traveler cell (Normal overlay only; suppressed
  under Power/Pollution/etc). Multiple travelers on a cell → one dot (deduped). The
  cursor highlight is preserved if the cursor is on a traveler cell.
- Cap / sample the number of rendered dots (representative, not 1:1) with a
  `// traffic:` ceiling note, to bound render cost on busy maps. **Not yet implemented**
  (P4 renders 1:1; fine for small maps).
- ASCII fallback does **not** render travelers (TUI-only).

---

## 7. Phase split (each its own tested city-dev patch)

- **P1 — pathfinding (weighted Dijkstra with crossing penalty).** Add
  `road_predecessors(world, network, sources) -> HashMap<Entity, Entity>` to
  `systems/road_network_analysis.rs` — a destination-rooted, multi-source
  **Dijkstra** search (not plain BFS) that records `came_from` for path
  reconstruction. Because the search is destination-rooted (sources = the
  destination's entry cells, expand outward), the edge weight for relaxing
  `current → neighbor` is **`1 + crossing_penalty(current)`** — the cost to
  enter `current` in the forward direction `neighbor → current` (toward the
  destination). `crossing_penalty = 2` if the road cell has **> 2 road
  neighbors** (T-junction or 4-way intersection), else `0`. This makes a path
  with fewer crossings beat one with more at the same hop count. Min-heap
  (`BinaryHeap<Reverse<(u32, Entity)>>`) with **entity-id tie-break** for
  determinism (BinaryHeap doesn't guarantee pop order for equal priorities;
  the entity id orders equal-cost heap pops deterministically; for unequal
  costs the cost itself orders the heap — the entity id only matters as a
  secondary tie-breaker when two cells have the exact same `dist` value). It
  does not *directly* select parents, but it determines which equal-cost
  relaxation is *recorded first* — and strict `<` then preserves that first
  parent. No cache, no movement, no behaviour change.
  `road_distances` (existing) stays plain BFS — its output feeds economy
  metrics, so no penalty there. Unit-test determinism: identical reconstructed
  route every run; on equal-cost routes the entity-id tie-break orders heap
  pops deterministically (lower entity id pops first); fewer crossings wins
  on equal-hop routes; an unreachable origin/disconnected road cell is
  absent from `came_from`.
- **P2 — region route cache.** Region-owned
  `RefCell<HashMap<(dest, road_network_id), HashMap<Entity,Entity>>>` derived cache;
  **chokepoint-specific invalidation (§3)**: coarse `clear()` on a road change,
  per-destination eviction on a building change (no `route_epoch`). Tests: reuse
  hits when fresh; a road mutation coarse-clears the cache (or a building mutation
  evicts only that building's entry); a recompute produces the same tree as a direct
  Dijkstra.
- **P3 — movement sim + schedule.**
  `world.travel: HashMap<Entity, TravelState>` (`#[serde(skip)]`); commute-only
  schedule (09–15 work, else home); travelers step one cell/tick via `came_from`;
  degenerate target → stay at current location (§4b); **remote workers idle at home**; `travel::run`
  prunes dead citizens each tick. Tests: at 09:00 a resident departs home and steps
  the exact route cells to its workplace, idling there until 15:00; an unreachable
  workplace keeps the citizen at home (no route to depart); an unreachable home
  keeps a stranded citizen at work after 15:00 (no route home, §4b stay-put);
  deterministic across runs.
- **P4 — view model + UI dots.** adapter
  `traveler_views` → `CitizenTravelView { x, y }`; TUI renders a yellow bold `•·` dot
  per traveler cell (Normal overlay only). No direction/facing. Tests: a moving
  traveler appears on its current cell; empty roads show none.
- **P5 (optional) — cross-region token handoff (§5).** The real crossing:
  route a cross-region commuter's token to its border-exit (using a worker-provided
  `border_neighbor_map` hint, §5d), then **hand the token off** over a new
  `RegionEvent`. Add `RegionEvent::ReceiveTraveler` +
  `OutboundMessage::TravelerHandedOff`, worker routing by `RegionNeighborLink`, a
  `visiting_travel: HashMap<TravelerId, TravelState>` map so the neighbor steps the
  *same* token type entry-cell → workplace, and a `TravelStatus::Away` mark on the
  home side so it stops dispatching/drawing W. Workday-end removes the token (dot
  gone) and emits `Return`, which clears the away mark. Fire-and-forget (no tick
  pause, no `TickState` phase, no grant). **v1: direct neighbors only; multi-hop
  deferred (§5f).** Tests: a token handed off at A's exit cell arrives at B and steps
  entry→workplace; `Return` clears the away mark so W is home; an unreachable token
  returns immediately; stale `Return` (generation mismatch) is ignored; round-trip is
  deterministic.
- **P6 (optional, parallel).** Aggregate per-cell **traffic load** + `Traffic`
  overlay as a second consumer of P1 routes (the old heatmap); and/or the cosmetic
  sub-cell movement tween. Gameplay coupling (congestion → commute penalty / land
  value) is a further, separately balanced mission.

Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test -q` after each.

### 7b. Per-patch architecture

Layers: `core` (sim/ECS) → `interface` (adapter + view models) → `ui` (TUI). Each
patch names which layers it touches, the data flow, and the additions.

**P1 — predecessor Dijkstra (crossing-penalty weighted)** · layer: **core only** · `systems/road_network_analysis.rs`
```
road graph ─► road_predecessors(world, network, sources) ─► HashMap<Entity,Entity>  (came_from)
              dest-rooted, multi-source Dijkstra (NOT plain BFS):
                queue  = BinaryHeap<Reverse<(u32 cost, Entity)>>   (min-heap)
                weight = 1 + if road_degree(current) > 2 { 2 } else { 0 }   (crossing penalty)
                         // destination-rooted: relaxing current→neighbor represents
                         // the forward step neighbor→current (toward destination);
                         // the penalty charges the cell being entered, which is
                         // `current` in the reverse direction.
                tie    = entity id in heap key (BinaryHeap has no FIFO guarantee;
                         lower entity id pops first on equal cost → deterministic
                         heap pop order; it does NOT *directly* select parents,
                         but it determines which equal-cost relaxation is
                         recorded first, and strict < then preserves that parent)
                pop    = pop min (cost, current) from the heap
                skip   = if cost != dist[current] { continue; }   (stale-heap entry; ignore)
                relax  = if nd < dist[neighbor]  (strict <: a strictly lower distance
                         replaces came_from; an equal-distance relaxation does NOT
                         replace — the existing parent stays. Equal-cost parent
                         outcome is therefore determined by which relaxation is
                         *recorded first*; the entity-id heap order indirectly
                         sets that, and strict < then preserves it)
                record came_from[neighbor] = current  (on strict distance improvement)
              road_distances (existing) stays plain BFS — economy metrics, no penalty
```

**Concrete Rust signature:**

```rust
pub(crate) fn road_predecessors(
    world: &World,
    network: &RoadNetwork,
    sources: &[Entity],
) -> HashMap<Entity, Entity>
```

**Test cases:**

- **Determinism** — call `road_predecessors` twice with the same input on a road
  network with equal-cost paths (two parallel routes between A and D) to exercise
  the entity-id tie-break. Use `assert_eq!` on the two returned `HashMap`s; both
  calls must produce structurally equal `came_from` trees. Additionally, assert the
  expected canonical parent for one ambiguous cell (e.g. on a path where two
  equal-cost parents are possible, the lower-entity-id source must win), so the
  test actually proves tie-breaking — not just that two runs are equal.
- **Crossing penalty** — build a road network with two equal-hop paths between A
  and D: one through a 4-way intersection (the crossing cell charges +2) and one
  through a straight road (zero crossings, all cells charge +1). The crossing
  path costs **+2 more than** the straight path (the +2 is the single crossing
  cell's penalty). Assert the returned `came_from` tree routes through the
  straight road (lower cost). (Absolute totals depend on the exact path shape
  — any two distinct equal-hop A→D paths must share a divergence and a
  convergence cell, which may themselves be degree >2 — so the test asserts
  the **relative** cost, not exact totals.)
- **Unreachable / empty sources** — `RoadNetwork` is a connected component by
  construction, so it can't contain a "disconnected island." Test empty/foreign
  sources: pass `sources = &[]` and assert the returned tree is empty (no cells
  were ever reached). The destination source itself is always absent from
  `came_from` (roots have no parent); only unreachable *non-source* cells are
  the test signal.
- **Cross-network filtering** — build two disconnected road networks. Pass
  `sources` that includes a cell from network 1 and, separately, pass `sources`
  that includes a cell from network 2 (with the function called on the wrong
  network). Assert that a source from network 2 is ignored when called on
  network 1, and vice versa. This proves the `network.roads.contains(...)` filter
  works — the test fails if a foreign-network source is accepted.
- **Multi-source** — pass two destination entry cells as `sources` (e.g. a
  destination building touching two different road cells). Assert both sources
  are absent from `came_from` (roots have no parent). For an ambiguous cell
  that is **directly adjacent to both sources** (so it's the only cell that
  could be reached by either source in one step) with equal cost and equal
  penalty, assert the canonical parent is the lower-entity-id source
  (deterministic tie-break). If the cell is not directly adjacent to both
  sources, its immediate parent need not be a source — it would be an
  intermediate cell whose parent is whichever source the search reached first.

Pure function, no state, no other layer. Layering: this is a pure-function patch
that adds a public-crate function alongside the existing `road_distances` (which
stays untouched). No changes to `World`, `region`, or the UI layer.

**P2 — route cache (chokepoint-specific invalidation)** · layer: **core** ·
`world.rs` (cache itself) + `systems/placement.rs` (creation dispatch) +
`systems/entity_cleanup.rs` (removal dispatch) + `systems/upgrade.rs`
(footprint-growth eviction)
```
        build/bulldoze/replace/upgrade ─► dispatch (§3):
            create  (placement::place_building)  ─► road → coarse clear;
                                                     building → no invalidation
            remove  (entity_cleanup::remove_entity)─► road → coarse clear;
                                                     building → per-destination eviction
            upgrade (upgrade::grow_to_level)      ─► per-destination eviction for
                                                     the surviving building
            power/pop etc                         ─► (no route cache invalidation)
caller ─► routes_to(world, dest, network):
             cache[(dest,network)] exists?  ─► reuse came_from
             else                           ─► road_predecessors() (Dijkstra, §7b P1) + insert
World { + #[serde(skip)] route_cache: RefCell<HashMap<(Entity, u32), HashMap<Entity,Entity>>> }
```
Cache key is `(dest, network_id)` — the crossing penalty is a compile-time const, so it
doesn't enter the key. (If the penalty ever becomes dynamic, e.g. congestion in P6, it
must enter the key or the cache serves stale trees.)

**Destination roots:** `routes_to` uses the building's adjacent road cells as Dijkstra
sources. For P5 border-exit routing, `dest` is a **road cell** `Entity` (not a
building), so `routes_to` must treat it as a single-element source list `[dest]`
when `dest` is a road entity, instead of looking up adjacent roads. Test: routing to
an explicit road cell produces a came_from tree rooted at that cell.
Chokepoint-specific invalidation (§3): coarse clear on road change, per-destination
eviction on building change. Test: fresh reuse; road change forces recompute.

**P3 — movement + schedule** · layer: **core only** · `components.rs`, `systems/travel.rs`, `world.rs`
```
tick (after happiness) ─► travel::run(world):
    for each citizen (sorted by entity.0):
        intent  = schedule_intent(hour, citizen)             // schedule → citizen-schedule-plan.md
        target  = resolve_intent(intent, world)               // movement-side: Home→home, Work→local/border-exit, Leisure→deferred
        local   = local_workplace(citizen, world)            // workplace.as_local(world.region_id)
        // local = Some  ─► route to local workplace
        // local = None  ─► remote: idle at home (P3); route to border-exit (P5)
        depart  = target changed ─► current_cell = building entry cell
        step    = current_cell = routes_to(target)[current_cell]  // O(1)
        unreachable ─► stay at current location                  // §4b (no teleport)
    prune dead citizens from world.travel
World { + #[serde(skip)] travel: HashMap<Entity, TravelState { status, current_cell, destination, building }> }
```
No interface/ui yet. Test: 09:00 home→work exact cells; unreachable→stay put; remote worker stays home.

**P4 — view model + dots** · layer: **interface + ui** · `view.rs`, `adapter.rs`, `ui/tui.rs`
```
core TravelState ─► adapter::traveler_views ─► Vec<CitizenTravelView{ x,y }> on GameView
                      (reads world.travel current_cell; deduped, sorted; no path/graph leaks)
                  ─► tui: per traveler cell, draw a 2-col yellow bold dot '•·' (Normal overlay only)
```
UI reads the view model only. Test: a traveler shows on its cell; empty roads → none.

**P5 (optional) — cross-region token handoff** · layer: **core + regions** (+ reuses P4 UI) · `components.rs`, `systems/travel.rs`, `regions/{mod,runtime,worker,threaded}.rs`
```
worker: provide border_neighbor_map hint to each region (refreshed like importable_remote_jobs)
A travel::run: W has remote workplace in B (direct neighbor):
     target = border-exit cell (§5d: border_neighbor_map + sort_entities_by_position)
     token reaches exit cell ─► OutboundMessage::TravelerHandedOff(Outbound, token, return_path=[hop_A])
     remove local token + mark W Away (TravelStatus::Away; schedule skips, no dot)
worker: route handoff to to_region by RegionNeighborLink   (same routing as job export)
B runtime: RegionEvent::ReceiveTraveler ─► insert token into visiting_travel at entry cell
B travel::run: step visiting token entry─►workplace (B's route cache); WAIT at workplace (dot stays)
              workday ends (hour≥15) / unreachable ─► emit Return; remove visiting token (dot gone)
A runtime: ReceiveTraveler(Return) with empty return_path ─► clear W's away mark ─► W = AtHome
World { + #[serde(skip)] visiting_travel: HashMap<TravelerId, TravelState> }
TravelerId { citizen: Entity, generation: u32 }   // citizen.region() IS home region
all transient; fire-and-forget (no TickState pause, no grant); v1 direct-neighbor only
  multi-hop (A→B→C): two-layer routing — weighted Dijkstra on component graph (road-cost edges) + per-region Dijkstra (§5f, deferred)
adapter: visiting tokens feed the SAME traveler_views path as P4 (one token type, one dot renderer)
```
Test: token handed off at A.exit arrives at B and steps entry→workplace; Return clears away mark; unreachable token returns; stale Return (generation mismatch) ignored; round-trip deterministic.

**P6 (optional, parallel)** · layer: varies
```
heatmap:  core LocalEffects.traffic (aggregate P1 routes) ─► adapter CellView.traffic
          ─► ui MapOverlayInput::Traffic (reuse intensity_tile)
tween:    ui-only sub-cell interpolation off anim_frame between two core positions
```
Each a separate mission; heatmap is a 2nd consumer of P1, tween is pure UI.

---

## 8. Risks / guardrails

- **Determinism (top risk):** `BinaryHeap` does **not** guarantee pop order for
  equal-priority entries — unlike BFS's FIFO queue, Dijkstra needs an explicit
  **entity-id tie-break** in the heap key (`(Reverse(cost), Reverse(entity))`) so
  equal-cost routes resolve deterministically (entity-id orders heap pops,
  which indirectly determines which equal-cost parent is recorded first;
  strict `<` then preserves it). Combined with stable traveler iteration, this
  gives identical `came_from` every run. Add a replay/parity test once
  movement exists. Travel state must not introduce float math.
- **Performance:** destination-rooted Dijkstra keeps pathfinding near-linear; the route
  cache clears wholesale on a road change and per-destination on a building change
  (no per-entry epoch). Cap rendered dots.
  Note any heuristic ceiling in a `// traffic:` comment.
- **Layering:** core computes status/position; the UI renders the view model
  (`CitizenTravelView { x, y }`) only and never reads ECS or paths.
- **Cross-region:** a real token handoff (§5) over a new `RegionEvent`, routed by the
  existing border topology. The entity never migrates and there is no separate proxy —
  the *same* token is handed to the neighbor's `visiting_travel` map. The token is
  owned/`#[serde(skip)]`, fire-and-forget (no tick pause, no `TickState` phase, no
  grant), one-tick-stale — so it adds no deadlock surface and no economic coupling.
  **v1: direct neighbors only** (§5d); multi-hop uses two-layer routing — weighted
  Dijkstra on the cross-region component graph (edge weights = road-level crossing
  costs) + per-region Dijkstra (§5f, deferred).
- **Balance:** movement/animation through P5 is display-only — it must feed no
  economy/happiness formula. Gameplay coupling is a deliberately separate, balanced
  mission (P6+).

## 9. Decisions locked (for review)

- Route table is **region-owned**, keyed by `(destination, road_network_id)` (not
  per-citizen, not city).
- Per-citizen state is just `{ status, current_cell, destination, building }`; movement steps
  in O(1) via the region's `came_from` (no stored path, no re-walk).
- Dirtying via a **chokepoint-specific strategy** (§3): **coarse `route_cache.clear()`**
  on a **road** change (any tree might be affected); **per-destination eviction** on a
  **building** change (only the affected building's entries). No `route_epoch`.
  **Building removal** self-heals via the per-tick schedule recompute + per-destination
  eviction + §4b (no explicit §3a scan).
- **Daily schedule** → defined in `docs/citizen-schedule-plan.md` (commute-only v1:
  09–15 work, else home; free-time/leisure deferred).
- **Unreachable target → stay at current location** (no teleport) (§4b). A remote
  workplace in a non-direct-neighbor region is unreachable in v1 → stay put.
- Movement is **deterministic transient core runtime state**, **tick-cadence** v1
  (stepped is accepted; smooth tween deferred to P6), **`#[serde(skip)]`** on save
  (load places by schedule). **No direction/facing** — P4 draws a plain dot.
- **`road_predecessors` uses weighted Dijkstra** (not plain BFS) with a **crossing
  penalty**: destination-rooted, so when relaxing `current → neighbor` (representing
  the forward step `neighbor → current` toward the destination), the edge weight
  is `1 + if road_degree(current) > 2 { 2 } else { 0 }` — the penalty charges
  the cell being entered in the forward direction. A crossing is a road cell
  with > 2 road neighbors (T-junction or 4-way). This makes paths prefer
  fewer crossings on equal-hop routes. Min-heap with **entity-id tie-break**
  for determinism. **`road_distances` (existing) stays plain BFS** — its
  output feeds `commute_distance`/`nearest_shop_distance` → economy/happiness, so the
  penalty is scoped to the travel/movement system (display-only) to avoid balance
  changes. The penalty is a compile-time const; if it becomes dynamic (congestion in
  P6) it must enter the route cache key.
- **Remote detection** uses `workplace.as_local(world.region_id)` — `Some` = local,
  `None` = remote (master's `Entity(u64)` model).
- **Cross-region uses a real token handoff** (§5): the citizen entity never moves —
  movement is always the **token** (the same `TravelState` used locally; no separate
  proxy). At the border-exit (picked via a worker-provided `border_neighbor_map` hint,
  §5d) the token **crosses on a new `RegionEvent`** routed by the existing topology;
  the neighbor steps that token to the workplace and it **waits** there. The home side
  marks W `Away` (no dot, not dispatched). When the workday ends the token is
  **removed** (dot gone) and a `Return` message clears the away mark — no return walk.
  Fire-and-forget, one-tick-stale, display-only. `TravelerId = { citizen: Entity,
  generation: u32 }` — `citizen.region()` IS the home region. This is P5. **v1:
  direct neighbors only; multi-hop deferred (§5f).**
- **Cross-region routing is two-layer** (§5f): **weighted Dijkstra** on the
  cross-region component graph (`CrossRegionDiscovery.components`, already exists in
  `RegionDirectory`) picks the region corridor — **lowest total road cost** when
  multiple paths exist (a 3-hop motorway beats a 2-hop congested path), edge weights
  are road-level Dijkstra distances with crossing penalty (precomputed in a
  `border_crossing_cost` table, worker-side), tie-break by border cell position;
  **Dijkstra** with crossing penalty (§7b, per-region, share-nothing) walks each
  road-level segment. Both layers share the same cost model (crossing-penalty
  Dijkstra). Neither algorithm crosses a region boundary — they hand off via the
  token message. A `border_route_hint` with weighted Dijkstra distances (extension of
  §5d's `border_neighbor_map`) tells each region which border link has the lowest
  road cost to the destination. Deferred until v1 is proven.
- **Token location across regions** (§5i): **unbuilt — known ceiling.** No
  `away_region` on `TravelState`, no `token_registry`. Rendering doesn't need it
  (per-region rendering). When an inspect feature needs it, fan-out on demand — the
  runner queries each worker like `remote_workers_at`, each scans its `visiting_travel`
  directly. YAGNI. Upgrade path: `TokenForwarded` message for proactive location.
- Animation/movement is **display-only** until a separate gameplay-coupling mission.

---

## Target architecture (P1–P4)

```text
CORE (deterministic)                                    UI
───────────────────────────────────────────────       ──────────────────────────
P1 road_network_analysis::road_predecessors             P4 render_map overlay:
     dest-rooted Dijkstra came_from  ───────────┐          for each view.travelers cell
     (crossing-penalty weighted)                 │          draw a 2-col dot '•·'
P2 World::routes_to(dest, network) ◄───────────┘             (Normal overlay)
     RefCell route_cache, keyed (dest, net id)                    ▲
     cleared on road change / per-destination on building change    │
                  ▲                                                │
P3 systems::travel::run(world)  (tick, after happiness)     P4 adapter::traveler_views
     schedule_intent(hour): Work/Home intent                     GameView.travelers
     resolve_intent → target (Home→home, Work→local/border-exit) = live Traveling cells
     step current_cell = routes_to(dest)[current_cell]           (deduped, sorted)
     unreachable/no-shared-net -> stay at current location (no teleport)
     remote workers idle at home (local_workplace -> None)           ▲
     prune dead citizens from world.travel                            │
     writes world.travel: HashMap<Entity, TravelState> ───────────────┘
```

- **P1** is a pure function; **P2** a region-owned derived cache (mirrors
  `ResourceRegistryCache`); **P3** the only writer of `world.travel`, run each tick;
  **P4** the only reader, through the adapter — the UI never sees paths or the graph.
- Determinism: fixed citizen order + deterministic Dijkstra (entity-id tie-break) +
  integer hours. Display-only:
  `world.travel` is read by no other system, so zero economy/balance impact.

## Design decisions (baked in from the start)

These are deliberate upfront simplifications. Each names the ceiling and the upgrade
path if profiling ever demands more.

- **`TravelState` in a `world.travel: HashMap<Entity, TravelState>` map**, not a
  `Citizen` field — per the existing `Citizen` doc-note ("future movement/pathfinding
  state should remain in separate reusable components instead of growing this
  record").
- **Commute-only schedule** (09–15 work, else home); the 15–22 "free time" /
  leisure→commercial hop is a later add. Two targets, two hour boundaries.
- **No direction/facing** — P4 draws a plain `•·` dot, not an arrow. A 4-way
  orthogonal enum is the upgrade path if the stepping looks too flat.
- **Chokepoint-specific route invalidation** (§3): coarse `clear()` on a road change,
  per-destination eviction on a building change — **two shared dispatch points plus one
  explicit `upgrade::grow_to_level` eviction** (not in `World`'s entity-kind-agnostic
  chokepoints):
  - `placement::place_building` — creates topology; branches on `kind` (road →
    coarse clear, building → no invalidation — a new building is a cache miss,
    not a stale entry).
  - `entity_cleanup::remove_entity` — removes topology; branches on saved `kind`
    (road → coarse clear, building → per-destination eviction). Called by
    `bulldoze`, `replace`'s demolish step, and `grow_to_level`'s absorbed-
    neighbour removal.
  - `upgrade` additionally evicts the surviving building's entries (its
    footprint grew, its entry roads changed).
  No `route_epoch` (a single global epoch is strictly worse than wholesale
  clear). Per-cell scoped eviction is the upgrade path only if a profiler says
  routing recompute is hot.
- **Building-removal self-heals** — no explicit §3a scan at the removal chokepoint. A
  removed destination self-heals via the per-tick schedule recompute + §3 per-destination
  eviction + §4b (unreachable → stay put). `travel::run` prunes dead citizens each tick; the adapter
  filters to live ones. Upgrade path: add an O(citizens) reverse scan at the chokepoint
  if removal-frequency × citizen-count makes the per-tick re-derive hot.
- **Remote detection via `as_local()`** — `workplace.as_local(world.region_id)` returns
  `Some` (local) or `None` (remote). The packed `Entity` carries the region.
- **Weighted Dijkstra with crossing penalty** (not plain BFS) for `road_predecessors`
  — destination-rooted, so when relaxing `current → neighbor` (representing the
  forward step `neighbor → current` toward the destination), the edge weight is
  `1 + if road_degree(current) > 2 { 2 } else { 0 }` (penalty charges the cell
  being entered in the forward direction). A "crossing" is a road cell with > 2
  road neighbors (T-junction or 4-way); the penalty (`+2`) makes paths prefer
  fewer crossings on equal-hop routes. Min-heap with entity-id tie-break for
  determinism (BinaryHeap has no FIFO guarantee). **Scoped to the travel system
  only** — `road_distances` (economy metrics) stays plain BFS to avoid balance changes.
  Ceiling: the penalty is a compile-time const; if it becomes dynamic (e.g.
  congestion-weighted in P6), it must enter the route cache key `(dest, network_id,
  penalty_version)` or the cache serves stale trees.

---

## 9. P2 implementation record (route cache)

P2 was implemented in commit `9dcd774..P2`. The patch is **standalone**: the
cache and chokepoints are wired, but `World::routes_to` is not yet called by
the movement system (that's P3).

### 9a. What the patch added

- **New file** `src/core/systems/route_cache.rs`:
  `RouteCache` struct (`RefCell<HashMap<(Entity, u32), HashMap<Entity, Entity>>>`)
  with `clear()`, `evict(dest)`, `get_or_compute(key, compute)`, and a
  test-only `contains(key)` helper.
- **`World` field** `route_cache: RefCell<RouteCache>` — `#[serde(skip, default)]`
  because the cache is derived state; a freshly-loaded world starts empty
  and the first access recomputes.
- **`World::routes_to(dest, network) -> Ref<'_, HashMap<Entity, Entity>>`** —
  the accessor. Picks sources (building → adjacent roads in network;
  road entity → `[dest]`), looks up or computes the tree via
  `get_or_compute`, returns a `Ref` to the cached tree.
- **`World::clear_route_cache()` / `evict_route_cache(dest)`** — the two
  invalidation methods called from the chokepoints.
- **Chokepoints** (the only places that touch the cache from the command
  layer):
  - `placement::place_building` — road → `clear_route_cache`; building → no
    invalidation (cache miss on first access).
  - `entity_cleanup::remove_entity` — road → `clear_route_cache`; building
    → `evict_route_cache(entity)`. Dispatched on the pre-removal kind
    (read before the building record is dropped). Two identical match arms
    (fallback path + normal path) because the fallback may lack an
    `entities` record.
  - `upgrade::grow_to_level` — after the footprint changes,
    `evict_route_cache(surviving_entity)`. Absorbed neighbours each go
    through `entity_cleanup::remove_entity` and fire their own building-evict.
- **11 tests** in `route_cache::tests`:
  - `RouteCache` direct: `clear`, `evict`, `get_or_compute` closure
    invocation count.
  - `World::routes_to`: building-as-dest, road-as-dest (P5), determinism,
    wrong network (edge case).
  - Chokepoints: road placement → coarse clear; building bulldoze →
    per-destination evict (selective, verified by key presence before
    and after); road bulldoze → coarse clear; upgrade footprint growth →
    per-destination evict.

### 9b. Why two chokepoints in `entity_cleanup::remove_entity`

The function has a fallback path for entities that lack an `entities`
record (the legacy `remove_from_all_component_maps` branch). Both paths
dispatch the route cache invalidation because the route cache doesn't
care about the `entities` record — it only cares about the pre-removal
building kind. The two match arms are intentionally identical (not
extracted to a closure) because the dispatch is four lines and a closure
would obscure the invariant.

### 9c. Reentrancy invariant in `routes_to`

`get_or_compute` holds a `borrow_mut` on the `RefCell` while the closure
runs. The closure is `road_predecessors`, which today is a pure graph walk
that never touches `route_cache` — so the reentrancy is safe. A comment
in `routes_to` pins the invariant: "compute must not touch route_cache."
If `road_predecessors` ever needs to read the cache (e.g. for a
congestion-aware penalty that looks at a cell's current traffic), the
borrow pattern must be restructured (e.g. compute the tree into a local
variable, then insert into the cache after releasing the borrow).

### 9d. Negative-result caching

A call to `routes_to(dest, network)` with a disconnected or wrong
network caches an empty tree under `(dest, network.id)`. The cache
stores negatives intentionally — the tree is the correct "no route"
answer for the current topology. A subsequent chokepoint (road change)
will clear it, and a subsequent call with the correct network id will
compute a fresh tree. This is the expected behavior: don't recompute
empty trees on every tick.

### 9e. ASCII diagram — what P2 is

```
+-----------------------+      +------------------------+
|   World::routes_to    |      |   RouteCache (P2)      |
|   (production API)    | ---> |   RefCell<HashMap<     |
|   returns Ref<...>    |      |     (dest, net_id),    |
+-----------------------+      |     came_from tree>>   |
         |                     +------------------------+
         |                              ^
         |                              | get_or_compute
         |                              |
         v                              |
+-----------------------+               |
|  road_predecessors    |  on miss ───> |
|  (P1, deterministic)  |               |
+-----------------------+               |

Chokepoints (the only places that touch the cache):
  placement::place_building      road ──> clear
                                  building ──> (none)
  entity_cleanup::remove_entity  road ──> clear
                                  building ──> evict(dest)
  upgrade::grow_to_level          surviving ──> evict(entity)
                                  absorbed   ──> (via remove_entity above)
```

### 9f. ASCII diagram — the problem P2 solves

```
Without route cache (P0):
  09:00 home → work:  Dijkstra for citizen A (10 ms)
  09:00 home → work:  Dijkstra for citizen B (10 ms)  ← same path!
  09:00 home → work:  Dijkstra for citizen C (10 ms)  ← same path!
  ...15 citizens, 1 shared workplace, 1 shared network...
  Total: 150 ms per tick for routing. Wasteful.

With route cache (P2):
  09:00 home → work:  citizen A → cache miss → Dijkstra (10 ms) + insert
  09:00 home → work:  citizen B → cache HIT (O(1) HashMap lookup)  (0.01 ms)
  09:00 home → work:  citizen C → cache HIT (O(1) HashMap lookup)  (0.01 ms)
  ...15 citizens...
  Total: ~10 ms per tick for routing. 15x speedup.

Topology change (e.g., new road):
  chokepoint → coarse clear
  next access: cache miss → Dijkstra (10 ms) + insert
  subsequent: cache hit
```

### 9g. Determinism and cross-region notes

- **Determinism**: `RouteCache` is a pure function of `(road topology,
  destination, network_id)`. The cache key is `(dest, network.id)` where
  `network.id` is discovery-order-deterministic (from P0's
  `discover_road_networks`). `get_or_compute` always runs the same closure
  for the same key, so two regions with the same topology produce the
  same trees.
- **Cross-region**: not relevant for P2. The cache lives on `World` (one
  per region), and P2 doesn't touch the region layer. P5 (cross-region
  token handoff) will use `routes_to` with a road-entity destination (the
  border-exit cell) — already supported by the road-as-source path.
- **One-tick staleness**: not introduced. The cache is invalidated at the
  same chokepoints as the resource registry, so cross-region effects
  remain one-tick-stale (the target snapshot model).

---

## Implemented — P3 (movement sim + schedule) · `systems/travel.rs`

`travel::run(world)` is wired into the tick at `simulation.rs:199` (after
`happiness::run` at `:196`, before `turn += 1` at `:200`). It is the consumer the
P-schedule layer was built for: the schedule answers *what* a citizen wants, this
system walks them there one road cell per tick over the P2 route cache. Core-only;
no interface/ui (that is P4).

### The per-tick state machine (per citizen, sorted by `entity.0`)

```text
  intent = schedule_intent(hour, citizen)        // schedule.rs (P-schedule)
  target = resolve_target(intent):               // movement-side resolution
             Home/Leisure          → citizen.home
             Work(wp)              → wp.as_local(region) ? local workplace : home
                                       (remote idles at home in v1; P5 → border-exit)
  state  = world.travel[citizen]  (or default: AtHome, no cell, no building)

  ┌─ current_cell = None  (idle in a building) ──────────────────────────────┐
  │   origin = state.building  IF it still exists  ELSE home   (§4b exception)│
  │   origin == target ?  → idle(arrived_status, target)        (stay put)    │
  │   else                → depart(origin → target):                          │
  │        first position-sorted entry road of origin that can reach target   │
  │          reachable → Traveling, current_cell = that entry road            │
  │          none       → stay idle at origin             (§4b, no teleport)   │
  └───────────────────────────────────────────────────────────────────────────┘
  ┌─ current_cell = Some(cell)  (en route) ──────────────────────────────────┐
  │   cell adjacent to target ? → arrive: idle(AtHome/AtWork), cell = None    │
  │   else                       → step: current_cell = came_from[cell]       │
  │          unreachable          → stay on the same cell  (§4b, no teleport)  │
  └───────────────────────────────────────────────────────────────────────────┘

  after the loop: prune world.travel entries whose citizen no longer exists
```

### `TravelState` — why the `building` field exists

```text
  TravelState { status, current_cell, destination, building }   (components.rs, Copy)
    idle:      current_cell = None,        building = Some(b)   (inside building b)
    en route:  current_cell = Some(road),  building = None      (on that road cell)
```

The departure **origin is read from `state.building`** — the building actually
occupied — *not* re-inferred from the (mutable) workplace assignment. Re-inferring
would teleport a citizen whose assignment changed while idle (origin would jump to
the new workplace), or strand one whose assignment cleared. The
`reassigned_worker_departs_from_old_workplace` test pins this.

### Two distinct "can't move" cases

```text
  §4b stranded   : origin EXISTS but no road route → STAY PUT (retry on reconnect)
  §4b exception  : origin DESTROYED (bulldozed)    → return HOME (no location to hold)
```

`state.building.filter(|b| world.buildings.contains_key(b)).unwrap_or(home)`
collapses the destroyed-origin case to a home fallback; capturing the building's
adjacent road at the removal chokepoint (the teleport-free alternative) would
couple movement into `entity_cleanup`, which isn't worth it for a corner case.

### Determinism / persistence / balance

Citizens visited in `entity.0` order; networks discovered deterministically;
`routes_to` is the deterministic P1 tree; entry cells sorted by position. Movement
is transient `#[serde(skip)]` display/derived state — not saved; the first tick
after load re-derives placement from the schedule. No economy/resource mutation
(the route cache is read, never written here), so balance-neutral.

### Reviewed via `claude-city-dev`

codex (`reviewer`) over three rounds — fixed a medium origin-teleport/strand bug
(added the `building` field), the destroyed-origin void-strand (home fallback +
documented §4b exception), and stale plan line numbers; then opencode
(`ses_108ae15e8ffel8UmUzUFJQ94IF`) clean ("ship it"), plus a test-helper
readability fix (`set_hour` now absolute). 9 travel tests; gates green.

---

## Implemented — P4 (view model + moving-citizen dots) · `view.rs`, `adapter.rs`, `ui/tui.rs`

Renders the P3 travel state as dots on the map. Pure read path: the core never
changes; the adapter turns `world.travel` into a presentation-agnostic marker
list, and the TUI draws it.

```text
  world.travel (P3) ──► adapter::traveler_views(world) ──► Vec<CitizenTravelView{ x,y }>
     │  for each (id, state):                               on GameView.travelers
     │    keep if world.citizens.contains_key(id)   ← stale-dot guard (removed-but-
     │    take state.current_cell (en route only)            not-yet-pruned citizen)
     │    map cell → world.positions → (x, y)
     │  sort + dedup (shared cell → one marker; deterministic)
     ▼
  ui/tui.rs render_map:
     traveler_cells = HashSet of (x,y)   ── built ONLY when overlay == Normal  (perf guard)
     per cell: overlay_traveler_dot(&mut glyph, overlay, has_traveler, is_cursor)
                 draws 2-col yellow bold "•·", keeps cell bg               (correctness guard)
                 gated: Normal overlay AND not the cursor cell
```

### Why the shape

- **`CitizenTravelView { x, y }` only** — no entity id, status, heading, or
  destination leaks to the UI (P4 is a plain dot; facing deferred). The view model
  stays a pure coordinate list.
- **Live-citizen filter in the adapter** — the view renders every frame (including
  paused), but `travel::run` only prunes on a tick, so a citizen removed mid-pause
  would otherwise leave a dangling dot. The `world.citizens.contains_key(id)` filter
  closes that window.
- **Two-layer Normal-overlay gate** — the `HashSet` is built only for the Normal
  overlay (perf), and `overlay_traveler_dot` re-checks Normal + `!is_cursor`
  (correctness + unit-testability, since it takes the overlay directly with no
  frame buffer).

### Determinism / layering / balance

The marker list is `sort_unstable` + `dedup` over `(x, y)` — stable across runs.
UI reads only `GameView`; the adapter is the sole ECS→view boundary. Pure display:
no core/economy mutation, balance-neutral. ASCII fallback untouched (dots are a
TUI-only affordance for v1).

### Reviewed via `claude-city-dev`

codex (`reviewer`) over three rounds — added the stale-dot live-citizen filter,
extracted `overlay_traveler_dot` for a real renderer unit test, restored the
Normal-only `HashSet` build, and switched the width assert to display width; then
opencode (`ses_108ae15e8ffel8UmUzUFJQ94IF`) clean ("ship it"). 7 tests (4 adapter
+ 3 tui); gates green.
