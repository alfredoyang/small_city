# Regional Resource Registry Plan

This plan introduces a shared way to register resource providers and consumers
(power first, jobs next, future resources later) so that local allocation and,
eventually, cross-region export read one source of truth instead of each system
re-deriving road-network adjacency and allocation.

It supersedes the discarded first attempt at cross-region power
(`regional-cross-region-power-import-plan.md` Patch P1), whose duplication of the
power allocation was the symptom that motivated this foundation.

## Motivation (current state)

- **Power** has explicit components (`PowerProvider`, `PowerConsumer`), but
  `systems/power.rs` re-derives allocation every tick: discover road networks,
  sum adjacent provider capacity, allocate to adjacent consumers in map order,
  set `powered`.
- **Jobs** have no provider/consumer components. `systems/stats.rs` sums
  `kind.jobs_at_level()` and `systems/economy.rs` rebuilds workplace slots each
  tick and assigns citizens to the nearest effective slot.
- The discarded cross-region power patch re-derived the power allocation a third
  time to compute spare capacity.

Three independent derivations of the same provider/consumer/road-network idea is
the problem this plan removes.

## Connectivity semantics differ (design constraint)

- **Power is road-network-scoped**: a consumer draws only from providers on its
  own connected road network. Disconnected networks never share capacity.
- **Jobs are proximity-scoped**: a citizen takes the nearest *effective*
  workplace city-wide, and the assignment carries identity (which citizen works
  where) used for salary, rent, and tax.

So the registry unifies the shared plumbing (registration, effective-gating,
deterministic map order, remaining-capacity accounting) while each resource keeps
its own matching rule. It is not a single generic allocator.

## Decision Record

- One registry entry per resource kind per region (a `Power` entry, a `Jobs`
  entry, ...). The entry is the region-level container; it still accounts per
  road network internally where the resource requires it.
- Implement local power onto the registry first, with strict behavior parity,
  before touching jobs or cross-region.
- Power allocation is modeled as a **request -> grant -> record** flow with one
  shared data shape (`PowerRequest`, `PowerGrant`, `PowerSource`). Local power
  resolves this flow **synchronously in-tick** (a deterministic pass, no event
  queue); cross-region routes the same requests and grants over region-runtime
  events. Unify the data shape, not the transport.
- Each `PowerConsumer` records its granting source, so spare capacity and
  imports/exports are derivable from authoritative state.

## Power Resolution Protocol

The protocol is the same concept everywhere; only the transport differs by
distance.

Data:

```text
PowerConsumer { powered, demand, source: Option<PowerSource> }
PowerSource   = Local(provider_entity)
              | Imported { source_region, border_link }   // owned ids, no remote ECS
PowerRequest  { consumer, demand, road_network }
PowerGrant    { consumer, source, amount }
```

Flow, run as a bounded **power-resolution phase inside the tick**, before any
downstream system that reads `powered` (population, economy, happiness, local
effects):

1. Reset: every consumer `powered = false, source = None`; provider remaining =
   capacity.
2. Emit one `PowerRequest` per consumer in deterministic map order (y then x),
   the same order today's shortage handling uses.
3. Process requests in that order. A request is granted by a provider reachable
   on the consumer's own road network with remaining capacity; on grant set
   `powered = true`, `source = Local(provider)`, and subtract demand.
4. Later (cross-region): a request that a border link exposes is routed to the
   neighbor region through the existing region-runtime neighbor request/continuation
   events; the neighbor's provider confirms a grant and the consumer records
   `source = Imported { .. }`.

Hard requirements the implementation must hold:

- **Determinism.** Requests are processed in stable map order, so shortage
  outcomes match today. An event queue does not remove this; the request batch is
  ordered before processing.
- **Intra-tick completion.** The resolution phase fully drains (local grants, and
  later cross-region confirms) before downstream systems run in the same tick.
- **Local resolution is synchronous, not events.** Local power is resolved by a
  single deterministic in-tick pass (a function call over consumers in map order)
  that produces `PowerGrant`s and writes each consumer's `source`. There is no
  local event queue. Only cross-region requests and grants travel as region-runtime
  events, since those genuinely cross actor boundaries. Same data shape, two
  transports.
- **Network scoping preserved.** A local request is only satisfiable by providers
  on the same connected road network; disconnected networks never share capacity.

## Goals

- Provide a single place where providers and consumers are registered per region.
- Make "remaining/spare capacity" a first-class, queryable value per resource.
- Preserve the existing power rule that disconnected road networks cannot share
  capacity.
- Keep `World` private to core; keep the simulation deterministic.
- Refactor without changing observable behavior until a later, explicit patch.

## Non-Goals

- Do not change power or jobs balance in the foundation patches.
- Do not build cross-region export in this plan's first patches; this is the
  foundation cross-region work will read.
- Do not collapse power and jobs into one generic allocator.
- Do not add external dependencies.

## Target Shape

```text
ResourceRegistry            (per region, rebuilt from authoritative state)
  Power entry
    providers : registered capacity sources
    consumers : registered demand sinks
    per-network pools -> allocation result + remaining capacity
  Jobs entry
    providers : registered workplace slots
    consumers : registered job seekers (citizens)
    effective slots -> assignment result + remaining slots
```

Each resource keeps its own matching rule; the registry owns registration,
gating, ordering, and remaining accounting.

## Patch R1: Power Onto The Registry (Local, Request/Grant, Parity-Preserving)

Goal: introduce the registry with a Power entry and resolve local power through a
synchronous request -> grant -> record pass, recording each consumer's source,
with no observable behavior change.

Likely files:

- `src/core/` new module, for example `resource_registry.rs`
- `src/core/components.rs` (add `PowerConsumer.source` + `PowerSource`)
- `src/core/systems/power.rs`
- `src/core/mod.rs`
- `tests/power_test.rs` and/or `tests/power_network_integration_test.rs`

Implementation:

- Add `PowerSource` and a `source: Option<PowerSource>` field on `PowerConsumer`
  (serde-defaulted so existing saves load; it is recomputed each tick).
- Add a Power registry entry that registers `PowerProvider` and `PowerConsumer`
  entities for a region's world.
- Resolve power through the request/grant protocol described above: emit one
  request per consumer in map order, grant from a same-network provider with
  remaining capacity, set `powered` and `source = Local(provider)`.
- Run it as a dedicated resolution phase; `power::run` builds the registry and
  applies grants instead of scanning inline. Keep `world.stats.power` totals
  identical.
- Preserve per-network accounting: disconnected networks keep separate pools.
- Keep the registry and source field computed from authoritative state
  (rebuildable, not new saved truth, no ECS identity leaked outside core).

Tests:

- existing power and power-network integration tests pass unchanged (parity)
- each powered consumer records a `Local` source pointing at a same-network
  provider; unpowered consumers have `source = None`
- shortage outcome (which consumers are powered) matches today, by map order
- disconnected networks do not share capacity
- existing single-city saves load with the defaulted `source` field

Review focus:

- No observable behavior change; `powered` flags and power stats are identical.
- Request processing is deterministic and map-ordered; shortage matches today.
- Per-network isolation preserved.
- `source` is derived state, not authoritative save data.

## Patch R2: Jobs Onto The Registry (Local, Parity-Preserving)

Goal: register job providers (workplace slots) and consumers (citizens) in a Jobs
entry and route the existing assignment through it, with no balance change.

Likely files:

- `src/core/resource_registry.rs`
- `src/core/systems/economy.rs`
- `src/core/systems/stats.rs`
- `tests/economy_test.rs`, `tests/population_test.rs`

Implementation:

- Add a Jobs entry that registers effective workplace slots and job-seeking
  citizens.
- Route the nearest-slot assignment and the `jobs`/`unemployment` stats through
  the entry, keeping proximity-based matching and citizen identity intact.
- Do not change salary, rent, tax, or assignment outcomes.

Tests:

- existing economy and population tests pass unchanged (parity)
- the Jobs entry reports the same `jobs` and `unemployment` totals as today
- remaining job slots are queryable per region

Review focus:

- Assignment identity and economy outputs are unchanged.
- Jobs proximity rule is preserved; only registration plumbing moved.

## Patch R3: Region-Level Spare Capacity Query

Goal: expose owned, ECS-free per-region spare capacity for power and jobs so
later cross-region work can read it without re-deriving anything.

Likely files:

- `src/core/resource_registry.rs`
- `src/core/regions/mod.rs`
- `tests/regional_state_test.rs`

Implementation:

- Add a read-only accessor that returns spare power capacity and spare job slots
  for the region, derived from the registry.
- Keep the output owned summary data with no ECS entities or world handles.

Tests:

- spare capacity matches registry remaining after local allocation
- output contains no ECS entity identity

Review focus:

- The query is read-only and rebuildable.
- No `World` leakage.

## Patch R4 And Later: Cross-Region On The Registry

Goal: re-attempt cross-region power and jobs by reading the registry's spare
capacity at border links, replacing the discarded standalone derivation.

This reuses the cross-region design notes
(`regional-cross-region-power-import-plan.md`) for border identity, request and
reply data, and stale-generation handling, but sources capacity from the registry
instead of a parallel computation.

## Guardrails

- Each patch is one mission, stays within roughly five files and 400 changed
  lines, and includes tests.
- Foundation patches (R1, R2) must be behavior-preserving; parity is proven by
  existing tests passing unchanged.
- `World` stays private to core. Registry output crossing any boundary is owned
  summary data without ECS identity.
- Determinism: stable road-network discovery, map-order allocation, integer math.
- No new external dependencies.

## Review Checklist

- Does the change keep `powered`/stats/economy outputs identical in foundation
  patches?
- Is per-network power isolation preserved?
- Is allocation deterministic and map-ordered?
- Is registry output owned and free of ECS identity?
- Did `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test` pass?
