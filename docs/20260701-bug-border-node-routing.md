# 20260701 Bug Border Node Routing

## 1. Introduction / Problem

`build_region_routes` currently builds Layer-1 routes with **region nodes**:

```text
node = RegionId
edge = region R -> neighbour N
cost = R cost to reach exit border + N cost from entry border
```

That is enough to choose a next-hop region, but the numeric `RouteHop.cost` is wrong
for multi-hop paths because a middle region's interior can be counted once per edge.

Example:

```text
A ---- B ---- C
      B west -> east road cost = 10

current region-edge model:
  A -> B includes B entry-side cost
  B -> C includes B exit-side cost
  cost_to_C(A) can become 20

real movement:
  enter B once, walk west -> east once, leave B
  B interior cost = 10
```

Today this is mostly hidden because travel consumes `RouteHop.exits`, not
`RouteHop.cost`. It becomes a bug as soon as Layer-1 cost is used for ETA, budget,
route comparison, or UI explanation.

## 2. Proposal

Replace the internal Layer-1 Dijkstra graph with **border-link nodes** while keeping
the public output shape.

```text
Current:

  A --------> B --------> C
  region     region     region

Proposed:

  (A, East) --cross--> (B, West) --walk inside B--> (B, East) --cross--> (C, West)
      ^                     ^                         ^
      BorderLinkId          BorderLinkId              BorderLinkId
```

Graph nodes:

```rust
type RouteNode = (RegionId, BorderLinkId);
```

Graph edges:

```text
1. Inside-region edge
   (R, entry_link) -> (R, exit_link)
   cost = RegionCrossCost { entry, exit, cost }

2. Border-crossing edge
   (R, local_link) -> (N, matching_link)
   cost = 1
```

The existing report already has the needed data:

```text
RegionRoadReport
  border_links   = where this region crosses to neighbours
  crossing_costs = Layer-2 road distance from entry border to exit border
```

Run destination-rooted Dijkstra over the reversed border-node graph. For destination
region `T`, seed every `(T, border_link)` at cost `0`. Then, for each source region
`R`, choose exits `(R, link)` whose crossing edge goes to a neighbour node with a
strictly lower distance.

Output stays the same:

```rust
RegionRoutes {
    to: HashMap<RegionId, RouteField>
}

RegionRoutes::exits_from(r) -> HashMap<RegionId, Vec<ExitLink>>
```

So `RegionState::set_region_routes` and travel movement do not need to know the
internal graph changed.

## 3. Important Functions And Structures

`src/core/regions/directory.rs`
- `build_region_routes(...)`
  - Replace the internal region-node graph with a border-node graph.
  - Keep return type `RegionRoutes`.
- `RegionDirectory::publish_region_road_report(...)`
  - Reused. It already rebuilds routes after reports change.
- `RegionDirectory::exits_from(...)`
  - Reused unchanged.

`src/core/regions/mod.rs`
- `RegionRoadReport`
  - Reused as input.
- `RegionBorderLink`
  - Reused to build border-crossing edges.
- `RegionCrossCost`
  - Reused to build inside-region edges.
- `RegionRoutes`, `RouteField`, `RouteHop`, `ExitLink`
  - Reused as output. `RouteHop.cost` becomes exact border-node distance for the
    selected source-to-destination route.
- `RegionState::road_report(...)`
  - Reused. No report shape change required.
- `RegionState::set_region_routes(...)`
  - Reused unchanged. It still receives final-target `RegionId -> Vec<ExitLink>`.

`src/core/regions/worker.rs`
- Existing publish/install flow is reused:

```text
runtime.state().road_report(...)
  -> directory.publish_region_road_report(...)
  -> directory.exits_from(source_region)
  -> runtime.set_region_routes(...)
```

No new worker command, event, or UI boundary change.

## 4. Pseudocode / Integration

Inside `build_region_routes`:

```rust
type RouteNode = (RegionId, BorderLinkId);

let reports_by_region = reports.iter().map(|r| (r.region, r)).collect();
let mut graph: HashMap<RouteNode, Vec<(RouteNode, u32, Option<ExitLink>)>> = HashMap::new();

// Inside-region movement: entry border -> exit border.
for report in reports {
    if owners.owner_of(report.region).is_none() {
        continue;
    }
    for c in &report.crossing_costs {
        graph[(report.region, c.entry)].push((
            (report.region, c.exit),
            c.cost.max(1),
            None,
        ));
    }
}

// Crossing movement: local border -> neighbour matching border.
for report in reports {
    for border in &report.border_links {
        let n = border.neighbour;
        if owners.owner_of(n).is_none() {
            continue;
        }
        let Some(n_report) = reports_by_region.get(&n) else { continue };
        let matching = border.link.matching_neighbor_link();
        if !n_report.border_links.iter().any(|b| b.link == matching && b.neighbour == report.region) {
            continue;
        }

        graph[(report.region, border.link)].push((
            (n, matching),
            1,
            Some(ExitLink { link: border.link, to_region: n }),
        ));
    }
}
```

For each target region `T`:

```rust
// Seed all border nodes in T.
for border in reports_by_region[&T].border_links {
    dist[(T, border.link)] = 0;
}

// Run Dijkstra over reversed graph.
// dist[(R, link)] means: cheapest cost from this border node to any border in T.
```

Then build `RouteField.from`:

```rust
for source_region in owned_regions {
    if source_region == T {
        from.insert(source_region, RouteHop { exits: Vec::new(), cost: 0 });
        continue;
    }

    let mut candidates = Vec::new();

    for border in reports_by_region[&source_region].border_links {
        let node = (source_region, border.link);

        // A valid first hop must be a crossing edge whose destination node has
        // a strictly lower distance. This preserves loop safety.
        for (next_node, crossing_cost, Some(exit)) in graph[node] {
            if dist[next_node] < dist[node] {
                candidates.push((crossing_cost + dist[next_node], exit));
            }
        }
    }

    keep_min_cost_candidates_sorted_by(region, edge, offset);
    from.insert(source_region, RouteHop { exits, cost: min_cost });
}
```

Call order stays:

```text
RegionState::road_report
  -> RegionDirectory::publish_region_road_report
     -> build_region_routes
  -> RegionDirectory::exits_from
  -> RegionRuntime::set_region_routes
  -> RegionState::set_region_routes
  -> travel stepper uses remote_exit_cells
```

## 5. Tests

Add or update tests in `src/core/regions/directory.rs`.

- `region_routes_cost_counts_middle_region_once`
  - A-B-C line.
  - B has `RegionCrossCost { west -> east, cost: 10 }`.
  - Assert `RouteHop.cost` from A to C reflects one B traversal, not double-counting.

- `region_routes_still_maps_multihop_destination_to_first_hop`
  - Existing behavior: A to C exits toward B.
  - Confirms output shape remains compatible with `RegionRoutes::exits_from`.

- `region_routes_preserve_parallel_exits_to_best_next_region`
  - Keep the regression from the frozen-dot bug: multiple exits to same next-hop survive.

- `region_routes_skip_roadless_border`
  - Raw adjacency without paired published border links must not create an edge.

- `region_routes_tie_break_is_deterministic`
  - Equal-cost exits sort by `(to_region, edge, offset)`.

No UI tests; no UI behavior changes.

## 6. Risks / Non-goals

- This is an internal route-cost fix. It should not change the `RegionRoutes` API or
  UI-facing models.
- It may change which equal-looking route is selected when the old inflated costs hid
  the cheaper true corridor. That is intended.
- Keep crossing cost as `1` for now. If border-crossing time becomes gameplay-visible,
  add a named constant or report field later.

## 7. P-d implementation record (border-node routing)

**Committed:** not yet (in the working branch `multi-region-return`).
**What landed in the worktree:**

### The new graph (`src/core/regions/directory.rs::build_region_routes`)

```text
node  = (RegionId, BorderLinkId)            // one per published border opening
edge  = 1) inside-region:  (R, entry) -> (R, exit)
                                 weight = RegionCrossCost{entry, exit, cost}
                                          clamped to >= 1 (strict-decrease)
        2) border-crossing: (R, link) -> (N, link.matching_neighbor_link())
                                 weight = 1
                                 payload = ExitLink { link, to_region: N }
```

The graph is built in one pass. Crossing edges also populate a reverse index
`crossing_rev[(N, matching_link)] -> Vec<(R, link, 1)>`; inside edges
populate `inside_rev[(R, exit)] -> Vec<(R, entry, weight)>`. Both reverse
indices are built once, after the forward pass.

### The Dijkstra (per owned destination T)

For each destination T, run forward Dijkstra on the reversed graph:

```text
seed:   dist[(T, link)] = 0  for every T border opening
relax:  when popping (R, link) at cost `c`:
          - inside_rev[(R, link)] -> for each (R, entry, w): relax dist[(R, entry)] = c + w
          - crossing_rev[(R, link)] -> for each (P, source_link, 1): relax dist[(P, source_link)] = c + 1
```

The frontier is a `BTreeSet<(u32, RouteNode)>` with lazy deletion; ties on
cost break by `RouteNode` ordering. The result: `dist[(R, link)]` =
shortest path from `(R, link)` to any `(T, _)` border node, in the
**original** graph.

### The candidate selection (per source region R)

```text
n_dist_for_n:  HashMap<RegionId, u32>            // min dist to T from N
valid_crossings: Vec<(R.link, N, ExitLink)>       // every strict-decrease crossing

best_next_total = min over n in n_dist_for_n of (1 + n_dist_for_n[n])
chosen_regions  = { N : 1 + n_dist_for_n[N] == best_next_total }
emitted         = { exit in valid_crossings : exit.to_region in chosen_regions }

RouteHop = { exits: emitted, cost: best_next_total }
```

The "all strict-decrease exits to the chosen next-hop region" rule is the
**frozen-token guarantee**: a token on a less-optimal local network still
has a reachable exit to the chosen next-hop region. The next-hop region
itself is what the consumer follows; the choice of border opening is a
local-network detail Layer 2 handles.

#### The patch (data flow)

```text
                  RegionRoadReport (input)
                  |   border_links  -> who crosses where
                  |   crossing_costs -> (entry, exit) Layer-2 distances
                  v
                  +------------------------------------------------+
                  |  build_border_node_graph (once)                |
                  |  graph: (R, link) -> (inside, crossings)       |
                  |  inside_rev, crossing_rev                       |
                  +------------------------------------------------+
                                    |
                                    v
                  +------------------------------------------------+
                  |  for each owned destination T:                 |
                  |    reversed Dijkstra from T                    |
                  |    -> dist[(R, link)] = shortest path to T     |
                  +------------------------------------------------+
                                    |
                                    v
                  +------------------------------------------------+
                  |  for each source R (other than T):             |
                  |    enumerate strict-decrease crossings        |
                  |    pick chosen_regions by 1 + n_dist_for_n min |
                  |    emit every crossing to a chosen region      |
                  |    cost = best_next_total                       |
                  +------------------------------------------------+
                                    |
                                    v
                  RegionRoutes { to: HashMap<T, RouteField> }      |
                  (output shape unchanged)
```

#### The problem the patch solves

A line A-B-C with B's interior cost 10. The old region-node model
summed the source's "border cost" with the destination's "entry cost"
per edge, so:

```text
old:
  A->B edge weight = A's "border-exit cost" + B's "border-entry cost" = 5 + 5 = 10
  B->C edge weight = B's "border-exit cost" + C's "border-entry cost" = 5 + 5 = 10
  cost_to_C(A) = 20    (DOUBLE-counts B's interior)
```

The new model makes the traversal an explicit `(R, entry) -> (R, exit)`
inside edge, charged once:

```text
new:
  (A, East) --1--> (B, West) --10--> (B, East) --1--> (C, West)
  dist[(A, East)] = 12    (correct)
```

Consumers (P-c, travel) read `from[R].exits`, not `from[R].cost`, so the
cost is informational and downstream routing is unaffected. The cost
field's meaning shifts from "full source-to-T cost" to "from-source's-
border-onward cost"; the source's own interior is excluded. Today
nothing reads the cost, so the drift is benign. A future patch that
consumes `RouteHop.cost` for budget must remember this semantic.

### Production-data fitness

`RegionState::road_report` (P-a, mod.rs:617-641) prices every distinct
`(entry_link, exit_link)` pair within each road network using real
`road_distance_to`, skipping self-pairs. So production actually emits
the middle-region traversal costs the new graph consumes. The tests
are not relying on data the runtime won't produce. Cross-network pairs
get no inside edge (they are not physically drivable), which is
correct and is exactly why parallel exits to the same next-hop region
matter for the frozen-token guarantee.

### Invariants the patch preserves

- **Loop safety / strict-decrease.** Every edge weight is >= 1 (inside
  `c.cost.max(1)`, crossing `1`). Every kept crossing has
  `n_dist < r_dist`. Distances strictly increase away from T, so no
  exit can cycle.
- **Frozen-token guarantee.** `chosen_regions` = next-hops at
  `best_next_total`; `all_exits` emits every strict-decrease crossing
  to any chosen region, not just the cheapest border. A token on a
  disconnected local net still has a reachable exit.
- **Determinism.** BTreeSet frontier; `owned_regions` sorted+deduped;
  cost via `min`; `all_exits.sort_by_key((to_region, edge, offset))`
  + `dedup_exits`. No HashMap iteration feeds output ordering.
- **Public output shape unchanged.** `RegionRoutes/RouteField/RouteHop/
  ExitLink` are intact. `exits_from` reads only `hop.exits`, never
  `hop.cost`. The cost field's semantic shift is informational and
  consumed by no routing decision today.

### Tests added (and why)

- `region_routes_cost_counts_middle_region_once` (new): a line A-B-C
  with B's `(West, East, 10)`. Asserts A's cost to C is 12, not 20.
  Pins the interior-once guarantee.
- `region_routes_emit_all_exits_to_chosen_next_hop_regardless_of_border_cost` (new):
  A has two borders both crossing to the same next-hop B, but B's
  matching West borders have different distances to T. Both A borders
  must be emitted. Pins the frozen-token guarantee.
- `region_routes_tie_break_is_deterministic` (new): A has two borders
  both crossing to T with identical cost. Both are emitted, sorted
  by (to_region, edge, offset). Pins tie-break determinism.
- `region_routes_asymmetric_inside_cost` (new): B has only
  `(West, East, 5)`, no reverse. The Dijkstra must follow the
  directed inside edge, not assume symmetry. Pins the
  reversed-Dijkstra direction.
- Existing tests `region_routes_map_multihop_destination_to_first_hop`,
  `region_routes_prefer_lower_cost_corridor`, `region_routes_skip_roadless_border`
  were updated to use `(entry, exit)` pairs in middle regions (B's
  `(West, East, X)`, D's `(North, South, X)`, E's `(North, West, X)`).
  The new model needs `entry != exit` to express interior traversal;
  leaf regions keep self-pair data.

### Known limitations (carried forward)

- The new model requires `(entry, exit)` data in middle regions
  (regions that have 2+ published border links). The producer
  (`RegionState::road_report`) already emits this; a region with only
  one border link is a leaf and produces only self-pairs (which the
  new model treats as a self-loop, contributing no progress — fine
  for leaves).
- `RouteHop.cost` semantic drift: now measures from the source's
  border onward, excluding the source's own interior. Today nothing
  reads the cost, so the drift is benign. Future consumers should
  treat it as advisory.
- Performance: per-destination Dijkstra over border nodes is
  `O(R^2 * b log)` (b = avg borders per region). Same order as the
  old region-node version at this scale; fine for the current city
  sizes.
- A `valid_crossings` field carries a `(BorderLinkId, RegionId,
  ExitLink)` tuple but the first field is unused at the filter site
  (line 678 `|(_link, n, exit)|`). Harmless; could drop the first
  field to a 2-tuple. Not worth a re-roll.
- Do not change `RegionRoadReport` unless implementation proves the existing
  `RegionBorderLink` + `RegionCrossCost` data is insufficient.
