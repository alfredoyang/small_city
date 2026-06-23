# Traveling Citizens — pathfinding, movement, and animation

Status: **P1–P4 implemented** (see "Implemented (P1–P4)" at the end); P5/P6 optional
and unbuilt. Core/simulation feature with a thin UI surface. Supersedes the earlier "traffic via path reconstruction" draft; the
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
  cross-region: the SAME token crosses on a RegionEvent to the neighbor's travel
    map (entity never migrates; no separate proxy) — §5
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
                       destination == E ─► retarget the token to home
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
it targets the border-exit cell, then hands a token across the border; see §5.)

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

`TravelStatus` sketch: `AtHome | Commuting | AtWork | AtLeisure | Away` (the token is
out in a neighbor region; the home side neither draws nor dispatches W until `Return`).
`Direction`: a 4-way orthogonal enum (roads are orthogonal); at a destination (no
next cell) keep the last facing.

---

## 5. Cross-region travel — a real token handoff over a crossing message

A cross-region commuter must **cross**: one logical traveler leaves region A at the
border, its **token** is handed to region B and walks to the workplace, where it
**waits**; when the workday ends the token is removed and a **return** message tells A
the traveler is home again — all carried by an explicit **crossing message**. This is
the authoritative design (it replaces the earlier "no handoff" sketch).

Share-nothing still holds: the citizen **entity never migrates**, and there is no
separate "proxy" — there is just the one **token** (the §2 `TravelState`), the same
thing that already represents *local* movement. What crosses is that token, routed as a
`RegionEvent` over the *same* border topology (`RegionNeighborLink` / `BorderLinkId`)
and worker routing that already carry power/job/goods exports. While the token is
visiting B it is the **single representation** of the away traveler and the only thing
drawn as a dot; the home side keeps no parked movement state — while the token is out it
simply does not dispatch or draw W.

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
   owns W's entity the whole time;            entity NEVER copied across;       the token IS the dot;
   the token is the only thing that crosses    token is owned + serializable    removed on return ⇒ no dot
```

### One token concept — local and cross-region

There is exactly **one** moving thing in this whole design: the **travel token** (the
`TravelState` from §2 — `{ destination, current_cell, status }`). The `Citizen` entity
**never moves**; it has no grid position, and locally only the token's `current_cell`
advances along roads (already true in P3). Cross-region is the *same* token, just
**handed to the neighbor** — there is no separate "proxy" type. A token is either
**local** (in its home region's travel map) or **visiting** (handed to a neighbor's
travel map); same struct, same stepping, same dot renderer either way.

The crossing message wraps that token with the routing it needs to reach the neighbor:

```text
TravelerHandoff {                         // a RegionEvent, routed by the worker
    token:      TravelState,              // THE token — same type used for local travel
    traveler:   TravelerId,               // owned id = (home_region, W.entity, generation)
    from_region, to_region: RegionId,     // routed by RegionNeighborLink, like an export request
    entry_link: BorderLinkId,             // sender's exit link; receiver maps it via matching_neighbor_link()
    return_path: Vec<ReturnHop>,          // multi-hop: push on outbound, pop on return
    purpose:    Outbound | Return,
}

ReturnHop { region: RegionId, entry_link: BorderLinkId }
```

`TravelerId` carries `W.entity` so the **return** message lets the home region find
exactly which away citizen to clear — the round-trip identity the handoff guarantees.
`TravelerId` is **owned data**, never an ECS reference: the neighbor never learns
`W.entity` *as an entity*, only as an opaque id it echoes back.

### The crossing message (new, mandatory)

Two additions, mirroring the existing export plumbing:

- `RegionEvent::ReceiveTraveler(TravelerHandoff)` — inbox event on the receiving region.
- `OutboundMessage::TravelerHandedOff(TravelerHandoff)` — the worker routes it to
  `to_region` by the same `RegionNeighborLink` topology used for job export.

**Key simplification vs. power/jobs/goods:** a handoff is **display-only and
fire-and-forget**. It does **not** pause the tick — there is no `WaitingFor…`
continuation, no grant/deny, no reservation ledger. A emits it and finishes its tick;
B consumes it on its next event pass (**one-tick-stale**, exactly the cross-region
boundary the rest of the model uses). Nothing blocks, so no deadlock surface and no
4th `TickState` phase.

### Lifecycle (round trip)

The token is the only moving thing; it crosses on the way out and is simply **removed**
on the way back (no return walk).

```text
REGION A  (owns W's entity; entity never moves)        REGION B  (hosts the token while away)
────────────────────────────────────────────          ───────────────────────────────────────
09:00 schedule: W's token target = border-exit cell       (no token yet)
      (§4b reuse: a Remote workplace targets the exit
       cell toward B, chosen deterministically from
       RegionNeighborLink + sort_entities_by_position)
token steps home ─► exit cell  (local route cache, P3)
token on exit cell:
  emit TravelerHandoff(Outbound, token, dest=workplace)
  HAND OFF: remove the local token, mark W "away"     ── worker routes ──►  ReceiveTraveler(Outbound):
    (so the schedule won't re-create or draw it)                              insert token at entry cell,
                                                                              dest = workplace
                                                                            token steps entry ─► workplace
                                                                              (B's route cache)  — drawn as a dot
                                                                            token WAITS at workplace (dot stays)
                                                       ── workday ends ──
ReceiveTraveler(Return): ◄── worker routes ──            emit TravelerHandoff(Return, token id);
  remove W's "away" mark ─► W = AtHome                   REMOVE the token (its dot disappears)
  (next work hour, schedule sends it out again)
```

Edge cases (all local, deterministic):

- **Token can't reach the workplace** in B (road torn up mid-trip): B removes the token
  and emits `Return` at once — W goes back to `AtHome`, same as §4b. (No dot lingers.)
- **W's remote job ends** while away: the next daily job phase clears
  `workplace_assignment`; with no remote target the schedule keeps W home. A token still
  out in B self-heals on its own unreachable/return path; the `Return` clears the away
  mark. No cross-region "cancel" message needed.
  - TODO(P5): accept `Return` only when `(TravelerId, generation)` matches W's current
    away token; ignore stale returns from an old trip.
- **A→B→C chain:** the token carries a `return_path` stack. Each outbound hop pushes the
  region/link it came from, then hands the same token to the next region. A return pops
  the stack and routes one hop back; when the stack is empty, the home region clears W's
  away mark. This keeps multi-hop explicit without adding a second proxy type.

### Determinism, persistence, balance

- **Determinism:** the handoff is a `RegionEvent` processed in FIFO order like every
  other event; token ids are derived from `(region, entity, generation)`; border cell,
  entry link, and return-path hops are chosen by `sort_entities_by_position`. Same
  inputs → same crossing.
- **One-tick-stale:** B sees the token the tick after A emits it — the standard
  cross-region latency, not within-tick sync.
- **Persistence:** the token in flight, the home "away" mark, and any visiting token a
  neighbor holds are **transient runtime state** (`#[serde(skip)]`), just like local
  travel state. Save/load drops in-flight crossings; on load the schedule rebuilds
  placement from the hour and each citizen's assignments (an away W simply restarts its
  commute). No save-format change.
- **Balance:** the crossing is **display-only**. Salary, workplace tax, and job
  identity stay entirely on the existing job-export flow; the token moves no money and
  feeds no formula. Removing the animation would not change a single economic number.

Border cells come from the existing `RegionNeighborLink` / `BorderLinkId` topology that
already carries the cross-region job export, so the exit-cell lookup and the
`matching_neighbor_link()` entry mapping are both reuse.

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
- **P5 (optional) — cross-region token handoff (§5).** The real crossing: route a
  cross-region commuter's token to its border-exit (P3 + §4b), then **hand the token
  off** over a new `RegionEvent`. Add `RegionEvent::ReceiveTraveler` +
  `OutboundMessage::TravelerHandedOff`, worker routing by `RegionNeighborLink`, a
  small "visiting tokens" map so the neighbor steps the *same* token type entry-cell →
  workplace (no new proxy type), and an "away" mark on the home side so it stops
  dispatching/drawing W. Workday-end removes the token (dot gone) and emits `Return`,
  which clears the away mark. Fire-and-forget (no tick pause, no grant). Tests: a token
  handed off at A's exit cell arrives at B and steps entry→workplace; `Return` clears
  the away mark so W is home; an unreachable token returns immediately; round-trip is
  deterministic.
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

**P5 (optional) — cross-region token handoff** · layer: **core + regions** (+ reuses P4 UI) · `components.rs`, `systems/travel.rs`, `regions/{mod,runtime,worker}.rs`
```
A travel::run: W's token on border-exit cell ─► OutboundMessage::TravelerHandedOff(token)
               then remove local token + mark W away (schedule skips, no dot)
worker: route the handoff to to_region by RegionNeighborLink   (same routing as job export)
B runtime: RegionEvent::ReceiveTraveler ─► insert the SAME token into B's visiting-token map
B travel::run: step token entry─►workplace or next border; outbound pushes return_path
               workday ends/unreachable ─► emit Return; return pops one hop at a time
A runtime: ReceiveTraveler(Return) with empty return_path ─► clear W's away mark ─► W = AtHome
the token, the away mark, and B's visiting tokens are #[serde(skip)] transient; fire-and-forget (no TickState pause, no grant)
adapter: B's visiting tokens feed the SAME CitizenTravelView path as P4 (one token type, one dot renderer)
```
Test: token handed off at A.exit arrives at B and steps entry→workplace; Return clears the away mark so W is home; unreachable token returns; stale Return is ignored; A→B→C unwinds through B; round-trip deterministic.

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
- **Cross-region:** a real token handoff (§5) over a new `RegionEvent`, routed by the
  existing border topology. The entity never migrates and there is no separate proxy —
  the *same* token is handed to the neighbor's travel map. The token is
  owned/serializable-skip, fire-and-forget (no tick pause, no grant), one-tick-stale —
  so it adds no deadlock surface and no economic coupling.
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
- **Cross-region uses a real token handoff** (§5): the citizen entity never moves —
  movement is always the **token** (the same one used locally; no separate proxy). At
  the border-exit the token **crosses on a new `RegionEvent`** routed by the existing
  topology; the neighbor steps that token to the workplace and it **waits** there. The
  home side marks W "away" (no dot, not dispatched). When the workday ends the token is
  **removed** (dot gone) and a `Return` message clears the away mark — no return walk.
  Fire-and-forget, one-tick-stale, display-only. This is P5.
- Animation/movement is **display-only** until a separate gameplay-coupling mission.

---

## Implemented (P1–P4) — as built

Shipped: `e926761` (P1), `8b5ff0b` (P2), `60263ff` (P3), `e33092c` (P4). P5/P6 remain
optional and unbuilt.

```text
CORE (deterministic)                                    UI
────────────────────────────────────────────────       ──────────────────────────
P1 road_network_analysis::road_predecessors             P4 render_map overlay:
     dest-rooted BFS came_from  ───────────────┐          for each view.travelers cell
                                               │          draw a 2-col dot '•·'
P2 World::routes_to(dest, network) ◄───────────┘             (Normal overlay)
     RefCell route_cache, keyed (dest, net id)                    ▲
     cleared in invalidate_resource_registry                      │
                  ▲                                                │
P3 systems::travel::run(world)  (tick, after happiness)     P4 adapter::traveler_views
     schedule(hour): 09..15 work else home                       GameView.travelers
     step current_cell = routes_to(dest)[current_cell]           = live Traveling cells
     unreachable/no-shared-net -> home (no teleport)             (deduped, sorted)
     prune dead citizens from world.travel                            ▲
     writes world.travel: HashMap<Entity, TravelState> ───────────────┘
```

- **P1** is a pure function; **P2** a region-owned derived cache (mirrors
  `ResourceRegistryCache`); **P3** the only writer of `world.travel`, run each tick;
  **P4** the only reader, through the adapter — the UI never sees paths or the graph.
- Determinism: fixed citizen order + deterministic BFS + integer hours. Display-only:
  `world.travel` is read by no other system, so zero economy/balance impact.
- Deviations from the plan, all ponytail simplifications:
  - `TravelState` lives in a `world.travel` component map, not a `Citizen` field
    (per the existing `Citizen` doc-note).
  - Schedule is commute-only (15–22 "free time" = home); leisure→commercial deferred.
  - Facing **direction** deferred — P4 draws a plain dot, not an arrow.
  - §3a explicit removal handling dropped: a removed destination self-heals via the
    per-tick schedule recompute + P2 cache clear + §4b; `travel::run` prunes dead
    citizens and the adapter filters to live ones.
