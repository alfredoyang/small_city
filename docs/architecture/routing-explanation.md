# 20260630 L1/L2 Region Routing

## 1. Introduction / Problem

Cross-region travel has two pathfinding layers:

- **Layer 1 (L1):** region-to-region routing. It answers, "from region A, which
  border should I leave through to eventually reach region C?"
- **Layer 2 (L2):** local road-cell routing inside one region. It answers, "from
  this road cell, which next road cell gets me to that border or building?"

The split matters because a citizen token moves one local road cell at a time,
but the route target can be several regions away.

```text
final target = C

Layer 1:
  A --------> B --------> C
  "leave A via East/0 toward B"

Layer 2 inside A:
  home road -> road -> road -> East/0 border cell
```

The code keeps those responsibilities separate:

```text
RegionState::road_report         prices local border-to-border roads  (L2 data)
build_region_routes              assembles border-node graph + Dijkstra (L1 map)
RegionState::set_region_routes   converts L1 border links to road cells
travel::advance_to_exit          walks the token to the chosen road cell
```

## 2. Proposal / Mental Model

This document is the readable map of the current implementation.

### Big Picture

```text
                 DATA / ROUTE PREPARATION
                 ------------------------

  Region A                  Region B                  Region C
  --------                  --------                  --------
  road_report()             road_report()             road_report()
      |                         |                         |
      | RegionRoadReport        | RegionRoadReport        | RegionRoadReport
      |                         |                         |
      +-------------------------+-------------------------+
                                |
                                v
                    RegionDirectory snapshot
                    ------------------------
                    build_region_routes(...)
                    - assemble weighted border-node graph
                    - run reversed Dijkstra per destination
                    - produce RegionRoutes
                                |
                                v
                    RegionWorker, for each region
                    -----------------------------
                    exits_from(A)
                      C -> [ExitLink { link: East/0, to_region: B, cost: 7 }]
                                |
                                v
                    RegionState::set_region_routes(...)
                    -----------------------------------
                    East/0 -> local road cell r42
                                |
                                v
                    World.remote_exit_cells
                    -----------------------
                    C -> [
                      RouteExit {
                        cell: r42,
                        link: East/0,
                        to_region: B,
                        cost: 7,
                      }
                    ]
```

The route-preparation side converts published local road reports into the exact
local exits a token can walk toward.

```text
                    RUNTIME MOVEMENT
                    ----------------

Citizen token in A wants final workplace in C

  travel::advance_to_exit(...)
          |
          | reads world.remote_exit_cells[C]
          v
  chooses cheapest RouteExit by:
    weighted local road cost to cell + RouteExit.cost
          |
          | Layer 2 local walking
          v
  home road -> road -> road -> r42
          |
          | reached border cell
          v
  PendingHandoff::Move
    to_region = B
    exit_cell = r42
    exit_link = East/0
          |
          | worker routes message
          v
  Region B receives token at matching border cell
          |
          | repeat same mechanism for final target C
          v
  Region C
```

Same data, different jobs:

```text
RegionRoadReport
  "what roads/border prices does this region publish?"

RegionRoutes / ExitLink
  "for final target C, which border link leaves A on the best next hop,
   and what does that exit cost after crossing?"

RouteExit
  "which local road cell should the token walk to, where does it cross,
   and what remains after that crossing?"
```

Short version:

```text
ExitLink  = which border to use + next region + remaining route cost
RouteExit = ExitLink + the concrete local road cell to walk to
```

```text
RegionDirectory / L1
  ExitLink { link: East/0, to_region: B, cost: 10 }
        |
        | RegionState::set_region_routes maps East/0 to road cell r42
        v
World.remote_exit_cells / L2
  RouteExit { cell: r42, link: East/0, to_region: B, cost: 10 }
```

### L2: Local Pricing Inside One Region

For one region, L2 is just normal road routing over local road cells.

```text
Region B

             North/0 -> D
                  |
        +---------+---------+
        |                   |
West/0  +--- local roads ---+  East/0
 -> A   |                   |   -> C
        +-------------------+

RegionBorderLink:
  West/0  -> A
  East/0  -> C
  North/0 -> D

RegionCrossCost:
  West/0  -> East/0  = 8
  West/0  -> North/0 = 3
  North/0 -> East/0  = 6
```

`RegionState::road_report(...)` produces that report. It uses
`World::road_distance_to(...)`, which calls `road_predecessors_with_dist(...)`;
it does **not** use `World::routes_to(...)` or the route cache.

### L1: Border-Node Dijkstra

The directory turns local reports into a weighted graph whose nodes are
`(region, border_link)`, not just `region`.

```text
Published border matches:

  A East/0  <->  B West/0
  B East/0  <->  C West/0
  A South/0 <->  D North/0
  D East/0  <->  C South/0

Border-node graph:

  (A, East/0)  --cross 1--> (B, West/0)  --inside B--> (B, East/0)  --cross 1--> (C, West/0)

  (A, South/0) --cross 1--> (D, North/0) --inside D--> (D, East/0) --cross 1--> (C, South/0)
```

There are only two edge types:

```text
inside-region:
  (R, entry_link) -> (R, exit_link)
  weight = RegionCrossCost { entry, exit, cost }.cost.max(1)

border-crossing:
  (R, local_link) -> (N, local_link.matching_neighbor_link())
  weight = 1
  payload = ExitLink { link: local_link, to_region: N, cost: 1 + dist[(N, matching)] }
```

Example weights:

```text
Goal: reach C

  via B:
    A East/0 --1--> B West/0 --8--> B East/0 --1--> C West/0
    total = 10

  via D:
    A South/0 --1--> D North/0 --5--> D East/0 --1--> C South/0
    total = 7
```

For destination `C`, Dijkstra seeds every C border node at `0`, then walks the
reversed graph:

```text
dist[(C, West/0)]  = 0
dist[(C, South/0)] = 0

dist[(B, East/0)]  = 1
dist[(B, West/0)]  = 9

dist[(D, East/0)]  = 1
dist[(D, North/0)] = 6

dist[(A, East/0)]  = 10
dist[(A, South/0)] = 7
```

So A chooses D, not B:

```text
Route field for final target C:

  from A -> RouteHop {
              exits: [ExitLink { link: South/0, to_region: D, cost: 7 }]
            }
  from D -> RouteHop {
              exits: [ExitLink { link: East/0, to_region: C, cost: 2 }]
            }
  from B -> RouteHop {
              exits: [ExitLink { link: East/0, to_region: C, cost: 2 }]
            }
  from C -> no exit
```

This is also the loop-safety rule: every hop must strictly decrease the Dijkstra
distance at the border node.

```text
(A, South/0)(7) -> (D, North/0)(6)   valid: 6 < 7
(D, East/0)(1)  -> (C, South/0)(0)   valid: 0 < 1
(D, North/0)(6) -> (A, South/0)(7)   rejected: 7 is not lower than 6
```

The selector chooses next-hop region(s) by the best region distance, then emits
every strict-decrease exit to those chosen region(s). This matters when one
region has disconnected local roads:

```text
Goal: A -> C, chosen next-hop region is B

  A East/0 -> B West/0 -> C   cheap
  A East/1 -> B West/1 -> C   valid but more expensive

Both A exits are published:
  remote_exit_cells[C] includes East/0 and East/1

Layer 2 then picks the cheapest reachable exit for the token:

  weighted local road cost to RouteExit.cell + RouteExit.cost

The local cost uses the same `step_cost` weighting as movement (turn/T-junction
= 2, four-way = 4), not raw hop count.
```

### L1 To L2 Adapter: `set_region_routes`

L1 returns border links. Local movement needs road cells.

```text
L1 answer for source A:

  final target C -> ExitLink { link: South/0, to_region: D, cost: 7 }

Local border roads in A:

  South/0 -> [road cell r42]

Stored movement answer:

  remote_exit_cells[C] = [
    RouteExit { cell: r42, link: South/0, to_region: D, cost: 7 }
  ]
```

`RouteExit` deliberately carries all three pieces:

```text
cell      = local L2 walking target
link      = sender-side border link for handoff placement
to_region = immediate next-hop region for worker routing
cost      = remaining L1 cost after using this exit
```

```text
RegionDirectory / L1
  ExitLink { link, to_region, cost }
        |
        | RegionState::set_region_routes maps border link -> local road cell
        v
World.remote_exit_cells / L2
  RouteExit { cell, link, to_region, cost }
        |
        | travel walks token to cell
        v
PendingHandoff::Move { exit_cell: cell, exit_link: link, to_region }
```

That prevents the old duplicate routing path. The border crossing uses the same
L1 choice the directory made; it does not re-derive a neighbour from a separate
direct-neighbour map.

### Concrete Sample: A Can Reach C Via B Or D

```text
Goal: citizen in A works in C

Weights:

  A East/0  -> B West/0  = 1 crossing
  B West/0  -> B East/0  = 8 inside B
  B East/0  -> C West/0  = 1 crossing

  A South/0 -> D North/0 = 1 crossing
  D North/0 -> D East/0  = 5 inside D
  D East/0  -> C South/0 = 1 crossing

Totals:

  A -> B -> C = 10
  A -> D -> C = 7

Chosen:

  A exits to D
```

Flow:

```text
Directory route:
  to[C].from[A].exits = [ExitLink { link: South/0, to_region: D, cost: 7 }]

Region A install:
  South/0 resolves to road cell r42
  remote_exit_cells[C] = [RouteExit { cell: r42, link: South/0, to_region: D, cost: 7 }]

Travel step:
  token walks home -> ... -> r42
  at r42:
    PendingHandoff::Move {
      to_region: D,
      exit_cell: r42,
      exit_link: South/0,
      token,
    }

Region layer:
  TravelerHandoff { to_region: D, entry_link: South/0, ... }

Worker:
  routes handoff to region D's owner
```

Then D repeats the same mechanism:

```text
Region D has:
  remote_exit_cells[C] = [RouteExit { cell: d_exit, link: East/0, to_region: C, cost: 2 }]

Token in D walks to d_exit and crosses to C.
```

## 3. Important Functions And Structures

`src/core/regions/mod.rs`
- `RegionBorderLink` — one local `BorderLinkId` plus the neighbour it reaches.
- `RegionCrossCost` — L2 cost from one local border link to another.
- `RegionRoadReport` — one region's published L2 border pricing.
- `RegionRoutes` / `RouteField` / `RouteHop` / `ExitLink` — L1 output.
- `RouteExit` — local movement-ready exit: `{ cell, link, to_region, cost }`.
- `RegionState::road_report(...)` — builds the per-region L2 report.
- `RegionState::set_region_routes(...)` — converts L1 `ExitLink`s into local
  `RouteExit`s.

`src/core/regions/directory.rs`
- `RegionDirectory::publish_region_road_report(...)` — stores one region report
  and rebuilds the snapshot when it changes.
- `build_region_routes(...)` — assembles the weighted border-node graph and runs
  one destination-rooted reversed Dijkstra per target region.
- `RegionDirectory::exits_from(region)` — returns this region's
  `HashMap<target_region, Vec<ExitLink>>`.

`src/core/regions/worker.rs`
- `border_neighbor_map_for_region(...)` — builds direct border-neighbour facts
  for `RegionState::road_report(...)` pricing only. It is not the travel route.
- `RegionWorker::process_region_events_with_mode(...)` — publishes reports and
  installs `directory.exits_from(source_region)` into the runtime.

`src/core/regions/runtime/mod.rs`
- `RegionRuntime::set_region_routes(...)` — passes L1 exits to `RegionState`.

`src/core/systems/travel.rs`
- `depart_toward(...)` — for remote targets, picks a reachable
  `remote_exit_cells[target.region]` candidate by
  `weighted local road distance + exit.cost`.
- `advance_to_exit(...)` — walks toward the selected `RouteExit.cell`; on the
  border cell, emits a `PendingHandoff::Move` carrying `exit.link` and
  `exit.to_region`. A committed exit is kept while reachable to avoid
  mid-route oscillation.

`src/core/components.rs`
- `PendingHandoff::Move { exit_cell, exit_link, to_region, ... }` — buffered
  crossing decision.
- `TravelerHandoff { entry_link, to_region, ... }` — worker-routed crossing
  message.

## 4. Pseudocode / Integration

Report publishing:

```rust
// RegionWorker, after event processing / derived-state refresh
let border_neighbours = border_neighbor_map_for_region(topology, region, links, is_owned);
let report = runtime.state().road_report(&border_neighbours);
directory.publish_region_road_report(report);
```

L1 route assembly:

```rust
// RegionDirectory snapshot rebuild
node = (RegionId, BorderLinkId);

inside edge:
    (R, entry) -> (R, exit), weight = RegionCrossCost.cost.max(1)

crossing edge:
    (R, link) -> (N, link.matching_neighbor_link()), weight = 1

for destination in owned_regions {
    seed every (destination, border_link) at 0;
    dist = dijkstra_over_reversed_border_node_graph();

    for source in owned_regions {
        enumerate every strict-decrease crossing from source borders;
        choose next-hop region(s) with minimum 1 + min_dist_for_region;
        emit every strict-decrease exit to those chosen next-hop region(s)
            as ExitLink { link, to_region, cost: 1 + dist[(to_region, entry)] };

        routes.to[destination].from[source] = RouteHop { exits };
    }
}
```

Install routes into one region:

```rust
let exits_from_a = directory.exits_from(A).unwrap_or_default();
runtime.set_region_routes(&exits_from_a);

// RegionState::set_region_routes
cells_by_link = border_road_links(); // BorderLinkId -> local road cells

for (target, exit_links) in exits_from_a {
    for exit in exit_links {
        for cell in cells_by_link[exit.link] {
            remote_exit_cells[target].push(RouteExit {
                cell,
                link: exit.link,
                to_region: exit.to_region,
                cost: exit.cost,
            });
        }
    }
}
```

Move a token:

```rust
let candidates = world.remote_exit_cells[target.region];
let exit = cheapest reachable candidate by:
    World::road_distance_to(exit.cell, current_or_depart_entry, network) + exit.cost;

if already walking to a still-reachable committed exit:
    keep it; // avoids oscillation when route costs change mid-trip

walk toward exit.cell;

if current_cell == exit.cell {
    emit PendingHandoff::Move {
        to_region: exit.to_region,
        exit_cell: exit.cell,
        exit_link: exit.link,
        token,
    };
}
```

Cross the border:

```rust
// Sender region
resolve_pending_traveler_handoffs:
    assert exit_link still resolves to exit_cell;
    send TravelerHandoff {
        to_region,
        entry_link: Some(exit_link),
        token,
    };

// Receiver region
receive_traveler_handoff:
    local_link = entry_link.matching_neighbor_link();
    entry_cell = cell_at_border_link(local_link);
    receive_traveler(token, entry_cell);
```

## 5. Tests

Existing tests that anchor this routing model:

- `region_routes_map_multihop_destination_to_first_hop`
- `region_routes_pick_only_cost_decreasing_neighbour`
- `region_routes_prefer_lower_cost_corridor`
- `region_routes_skip_roadless_border`
- `region_routes_cost_counts_middle_region_once`
- `region_routes_emit_all_exits_to_chosen_next_hop_regardless_of_border_cost`
- `region_routes_asymmetric_inside_cost`
- `region_routes_exit_links_carry_per_exit_cost`
- `remote_exit_choice_prefers_cheapest_reachable_exit`
- `remote_exit_choice_uses_weighted_local_distance`
- `remote_exit_keeps_committed_exit_when_still_valid`
- `set_region_routes_populates_remote_exit_cells`
- `set_region_routes_empty_routes_clear_remote_exit_cells`
- `drain_move_resolves_border_link`
- `cross_region_commuter_goes_to_work_and_returns_home`

## 6. Risks / Non-goals

- L1 costs are exact for the published border-node graph, not for every possible
  future traffic model. Congestion or time-varying costs would need new report
  fields.
- The direct border-neighbour facts still exist for report pricing. They are not
  a travel-routing source.
- Cross-region route snapshots are stale-tolerant. If a road disappears after a
  token selected an exit, the handoff path rolls back instead of stranding.
- This doc does not propose new routing behavior; it explains the current L1/L2
  interaction.
