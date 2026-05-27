# Regional Imported Resource Threading Model

This note records a possible future threading model for Small City. It is an idea document only; it does not change the current single-region simulation. The goal is to explore multithreading without exposing the ECS `World` across threads or requiring the whole map to be globally deterministic.

## Summary

Split the city into regions. Each region owns its own authoritative simulation and can run on one worker thread. A region does not read or mutate another region's ECS world. Instead, neighboring regions exchange small imported resource offers across connected border roads.

Inside one region, behavior can remain deterministic and single-threaded:

- The region owns its local ECS world.
- Local systems update in a fixed order.
- Local citizens, buildings, roads, money, power, and other authoritative state belong to that region.
- The UI still reads through the `Game` API and view models, not directly from ECS internals.

Between regions, behavior can be eventually consistent:

- Resource offers propagate gradually through neighboring regions.
- A region may use slightly stale imported resource data.
- Cross-region effects do not need to resolve on the same tick everywhere.
- A distant change can spread through the map over several ticks or game days.

## Region Boundary

Each region should export only a compact boundary contract, not its internal world.

For example, if Region B has a park or jobs reachable from a road connected to Region A, Region B can export an offer such as:

```text
origin region: B
resource kind: park access
capacity: 12
border road: west edge road 3
travel cost: 4
generation: 8
```

Region A treats the connected road end as an external access point. It does not need to know where the park or job provider exists inside Region B.

## Imported Resource Identity

Every exported resource needs a stable identity so regions can reject duplicates and avoid echoed supply.

Suggested identity:

```text
ResourceOfferId:
  origin_region
  local_resource_id
  generation
```

For an aggregated first version, the identity can be simpler:

```text
ResourceOfferId:
  origin_region
  resource_kind
  generation
```

The `generation` changes when the origin region's exported resource changes. For example, if Region E builds a new park, removes a park, changes road access, or changes job capacity, Region E emits a new generation. Neighboring regions should treat the new generation as a replacement for the older one.

Important rule:

```text
Imported resources keep their original ResourceOfferId forever.
```

If Region A imports `E:park:17`, Region A must not re-export it as `A:park:22`. Rewriting the origin would make the same resource look like new supply and can create echo loops.

## Propagation Rule

When a region receives an imported offer:

1. If the same `ResourceOfferId` is already known, reject the duplicate.
2. If it is new, store it as imported supply.
3. Use part of the capacity locally if local citizens can reach the border access point.
4. Forward the remaining capacity to other neighboring regions.
5. Do not forward it back to the neighbor that just sent it.
6. Stop forwarding when the offer reaches `max_hops` or has no remaining capacity.

Example:

```text
Region E exports park offer E:park:17 with capacity 100.
Region A receives E:park:17, uses 30, and forwards 70.
Region B receives E:park:17 from A and stores it.
Region B later receives E:park:17 from C and rejects it as duplicate.
```

This keeps propagation simple while preventing the same external park or job source from being counted repeatedly through multiple routes.

## Capacity And Decay

A forwarded imported resource should carry remaining capacity and path cost.

Suggested data:

```text
ImportedResourceOffer:
  id
  kind
  remaining_capacity
  hop_count
  max_hops
  travel_cost
  source_neighbor
```

The first version can use simple integer rules:

```text
remaining_capacity = imported_capacity - locally_used_capacity
travel_cost = previous_travel_cost + border_crossing_cost
hop_count = previous_hop_count + 1
```

If far-away resources should be weaker, add decay:

```text
remaining_capacity = remaining_capacity - decay_per_hop
```

Keep the formula deterministic inside each region. Cross-region arrival order may vary, but each region's local update from its accepted imported offers should stay predictable.

## Duplicate Handling

The simplest duplicate rule is:

```text
same ResourceOfferId already exists -> reject
```

This is easy to understand and avoids echo loops.

A future improvement is best-known route selection:

```text
same ResourceOfferId and newer generation -> replace
same generation but lower travel cost -> replace
same generation but higher remaining capacity -> maybe replace
otherwise -> reject
```

The first implementation should probably use simple rejection unless gameplay shows that worse first-arrival routes are a real problem.

## Threading Shape

Each region can run independently:

```text
Region worker:
  receive imported offers from neighbors
  apply accepted offers at tick boundary
  run local deterministic systems
  build exported local offers
  forward accepted imported offers
  publish outbound messages
```

A coordinator can pass messages between region workers:

```text
Coordinator:
  owns region topology
  moves outbound messages to neighbor inboxes
  does not inspect region ECS worlds
  does not mutate region-local state
```

This keeps the important ownership rule:

```text
No region mutates another region.
```

## What Should Cross Region Boundaries

Good candidates:

- job access summaries
- park access summaries
- service access summaries
- shopping access summaries
- road exit reachability
- approximate traffic pressure

Riskier candidates:

- exact money transfer
- entity ownership
- citizen identity migration
- building construction
- save/load authority
- anything that needs immediate global consistency

For a first version, imported resources should be treated as access or capacity, not as remote ECS entities.

## Save And Rebuild

Imported offers should be considered rebuildable cache.

Save authoritative region state:

- local buildings
- local roads
- local citizens
- local resources
- local resource generation counters

Avoid treating imported offers as permanent truth. On load, regions can rebuild exported offers from local state and propagate them again.

## Open Risks

- A region may accept a weaker route first and reject a better duplicate later.
- Capacity accounting is approximate when several regions consume the same origin resource.
- Cross-region behavior may vary depending on message arrival order.
- Long propagation ranges can make balance harder to understand.
- Debugging needs good tools to inspect which imported offer a region accepted and where it came from.

## Suggested First Mission

Do not implement full threaded regions first. A smaller first mission would be:

```text
Add single-threaded region resource propagation data structures and tests.
```

That mission could test:

- a region accepts a new imported offer
- a region rejects the same `ResourceOfferId`
- a region forwards remaining capacity
- a region does not forward back to the sender
- a newer generation replaces or coexists according to the chosen rule

After the data model is stable, a later mission can move region execution to workers.
