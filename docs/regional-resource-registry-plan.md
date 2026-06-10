# Regional Resource Registry And Cross-Region Sharing Plan

This plan introduces a shared way to register resource providers and consumers so
that local allocation and cross-region sharing read one source of truth instead of
each system re-deriving road-network adjacency and allocation. Power comes first,
jobs next, and every other building-derived `ResourceKind` follows the same model.

Three phases:

- **Local registry (R1-R3, done):** power and jobs resolve through a per-region
  registry that records each consumer's source and exposes spare capacity.
- **Cross-region sharing (R4 = CR1-CR6):** regions share spare over connected
  roads via a discovery directory plus authoritative producer-owned export
  allocation over the existing region-runtime event flow.
- **Persistent registry (R5, deferred):** recompute on change instead of per tick.

It supersedes a discarded first attempt at cross-region power that recomputed
export capacity independently of the local power system; that duplication was the
symptom that motivated this foundation.

For definitions of the types and terms used below (import vs export convention,
topology, components, export allocations, generations), see
[regional-terminology.md](regional-terminology.md).

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
- Cross-region sharing **follows roads**: only regions in the same connected
  cross-region road network may share. Connectivity is computed at
  `(region, road-network)` granularity, never region granularity.
- Cross-region sharing splits into **discovery** (a stale-tolerant component graph
  plus an availability hint) and **export allocation** (an authoritative
  request/grant over the existing region-runtime event flow with producer-owned
  allocation). Determinism lives in the event flow, not the hint.
- All `ResourceKind`s use this one registry + discovery model; the earlier
  visibility-only push cache is retired (CR6).

## Local Power Resolution Protocol

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
4. Cross-region (R4): a request a border link exposes is routed to a producer in
   the same road component over the region-runtime event flow; the producer
   confirms a grant and the consumer records `source = Imported { .. }`.

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

## Cross-Region Sharing Model (R4)

Cross-region sharing splits into **discovery** and **export allocation**. Discovery is a
lookup, not a routed search: a stable component graph (which regions share a road
network) plus a tiny, stale-tolerant **availability hint** per region (does it
have spare). A locally-short consumer uses these to pick a producer in its own
component. The **export allocation** is then an authoritative request over the
existing cross-region event flow: the producer grants from its own runtime and
records transient export allocation, which is the single source of truth. There
is no hop-by-hop forwarding and no "which direction" decision, so any topology
(line, ring, 5x5 grid) works the same. Because the hint is only used to choose
whom to ask, it may be stale: a wrong guess costs one declined request, never a
wrong allocation.

Two separate pieces; keep them distinct.

1. Stable component graph (changes only on road/border topology change):

```text
RegionRoadNetworkId { region: RegionId, road_network: u32 }   // owned, not an ECS entity
BorderLinkId        { edge: BorderEdge, offset: usize }
BorderEdge          = North | South | West | East

union-find over RegionRoadNetworkId nodes:
  join two networks when their regions share a complementary border cell
  (A.East offset y  <->  B.West offset y) that is a road on each side
        |
        v
components: which (region, network) nodes share one cross-region road network
```

2. Availability hint (volatile, tiny, stale-tolerant):

```text
per RegionRoadNetworkId (or per region):
  has_spare_power: bool      (or a coarse amount)
  has_spare_jobs:  bool
```

The hint is the only thing published frequently. It is so small that a worker can
swap it cheaply -- a relaxed atomic for a scalar, or a double-buffer/seqlock for a
small struct -- and other workers may read it stale without harm.

Why network granularity matters (the trap that makes region-level wrong):

```text
B holds two disconnected networks:
   A.net -- border -- B1          (B1 and B2 are NOT connected inside B)
   C.net -- border -- B2
region-level union  -> {A, B, C} : A wrongly draws from C
network-level union -> {A.net, B1}  and  {B2, C.net} : A cannot reach C  (correct)
```

Discovery then export allocation (power; jobs are analogous with slots instead of capacity):

```text
short consumer on (region, net), after local grants:
    component  = component_graph.component_of(net)
    candidates = regions in component whose hint says "has spare"   [stable order]
    for producer in candidates:
        send an authoritative request over the cross-region event flow
        producer's event loop grants if local remaining capacity
            minus active export allocations can satisfy demand
        producer records transient export allocation and replies <- source of truth
        on grant: consumer.powered = true; source = Imported { producer.region }
        on a stale "has spare" that is actually empty: try the next candidate
        stop when demand is met
```

Concurrency and determinism:

- The hint may be read stale across worker threads; a wrong guess only wastes a
  request the producer declines, so the hint needs no barrier or consistency.
- Export allocation is authoritative and deterministic because the producer
  serializes requests in its own event loop and records active allocations as it
  grants. Cross-worker request ordering is the existing event flow's
  responsibility, not the hint's.

Save and rebuild: imported resources are rebuildable cache. On load each region
re-resolves locally, the component graph and hints rebuild, and imports
regenerate. Nothing imported is saved as authoritative truth.

## Goals

- Provide a single place where providers and consumers are registered per region.
- Make remaining/spare capacity a first-class, queryable value per resource.
- Preserve the existing power rule that disconnected road networks cannot share
  capacity.
- Keep `World` private to core; keep the simulation deterministic.
- Refactor local power and jobs without changing observable behavior in the
  foundation patches.
- Cross-region sharing follows roads: only regions in the same connected
  cross-region road network may share.
- Only owned summary data crosses a region boundary (component graph, availability
  hint, request/reply); no remote road, building, citizen, or power entity is
  shared.
- Confirm exported capacity before the caller marks buildings powered or assigns
  residents to remote jobs. For power, producer-owned export allocation prevents
  double-spend.
- Make imported resources and their blockers visible through inspect notes and
  tick summaries.

## Non-Goals

- Do not change power or jobs balance in the foundation patches.
- Do not collapse power and jobs into one generic allocator.
- Do not merge road graphs across regions. The component graph matches border-link
  *summaries*; it never shares road entities or reads another region's `World`.
- Do not make imported/cache state authoritative save data; it is rebuildable.
- Do not model per-border transit capacity. Connectivity is binary (same component
  may share, transit is unlimited); this avoids a min-cost-flow problem.
- Do not add true parallelism changes as part of this mechanic.
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
  other ResourceKind entries (service, shopping, park, road access, ...)
    same pattern: registered producers/consumers -> result + remaining
```

Each resource keeps its own matching rule; the registry owns registration,
gating, ordering, and remaining accounting. Power and jobs come first (R1/R2);
every other `ResourceKind` follows the same shape, and cross-region sharing (R4)
reads each entry's remaining the same way. This replaces the earlier
visibility-only push cache, which is retired once the registry path exists (CR6).

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

## Patch R4: Cross-Region Sharing (CR1-CR6)

R4 is the cross-region phase. It reads each registry entry's remaining (R3),
builds the discovery directory described in "Cross-Region Sharing Model" above,
and resolves producer-owned export allocation over the region-runtime event flow.
It lands as the sub-patches below.

### Patch CR1: Component Graph And Availability Hint

Goal: build the cross-region road-network component graph from border-link
summaries, and publish a tiny per-region availability hint. Discovery data only;
no export allocation and no tick behavior change.

Likely files:

- `src/core/resource_registry.rs` (per-network spare -> a `has_spare` hint)
- `src/core/regions/mod.rs` (`RegionRoadNetworkId`, `BorderLinkId`, border links
  per network)
- `src/core/regional_game_runner.rs` or `src/core/regions/worker.rs` (component
  graph + hint publication)
- `tests/regional_state_test.rs`, `tests/region_worker_test.rs`

Implementation:

- Add `BorderEdge`/`BorderLinkId` and report each local network's border links as
  owned summaries.
- Build a union-find over `RegionRoadNetworkId` joined by matched complementary
  border links to get the component graph.
- Derive a small availability hint per `RegionRoadNetworkId` (or per region) from
  the registry's spare: `has_spare_power` / `has_spare_jobs` (bool or coarse
  amount).
- Publish the hint so other workers can read it (a relaxed atomic for a scalar, a
  double-buffer/seqlock for a small struct). The hint may be read stale; the
  component graph and hint are owned summaries with no ECS identity.
- Do not perform any export allocation here.

Tests:

- two regions sharing a border road land in one component; mismatched edges do
  not join.
- a region with two disconnected networks linking different neighbors keeps those
  components separate (the trap above).
- the hint reports `has_spare` consistently with the registry's spare; it is owned
  and free of ECS identity.

Review focus:

- Components are keyed by `(region, road-network)`, not region.
- The hint is minimal and stale-tolerant; no consistency barrier is required.
- Discovery data is owned and deterministic; no export allocation happens in CR1.

### Patch CR2: Cross-Region Power Export Allocation

Goal: power a locally-short consumer from a producer in its own road component via
an authoritative producer-owned export request and grant with transient
allocation.

Likely files:

- `src/core/regions/runtime/mod.rs`
- `src/core/regions/worker.rs`
- `src/core/regional_types.rs`
- `src/core/components.rs` (extend `PowerSource` with `Imported`)
- `tests/region_runtime_test.rs`, `tests/region_worker_test.rs`

Implementation:

- After local power grants, a still-short consumer reachable from a border link
  uses the component graph plus the availability hint (CR1) to pick candidate
  producers in its component, in stable order.
- It sends an authoritative request over the existing cross-region event flow. The
  producer's event loop grants only if local remaining capacity minus active
  export allocations can satisfy the demand, records a transient export
  allocation, and replies. The consumer sets `powered = true` and
  `source = Imported { region }`. A stale "has spare" that is actually empty
  makes the consumer try the next candidate.
- Resolve all cross-region power before downstream systems read `powered`.

Tests:

- a short consumer is powered from a same-component producer.
- a consumer in a different component is not powered by an unreachable producer
  (the trap).
- two consumers competing for a producer's last unit resolve deterministically
  with no double-spend (producer export allocation).
- export allocation resolves before downstream systems read `powered`.

Review focus:

- Sharing follows roads (same component only).
- Producer-owned export allocation prevents double-spend; resolution order is
  deterministic.

### Patch CR3: Cross-Region Jobs Export

Goal: assign a jobless citizen to a spare workplace slot in its road component
through a producer-owned export grant, mirroring CR2's power model. The workplace
(exporting) region owns the slot and the resulting tax and business profit, so it
is the authoritative decider — a consumer imports a job, a producer exports a slot.

This reuses CR1 discovery (same component graph and the `has_spare_jobs` hint) and
the CR2 export-allocation lifecycle (`caller_generation` reservations plus the
unconditional release broadcast). The type vocabulary mirrors CR2 one-to-one:

- `JobExportRequest` / `JobExportAllocationRequest` (carry candidates + index)
- `JobExportGrant`
- `JobExportAllocation` (+ `JobExportAllocationKey`)
- `JobExportAllocationRelease`

Likely files:

- `src/core/systems/economy.rs`
- `src/core/regions/runtime/mod.rs`
- `src/core/regions/mod.rs`, `src/core/regions/worker.rs`
- `tests/economy_test.rs`, `tests/region_runtime_test.rs`,
  `tests/region_worker_test.rs`

Implementation:

- After local assignment, a citizen with no reachable local slot looks up spare
  job slots in its component and requests one. The producer reserves the slot; the
  consuming region records an owned remote-workplace reference (region plus slot
  id), not a remote ECS entity.
- The exporting region owns the workplace tax and business profit from that remote
  worker.
- Resolve before economy reads salaries, rent, and taxes.

Do not blind-copy CR2 — four things differ from power and must be handled
deliberately:

1. **The grant carries identity.** A `JobExportGrant` returns `{ source_region,
   slot_id }`, not just a region, so the consumer can record the remote-workplace
   reference. Keep the invariant: that reference is owned data (region + slot id),
   never a remote ECS entity.
2. **Economic ownership flows to the producer, the opposite of power's stat
   quirk.** Power counts imported supply in the *consumer* region; here the
   *exporting* region accrues the tax and business profit, while the citizen's home
   region gets the salary and rent effects. Resolve before economy reads salaries,
   rent, and taxes.
3. **A tick can be short on both power and jobs.** Extend the `TickState` machine
   with a sequential job phase rather than a combined wait: resolve power first
   (it sets `powered`, which jobs and economy then read), then resolve jobs, each
   as its own waiting sub-state. Suggested shape: `Idle ->
   WaitingForPowerExports -> WaitingForJobExports -> Idle`, skipping either wait
   when that resource has no exportable demand.
4. **No partial grants.** One citizen fills one whole slot, like one building draws
   its whole demand; the all-or-nothing grant from CR2 carries over unchanged.

Tests:

- a jobless citizen takes a same-component spare slot.
- an unreachable slot is not taken (the component trap, as in CR2).
- two jobless citizens competing for a producer's last slot resolve
  deterministically with no double-spend (reservation).
- exported job tax and profit accrue to the exporting region; no remote ECS entity
  is stored by the consuming region.
- a tick short on both power and jobs resolves both phases before downstream
  systems read `powered`, salaries, rent, and taxes.

Review focus:

- Sharing follows roads (same component only); the producer is the authoritative
  decider that owns the slot and its tax/profit.
- Reservation prevents double-spend; the power and job wait phases compose
  deterministically.

### Patch CR4: Imported-Resource Visibility

Goal: surface imported resources, and their blockers, through view models.

Likely files:

- `src/interface/adapter.rs`, `src/interface/view.rs`
- `tests/inspect_view_test.rs`

Implementation:

- Inspect notes for "powered by region N" and "works in region N", plus blockers
  when no same-component spare was available.
- Tick summary counts for imported power and imported jobs.

Tests:

- inspect exposes imported power and job notes.
- blockers appear when no same-component spare is available.

### Patch CR5: Save And Load Rebuild

Goal: keep imports rebuildable; never persist them as authoritative truth.

Likely files:

- `src/core/regions/mod.rs`, `src/core/regional_game.rs`
- `tests/regional_save_load_test.rs`

Implementation:

- On load, re-resolve local registries, rebuild the component graph and hints, and
  re-exchange import requests so imports regenerate from authoritative state.
- Do not serialize imported grants, remote sources, the component graph, or hints
  as truth.

Tests:

- a multi-region game with imports round-trips and rebuilds identical imports.
- imported state, the component graph, and hints are absent from save files.

### Patch CR6: Retire The Visibility-Only Push Cache

Goal: remove the earlier push-propagation cache now that every `ResourceKind`
resolves through the registry + discovery model, leaving one cross-region
mechanism. Do this only after CR1-CR4 provide the replacement, so the two paths
never run at once.

Likely files:

- `src/core/regions/mod.rs` (remove `imported_resources`,
  `neighbor_import_results`, and the `ImportedResourceCache` accept/forward path)
- `src/core/regions/runtime/mod.rs` (remove `RegionalExport` /
  `RegionalExportChange` tracking and `RegionExportsChanged`)
- `src/core/regions/worker.rs` (remove export-change routing)
- `src/core/regional_game.rs` (remove export-send paths; drop the generic import
  inspect note in favor of CR4 source-based notes)
- `tests/regional_command_test.rs`, `tests/regional_multi_region_play_test.rs`,
  `tests/region_runtime_test.rs`, `tests/region_worker_test.rs`,
  `tests/region_continuation_test.rs`, `tests/regional_save_load_test.rs`

Implementation:

- Move the building-derived `ResourceKind`s (service, shopping, jobs, park, road
  access) onto the registry as additional resource entries, discovered and
  allocated/requested exactly like power and jobs. No resource kind keeps a
  separate push path.
- Remove `RegionState.imported_resources` and `neighbor_import_results`, the
  `ImportedResourceCache` accept/forward machinery, and the runtime/worker
  export-change propagation.
- Replace the old "Imported regional resources: N" inspect note with the
  source-based notes from CR4.
- Save/load: nothing imported is stored, so there is no import cache to rebuild;
  imports regenerate from the registry + discovery path.

Tests:

- the old push-cache types and fields are gone; no region stores another region's
  exported resources.
- a building-derived resource (for example service access) is shared cross-region
  through the registry + discovery path, not a push cache.
- inspect shows source-based import notes; the old generic count is gone.
- multi-region save/load round-trips with imports rebuilt from authoritative state.

Review focus:

- Exactly one cross-region mechanism (registry + discovery + allocation request).
- No region stores another region's exported resources.
- Building-derived resources are not lost; they moved to the registry.

## Patch R5 (Deferred): Persistent, Change-Driven Registry

Goal: stop rebuilding the registry from `World` every tick. Power and job-slot
results are pure functions of building/provider/consumer topology, which only
changes on a few events; recompute on those events instead of every hourly tick.

Do this only after R1-R4 are stable, and only if it is worth the added
invalidation surface. The deciding factors are a profile showing per-tick
resolution is hot on large/multi-region games, or the cross-region event flow
making a maintained registry the natural shape anyway.

What is and is not per-tick today:

- `powered` flags and job slots are invariant between topology changes; a plain
  hourly tick does not change their inputs.
- Job assignment also depends on citizens and already runs only on the daily
  economy boundary, not every tick.

Invalidation triggers (the registry must recompute when any of these fire):

- build, bulldoze, replace, upgrade commands (already call
  `refresh_derived_state_for_world`).
- business auto-upgrades, which change a building level after `power::run` in the
  same tick and must mark the registry dirty for the next read.
- population growth, for job assignment only (new or removed citizens).
- save load, which rebuilds the registry from authoritative state.

Design:

- Keep the registry as derived state owned by the region. Maintain it across
  ticks; mark it dirty on the triggers above; recompute lazily on the next read.
- Recompute remains the same deterministic resolution used in R1/R2; only its
  frequency changes. No new behavior.
- Keep `World` private; the registry still exposes only owned summary data.

Hard requirement: behavior must stay identical to full per-tick recompute. A
missed invalidation is a silent determinism bug, so this patch is gated on a
**parity-guard test**:

- run a scripted sequence of commands and ticks (including business upgrades,
  population growth, save/load) twice: once with the change-driven registry and
  once forcing full recompute every tick.
- assert identical `powered` flags, job assignments, and `world.stats` at every
  step. Any divergence fails loudly and points at a missing invalidation
  trigger.

Tests:

- the parity-guard scripted comparison above
- each invalidation trigger marks the registry dirty and produces the same result
  as a forced recompute
- no invalidation on a plain hourly tick reuses the cached result

Review focus:

- The invalidation trigger set is complete; nothing that changes power or job
  topology is missed.
- The change-driven path is byte-identical to full recompute (parity guard).
- Determinism and `World` privacy are preserved.

## Guardrails

- Each patch is one mission, stays within roughly five files and 400 changed
  lines, and includes tests. No new external dependencies.
- Foundation patches (R1, R2) must be behavior-preserving; parity is proven by
  existing tests passing unchanged.
- `World` stays private to core. Any data crossing a boundary is owned summary
  data without ECS identity.
- Cross-region components are computed at `(region, road-network)` granularity.
- The component graph and availability hint hold owned summaries only; the hint is
  stale-tolerant; export allocation is authoritative over the cross-region event
  flow with producer-owned transient allocation; no region reads another region's
  `World`.
- Connectivity is binary; there is no per-border transit-capacity flow.
- Capacity comes from the registry spare query, never a parallel re-derivation.
- Determinism: stable road-network discovery, map-order allocation, stable
  component/producer/request order, integer math.

## Review Checklist

Foundation (R1-R3):

- Does the change keep `powered`/stats/economy outputs identical?
- Is per-network power isolation preserved?
- Is allocation deterministic and map-ordered?
- Is registry output owned and free of ECS identity?

Cross-region (R4):

- Are components keyed by `(region, road-network)`, not region?
- Does sharing follow roads (same component only)?
- Does producer-owned export allocation prevent double-spend?
- Do power and jobs resolve before the downstream systems that read them?
- Is all cross-boundary data owned and free of remote ECS identity?
- Is imported state rebuildable, not saved as truth?

Always:

- Did `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test` pass?
