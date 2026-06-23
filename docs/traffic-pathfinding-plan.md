# Traveling Citizens — pathfinding, movement, and animation

Status: **planned (not implemented).** Core/simulation feature with a thin UI
surface. Supersedes the earlier "traffic via path reconstruction" draft; the
headline goal changed from a per-cell heatmap to **citizens that visibly move to
their destinations**, so the architecture is built around per-citizen movement, not
an aggregate load scalar. (A heatmap can still fall out as an optional consumer —
see P6.)

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
  route_epoch: u64                                      { x, y, direction, status }
  route cache (derived, #[serde(skip)]):            TUI: draw a dot per traveler
    HashMap<(dest, network), DestinationRoutes{          (position + facing); never
      came_from: HashMap<Entity,Entity>, epoch }>        sees paths or the graph
  per-citizen travel state:
    TravelState { destination, current_cell, status }
  movement system: advance travelers each tick
  cross-region: each region animates its own half (no handoff — §5)
```

Two hard rules this design exists to honour:

1. **Strict layering.** The UI knows only a citizen's *status, position, and facing
   direction* — everything else (paths, the road graph, progress math) is computed
   in core and exposed through a view model. The Traffic/animation UI never reads
   ECS.
2. **Determinism + the one-tick-stale boundary.** Movement is deterministic core
   state (saved-or-derived, replayable). Cross-region effects read the neighbour's
   *previous-tick published snapshot*, never its live `World` — like power/jobs/goods.

---

## 1. What already exists (reuse, don't rebuild)

`src/core/systems/road_network_analysis.rs` already has the deterministic road-graph
machinery:

- **`road_distances(world, network, sources) -> HashMap<Entity, u32>`** — multi-source
  BFS, FIFO queue, neighbours sorted by `road_connectivity::sort_entities_by_position`,
  **first-visit-wins**. This is exactly the BFS we extend with predecessors (P1).
- **`adjacent_road_entities` / `adjacent_cells`** — already **footprint-aware**: they
  enumerate every footprint cell of a building and collect all surrounding road cells.
  So multi-cell buildings already have correct road entry points.
- `RoadNetwork` (connected road component), `sort_entities_by_position` (stable order).

Trip endpoints exist: `Citizen { home, workplace_assignment }` (commute) and
residential → commercial (shopping).

The cross-region scaffolding exists too: `RegionNeighborLink` / `BorderLink`
topology and the region event flow used by power/jobs/goods export.

**The gap:** BFS returns distances, not *paths*; there is no per-citizen movement and
no route cache. (Cross-region needs nothing new — see §5.)

### Corrections to the earlier draft

- `CellView.traffic` and `MapOverlayInput::Traffic` were described as "reserved" —
  **they do not exist today.** Any UI surface must add them; trivial, but not pre-wired.
- The multi-cell-buildings ordering concern (old §7) is **resolved**: multi-cell
  buildings are implemented and `adjacent_cells` is footprint-aware.

---

## 2. Where the data lives — region-owned route cache, per-citizen trip state

A path is a pure function of `(road graph, origin cell, destination cell)`; the
citizen's identity is irrelevant. Storing it per-citizen would duplicate identical
routes (15 residents → 15 copies). So split by what each piece actually depends on:

```text
REGION (one cache, shared)                  CITIZEN (per-entity, tiny)
  the "pathfinding table" = route geometry     destination:  Option<Entity>
  keyed by (DESTINATION, ROAD NETWORK)         current_cell: Option<Entity>  (road cell now)
  destination-rooted BFS predecessor trees     status:       TravelStatus
  epoch-stamped; invalidated as a unit         (steps via the region's came_from,
                                                O(1) per tick — no stored path)
```

- **Region owns the route table.** Best form: **destination-rooted BFS** — one BFS
  per `(destination building, road network)` covers *all* origins on that road
  network. Cache
  `HashMap<(dest_entity, road_network_id), DestinationRoutes { came_from: HashMap<Entity,Entity>, epoch }>`.
  The `road_network_id` matters because a multi-cell destination can touch roads
  from more than one disconnected local road network; those trees must not collapse
  into one entry.
  A citizen reconstructs its route by walking `came_from` from its origin entry cell
  — O(path length), zero per-citizen geometry storage.
- **Citizen owns only trip state**: its destination, the road cell it is on **now**,
  and its status. Because the BFS is destination-rooted, `came_from` already points
  *toward* the destination, so movement is **O(1) per tick** with no per-citizen path
  `Vec` and no re-walk:

  ```text
  store   current_cell: Option<Entity>          (None = inside a building, not on a road)
  step    current_cell = came_from[current_cell] (one cell toward the destination)
  dir     pos(next) − pos(current)
  arrive  current_cell is a destination root  ─►  status = At…, current_cell = None
  ```

Why region (not per-citizen, not city-global):

| | Per-citizen | **Region** ✅ | City-global |
|---|---|---|---|
| Dedup of shared routes | No (N copies) | Yes (one tree/dest) | Yes |
| Dirtying | Walk every citizen | **One epoch bump per region** | Coarse global |
| BFS work | Risk per-citizen recompute | One BFS/(dest, network) serves all origins | Same |
| Threading fit | ok | **Matches per-region World ownership** | ✗ shared cross-thread cache |
| Cross-region | awkward | Each region routes its own segment | ✗ mixes region graphs |

City-global is rejected: it breaks the share-nothing region model (regions own their
`World`/derived state per thread) and a path never spans graphs anyway.

This mirrors the existing **`ResourceRegistryCache` (R5)**: a `#[serde(skip)]`
derived cache recomputed on change and invalidated at the `attach_*`/`remove_entity`/
upgrade chokepoints. The route cache is another such region-owned derived cache.

---

## 3. Dirty mechanism — epoch stamping (lazy)

```text
build / bulldoze / replace / upgrade road/building footprint ─► world.route_epoch += 1
citizen needs route to B:
    entry = route_cache[(B, origin_network)]
    if entry is None || entry.epoch != world.route_epoch  ─► recompute (dest-rooted BFS), restamp
    else                                                   ─► reuse
```

- Add `route_epoch: u64` to `World`, bumped whenever route geometry can change — at
  the same mutation chokepoints that already call `invalidate_resource_registry`.
  This is wider than "road changed": building placement/removal and footprint growth
  can change destination roots / entry roads even if the road tiles themselves did
  not change.
- Each cache entry stamps the epoch it was built under; stale entries recompute
  **lazily on next use** (no eager sweep of citizens).
- Coarse by design — any route-shape change invalidates the whole region's route
  cache. Simple and robust; a `// traffic:` comment records the ceiling and the
  upgrade path (per-cell-scoped invalidation) if profiling ever demands it.
  **YAGNI until then.**

### 3a. Destination removal — update citizens, evict the cache

The `route_epoch` handles route geometry changes, but removing a **building** is
different: a citizen's home / workplace / free-time destination can disappear. On
`remove_entity` (bulldoze/replace) we must, for the removed building:

```text
remove building E ─► scan citizens for references to E:
                       home == E       ─► citizen is homeless (existing residential-
                                           removal path handles despawn/rehome)
                       workplace == E   ─► clear workplace_assignment (job phase)
                       destination == E ─► redirect to home (status Returning)
                     drop route_cache entries where destination == E
```

Removal is a rare player action, so an O(citizens) reverse scan at the chokepoint is
fine (no reverse index needed — note the ceiling with a `// traffic:` comment). This
is also the cache **eviction** path: a removed destination's `DestinationRoutes`
entry is dropped, not left as garbage.

---

## 4. Movement (core, deterministic)

### 4a. Daily schedule (what makes a citizen travel)

One simple schedule, shared by all citizens, keyed off the now-unified `GameTime`
hour:

```text
 hour:  00 ───── 09 ─────────── 15 ─────────── 22 ──── 24
 want:  [   HOME    ][   WORK      ][  FREE TIME  ][  HOME  ]
 dest:   home         workplace      a commercial   home
```

- **09:00–15:00 → WORK** (target = workplace)
- **15:00–22:00 → FREE TIME** (target = a commercial; v1: the nearest reachable one
  from the citizen's **current location**, deterministic tie-break by
  `sort_entities_by_position`; none reachable → home)
- **22:00–09:00 → HOME** (target = home)

Each tick the movement system computes each citizen's *target building* from the
hour, then:

```text
at target building?  ─► status = AtHome / AtWork / AtLeisure ; current_cell = None (idle)
target changed?      ─► depart: current_cell = an entry road cell of the current
                          building; status = Commuting/Traveling; destination = target
en route?            ─► step current_cell = came_from[current_cell] (one cell);
                          arrived when current_cell is a destination root
```

So trips are generated by the **hour boundaries** (09 / 15 / 22) — a citizen leaves
when its scheduled target changes. v1 is **commute + a single free-time hop**;
richer errands are later.

### 4b. Degenerate routes → go home

If the target has **no road route** from the origin (different `RoadNetwork`, or
disconnected), the citizen does **not** teleport: its destination falls back to
**home**. If home is also unreachable or it is already home, it stays put (`AtHome`,
no dot). "Return home if it happens." (A *cross-region* workplace is not unreachable —
it targets the border-exit cell instead; see §5.)

### 4c. Granularity / determinism / persistence

- **v1 granularity: tick-cadence (accepted).** A traveler advances one cell per tick;
  the UI shows discrete steps; paused sim ⇒ frozen. A smooth sub-cell tween is the
  *only* thing that could ever live in the UI (off `anim_frame`, between two
  core-provided positions) — deferred (P6), add only if the stepping looks too coarse.
- **Determinism:** integer math; travelers iterated in a fixed order
  (`sort_entities_by_position`); fixed BFS neighbour order; first-visit-wins
  predecessors → identical routes and movement for identical inputs.
- **Persistence:** travel state is transient derived runtime state
  (`#[serde(skip)]`). Save/load does **not** preserve in-flight trips; on load the
  movement system rebuilds placement from the current hour and each citizen's
  assignments. No save-format change.

`TravelStatus` sketch: `AtHome | Commuting | AtWork | AtLeisure | CommutingAway | Returning`.
`Direction`: a 4-way orthogonal enum (roads are orthogonal); at a destination (no
next cell) keep the last facing.

---

## 5. Cross-region travel — two independent halves, no handoff

The UI shows **one region at a time**, so a single dot crossing the seam is never
visible. So we don't move one dot across — each region animates **its own half**, and
the two halves are never synchronized or shared.

A cross-region worker `W` already has everything we need: `W.workplace_assignment =
Remote { region: B, position }` (built by the job-export phase), and `B` already
computes how many remote workers each of its workplaces hosts (the remote-workers
feature). Nothing else is added.

```text
   REGION A  (home — owns W's entity)              REGION B  (hosts the job)
   ┌──────────────────────────────────┐            ┌──────────────────────────────────┐
   │ when viewing A you see W's dot:   │            │ when viewing B you see anon dots: │
   │   09:00 home ─►(route cache)─►    │            │   per local workplace with        │
   │         A.border-exit toward B    │  ░░ border │     remote_count > 0:             │
   │         arrive ─► CommutingAway   │  ░░░░░░░░░  │     spawn remote_count dots,      │
   │   15:00 border-exit ─► leisure    │  ░░ never  │     looping                       │
   │   22:00 ─► home                   │     seen   │     B.border-entry ─► workplace   │
   │                                   │     at the │                                   │
   │ input: W.workplace_assignment     │     same   │ input: remote-worker count per    │
   │        (Remote → region B)        │     time   │        workplace (already known), │
   │        + A's border-exit cell     │            │        + B's border-entry cell    │
   │ ALL LOCAL to A — no neighbour read │            │ ALL LOCAL to B — no neighbour read │
   └──────────────────────────────────┘            └──────────────────────────────────┘
```

### How a worker "crosses"

- **Home side (A), real entity.** During work hours `W`'s travel target is the
  **border-exit cell** — the road cell on A's edge whose border link reaches B's road
  network (the same link the job export was assigned over; pick deterministically by
  `sort_entities_by_position`). `W` routes there with the normal route cache, then
  idles `CommutingAway` (no dot drawn — it has "left the map"). The schedule walks it
  back home at 15:00/22:00. **This is just §4b's border-exit target — no new code in
  the movement system.**
- **Destination side (B), anonymous proxies.** Purely cosmetic, generated in the
  adapter, **not** citizens: for each B workplace, read its remote-worker *count* and
  emit that many dots that loop **B.border-entry → workplace** along B's own route
  cache. They carry no identity, no link to `W`. (Optional P5; skip it and the worker
  simply vanishes at A's edge.)

### Why this is enough

The two halves only have to *look* right from one viewpoint at a time, which they do.
Nothing crosses the share-nothing boundary: A reads only A's `World`, B reads only
B's. No token, no proxy lifecycle, no handoff event, no one-tick reconciliation, no
in-flight save state. Determinism is trivial (each side is a local, deterministic
animation). If a worker commutes A→B→C, it's just A→border and C drawing arrivals —
still two independent halves.

Border cells come from the existing `RegionNeighborLink` / `BorderLink` topology that
already carries the cross-region job export, so even the border lookup is reuse.

---

## 6. UI surface (thin)

- adapter ─► `CitizenTravelView { x, y, direction, status }` (and region, for the
  selected-region view). Built from core travel state; **no path/graph leaks to UI.**
- TUI: draw a dot per traveler on its current road cell, oriented by `direction`.
  Stays within the 2-column tile discipline (overlay a marker / tint the road cell;
  multiple travelers on a cell → show one). Driven by the simulation position, not a
  UI clock (the existing `anim_frame` is only needed if we add the optional sub-cell
  tween).
- Cap / sample the number of rendered dots (representative, not 1:1) with a
  `// traffic:` ceiling note, to bound render cost on busy maps.

---

## 7. Phase split (each its own tested city-dev patch)

- **P1 — pathfinding.** Add predecessor back-pointers to the existing BFS
  as a sibling helper, e.g.
  `road_predecessors(world, network, sources) -> HashMap<Entity, Entity>`.
  No cache, no movement, no behaviour change yet. Unit-test determinism: identical
  reconstructed route every run; fixed tie-break on equal-length routes.
- **P2 — region route cache + epoch dirtying.** Region-owned
  `HashMap<(dest, road_network_id), DestinationRoutes>` derived cache;
  `world.route_epoch`; lazy recompute on stale. Tests: reuse hits when fresh; a
  road/building-footprint mutation bumps the epoch and forces recompute; route
  reconstruction from an origin matches a direct BFS.
- **P3 — movement sim + schedule.** Per-citizen `TravelState`; the daily schedule
  (§4a) chooses each citizen's target by hour; travelers step one cell/tick via
  `came_from`; degenerate target → home (§4b); building removal updates citizen
  status + evicts the cache (§3a). Tests: at 09:00 a resident departs home and steps
  the exact route cells to its workplace, idling there until 15:00; an unreachable
  target sends it home; bulldozing the workplace flips status and drops the cache
  entry; deterministic across runs.
- **P4 — view model + UI dots.** adapter `CitizenTravelView`; TUI renders moving
  dots (position + facing). Tests: a moving traveler appears on its current cell with
  the right facing; empty roads show none.
- **P5 (optional) — incoming-commuter dots.** The home-region leg is already free
  (P3 + §4b routes a cross-region commuter to its border-exit). This patch only adds
  the destination-side anonymous dots from border-entry → workplaces, off the
  remote-worker count. No token/proxy/handoff/event.
  Tests: a region with N remote workers shows dots moving border-entry → workplace;
  zero remote workers → none.
- **P6 (optional, parallel).** Aggregate per-cell **traffic load** + `Traffic`
  overlay as a second consumer of P1 routes (the old heatmap); and/or the cosmetic
  sub-cell movement tween. Gameplay coupling (congestion → commute penalty / land
  value) is a further, separately balanced mission.

Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test -q` after each.

### 7b. Per-patch architecture

Layers: `core` (sim/ECS) → `interface` (adapter + view models) → `ui` (TUI). Each
patch names which layers it touches, the data flow, and the additions.

**P1 — predecessor BFS** · layer: **core only** · `systems/road_network_analysis.rs`
```
road graph ─► road_predecessors(world, network, dest_roads) ─► HashMap<Entity,Entity>
              (same BFS as road_distances; record came_from on first visit, dest-rooted)
```
Pure function, no state, no other layer. Test: identical came_from every run.

**P2 — route cache + epoch** · layer: **core only** · `world.rs` (+ `systems/route_cache.rs`)
```
        build/bulldoze/upgrade ─► World.route_epoch += 1   (at attach_*/remove_entity)
caller ─► routes_to(world, dest, network):
            cache[(dest,network)] fresh?  ─► reuse came_from
            else ─► road_predecessors() + restamp epoch
World { + route_epoch: u64, + #[serde(skip)] route_cache: HashMap<(Entity,NetId), DestinationRoutes> }
```
Mirrors `ResourceRegistryCache`. Test: fresh reuse; road change forces recompute.

**P3 — movement + schedule** · layer: **core only** · `components.rs`, `systems/travel.rs`, `world.rs`
```
tick (after jobs) ─► travel::run(world):
    for each citizen (sorted by position):
        target  = schedule(world.time.hour, citizen)            // §4a  home/work/leisure
        depart  = target changed ─► current_cell = building entry cell
        step    = current_cell = routes_to(target)[current_cell] // §2  O(1)
        unreachable ─► target = home                             // §4b
remove_entity(E) ─► scan citizens (home/work/dest == E) fix status; route_cache evict E  // §3a
Citizen { + #[serde(skip)] travel: TravelState { destination, current_cell, status } }
```
No interface/ui yet. Test: 09:00 home→work exact cells; unreachable→home; bulldoze flips status.

**P4 — view model + dots** · layer: **interface + ui** · `view.rs`, `adapter.rs`, `ui/tui.rs`
```
core TravelState ─► adapter ─► Vec<CitizenTravelView{ x,y,direction,status }> on GameView
                                  (reads sim state; no path/graph leaks)
                 ─► tui: per traveler, draw a 2-col dot on its road cell, facing `direction`
```
UI reads the view model only. Test: a traveler shows on its cell with the right facing; empty roads → none.

**P5 (optional) — incoming-commuter dots** · layer: **interface + ui** · `adapter.rs`, `ui/tui.rs`
```
remote-worker count per workplace (existing) ─► adapter: emit N anonymous dots/workplace
                                                  routing border-entry ─► workplace (reuse route cache)
                                              ─► tui: same dot renderer as P4
no new core code/event; home-region leg already done by P3/§4b
```
Test: region with N remote workers → N dots border-entry→workplace; zero → none.

**P6 (optional, parallel)** · layer: varies
```
heatmap:  core LocalEffects.traffic (aggregate P1 routes) ─► adapter CellView.traffic
          ─► ui MapOverlayInput::Traffic (reuse intensity_tile)
tween:    ui-only sub-cell interpolation off anim_frame between two core positions
```
Each a separate mission; heatmap is a 2nd consumer of P1, tween is pure UI.

---

## 8. Risks / guardrails

- **Determinism (top risk):** fixed BFS neighbour order + first-visit predecessors +
  stable traveler iteration are non-negotiable; add a replay/parity test once movement
  exists. Travel state must not introduce float math.
- **Performance:** destination-rooted BFS keeps pathfinding near-linear; the route
  cache recomputes on epoch change, not per frame/tick. Cap rendered dots. Note any
  heuristic ceiling in a `// traffic:` comment.
- **Layering:** core computes status/position/direction; the UI renders the view
  model only and never reads ECS or paths.
- **Cross-region:** no handoff — each region animates only its own roads (§5), so
  there is no neighbour read at all. A commuter just routes to its border-exit.
- **Balance:** movement/animation through P5 is display-only — it must feed no
  economy/happiness formula. Gameplay coupling is a deliberately separate, balanced
  mission (P6+).

## 9. Decisions locked (for review)

- Route table is **region-owned**, keyed by `(destination, road_network_id)` (not
  per-citizen, not city).
- Per-citizen state is just `{ destination, current_cell, status }`; movement steps
  in O(1) via the region's `came_from` (no stored path, no re-walk).
- Dirtying via a **region `route_epoch`**, lazy recompute. It bumps on road graph
  changes and building adjacency/footprint changes. **Building removal** scans
  citizens to fix status and evicts the dead destination's cache entries (§3a).
- **Daily schedule** (shared, by hour): 09–15 work, 15–22 free time (nearest
  commercial from the current location), else home; hour boundaries generate the
  trips (§4a).
- **Unreachable target → go home** (no teleport) (§4b).
- Movement is **deterministic transient core runtime state**, **tick-cadence** v1
  (stepped is accepted; smooth tween deferred to P6), **derived/`skip`** on save
  (load places by schedule).
- **Cross-region has no handoff** (§5): a commuter routes to its border-exit and
  idles `CommutingAway` in its own region; destination-side incoming dots (P5) are an
  optional, isolated add off the remote-worker count. No token/proxy/event.
- Animation/movement is **display-only** until a separate gameplay-coupling mission.
