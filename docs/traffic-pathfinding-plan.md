# Traffic simulation via deterministic path reconstruction

Status: planned (not implemented). Core/simulation mission — **not** UI.

This scopes the §6 "traffic" feature reserved in
[tui-city-redesign-plan.md](tui-city-redesign-plan.md). The headline: most of the
road-graph work already exists; the new piece is **path reconstruction** (the
sequence of road cells a trip crosses), which lets us accumulate a per-cell
**traffic load** and surface it through the already-reserved Traffic overlay.

Pathfinding alone has no standalone value, so this plan scopes it **with its first
consumer** (the Traffic overlay) rather than as an unused API.

---

## 1. What already exists (reuse, don't rebuild)

`src/core/systems/road_network_analysis.rs` already does deterministic road-graph
work, consumed by economy/happiness/inspect:

- **multi-source BFS distances**: `road_distances(world, network, sources) ->
  HashMap<Entity, u32>`
- **building → road entry cells**: `adjacent_roads_in_network`, sorted by
  `road_connectivity::sort_entities_by_position` (already deterministic)
- `nearest_distance`, `distance_between_buildings`, `access_for`
- `RoadNetwork` (the connected road component) and per-region/cross-region
  road-component reachability in the worker discovery graph

Trips have well-defined endpoints already: `Citizen { home, workplace_assignment }`
(commute) and residential → commercial (shopping).

**The gap:** BFS today returns *distances*, not *paths*. Traffic load needs to
know *which cells* each trip traverses.

---

## 2. The increment: path reconstruction

Extend the existing BFS with predecessor back-pointers, then walk them:

```
DESTINATION-ROOTED BFS (one per unique destination entry-cell set)
  road_distances + came_from:  HashMap<Entity, Entity>   (cell -> its BFS parent)
  for each origin road cell:
      follow came_from from origin's nearest entry to the destination,
      incrementing traffic_load[cell] for every cell on the way
```

Why root BFS at the **destination** (workplace / shop), not the origin: many
origins share one destination, so one BFS tree serves all of them — each origin
just walks its predecessor chain (O(path length)), giving an efficient
`O(destinations · roads + Σ path lengths)` instead of a BFS per citizen.

### Determinism (CLAUDE.md §3 — mandatory)

The path a trip takes must be identical for identical inputs, or saves/replays
diverge. Lock in:

- **Fixed neighbour order** when expanding BFS (e.g. N, E, S, W) and a FIFO queue.
- **Predecessor is set once** (first visit wins); with fixed neighbour order this
  makes the BFS tree deterministic even when multiple shortest paths exist.
- **Stable trip iteration**: iterate citizens/destinations in a fixed order
  (sort by entity position, reusing `sort_entities_by_position`), so load
  accumulation order is fixed.
- Pure integer math; no floats.

This is *determinism*, not *synchronicity* — intra-region traffic resolves within
the tick; cross-region trips stay one-tick-stale (see §6).

---

## 3. Where the load lives

Traffic is a **derived per-road-cell scalar**, exactly like `pollution_pressure`:

- Add `traffic` to the per-cell `LocalEffects` (core) so it rides the existing
  `LocalEffectsMap` and overlay machinery. **No `Grid` storage change.**
- Recompute in a `systems/traffic.rs` pass, invalidated at the same mutation
  chokepoints as other derived state (build/bulldoze/upgrade), not every frame.
- Clamp/scale to the 0..9 band the intensity overlays already use.

```
core: systems/traffic.rs  ──►  LocalEffects.traffic (0..9 per road cell)
adapter: inspect/view  ─────►  CellView.traffic: Option<i32>   (reserved field, §2 of UI plan)
ui: MapOverlayInput::Traffic ─► colour road cells green→yellow→red (intensity_style)
```

---

## 4. UI consumer (already shaped for this)

The TUI redesign plan reserved exactly these hooks, so the UI side is small:

- `CellView.traffic: Option<i32>` — the reserved field (UI plan §2); fill it in the
  adapter from `LocalEffects.traffic`.
- `MapOverlayInput::Traffic` — one new overlay variant; cycle + legend + colour via
  the existing `intensity_tile`/`intensity_style` (same path as pollution).
- Optional later: cosmetic **vehicle dots** animating along high-load roads, using
  the UI animation clock already shipped (frame-counter; cosmetic only, no sim
  coupling).

---

## 5. Gameplay coupling — start display-only

Decide deliberately to avoid balance surprises:

- **Phase 1: display-only.** Traffic is computed and shown but feeds **no**
  economy/happiness formula. Zero balance change; easy to validate.
- **Phase 2 (separate, optional): couple it.** Congestion raises the commute
  penalty / lowers nearby land value. This is where traffic becomes a *mechanic*.
  Gate it behind its own mission and balance pass; `route_margin_penalty` /
  `commute_penalty` in `road_network_analysis` are the natural seams.

---

## 6. Cross-region traffic — defer

Intra-region first. Cross-border trips (commute/goods across a region edge) must
read the neighbour's **previous-tick published snapshot** (one-tick-stale), like
power/jobs/goods already do — never a live neighbour `World`. Scope cross-region
load as a follow-up once the per-region pass and the published-summary plumbing
exist; do not read neighbour worlds synchronously.

---

## 7. Ordering decision: multi-cell buildings (§5) first?

**Recommendation: land multi-cell buildings *before* coupling traffic, but
intra-region display-only traffic can proceed now.**

- Traffic routes between **building road-entry cells**. Multi-cell buildings
  (UI plan §5) change a building's footprint and therefore *which* cells are its
  road-adjacent entry points (`adjacent_roads_in_network` would consider all
  footprint cells, not just the anchor).
- For **1×1 buildings (today)** nothing changes, so Phase 1 traffic is safe now.
- But if multi-cell lands after traffic is *coupled to gameplay*, the entry-point
  change would shift routes and rebalance the economy — avoid that churn by either
  doing multi-cell first, or keeping traffic display-only until multi-cell is in.

---

## 8. Suggested split (each its own tested patch)

- **P1 — path reconstruction**: add predecessor BFS to `road_network_analysis`
  (or a new `road_paths` helper); unit-test determinism (same path every run,
  fixed tie-break on equal-length routes). No behaviour change yet.
- **P2 — traffic pass**: `systems/traffic.rs` accumulates per-cell load from
  commute + shopping trips into `LocalEffects.traffic`; cache-invalidated at the
  mutation chokepoints. Tests: a single home→work pair loads exactly the cells on
  its route; two trips sharing a segment sum on the shared cells.
- **P3 — UI surface**: adapter fills `CellView.traffic`; add
  `MapOverlayInput::Traffic` + legend + colour. Tests: overlay colours a loaded
  road, dims an empty one.
- **P4 (optional, separate) — gameplay coupling** and **P5 (optional) —
  cross-region** and **vehicle animation**: each its own mission.

Run `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test -q` after each.

---

## 9. Risks / guardrails

- **Determinism** is the top risk — §2's fixed neighbour order + first-visit
  predecessor + stable iteration are non-negotiable; add a replay/parity test.
- **Performance**: destination-rooted BFS keeps it near-linear; recompute on
  change, not per frame. Watch large maps; cap or sample if needed (note the
  ceiling in a `// traffic:` comment if a heuristic is used).
- **No balance drift** in Phase 1 — traffic must not feed any formula until P4.
- **Architecture**: all of this is `core` + adapter; the UI reads view models
  only (the Traffic overlay never reads ECS), per the layering rule.
