# Regional Resource Sharing — Terminology

A glossary for the cross-region resource model (registry, discovery, and
cross-region power/jobs sharing). Keep this in sync when types are renamed.

Companion to [regional-resource-registry-plan.md](regional-resource-registry-plan.md),
which holds the staged implementation plan (R1–R3, CR1–CR6, R5). This file
defines *words*; the plan defines *work*.

---

## 1. The naming convention: import vs export

The single biggest source of confusion is that one transfer of power has two
names depending on whose side you stand on. We name types from the **producer's**
side ("export") for the authoritative machinery, and keep "import" only for the
**consumer's** recorded result.

| Perspective | Word | Where it shows up |
| --- | --- | --- |
| Consumer needs power it can't generate | **demand** | `PendingPowerDemand`, `PowerExportRequest.demand` |
| Consumer's request to a neighbor | **request** | `PowerExportRequest` |
| Producer's authoritative decision | **export grant / allocation** | `PowerExportGrant`, `PowerExportAllocation` |
| What the consumer records after a grant | **imported** | `PowerSource::Imported { source_region }` |

Rule of thumb: **a consumer imports; a producer exports.** The allocation ledger
and every request/grant/release type lives on the producer's side and uses
"export". The only place "import" survives is the consumer's derived
`PowerSource::Imported`, because from the consumer's view the power *is* imported.

> Removed legacy exception: `ImportedResource` / `ImportedResourceCache` /
> `RegionalExport` were the old visibility-only push cache (see §6). They
> predated this convention and were removed in patch CR6.

---

## 2. Geography and topology

These describe *where* regions and roads are, and which roads may share power.

- **`RegionId`** — stable id for one region (one private `World`).
- **road network** — a connected component of road tiles inside one region.
  Power pools per road network: disconnected networks never share capacity.
- **`RegionRoadNetworkId { region, road_network }`** — names one road network in
  one region. This is the granularity sharing follows — *not* the region as a
  whole. "Sharing follows roads" means it follows `RegionRoadNetworkId`s.
- **`BorderEdge`** — `North | South | East | West`: which map edge a road touches.
- **`BorderLinkId { edge, offset }`** — a specific spot on a border edge where a
  road meets the region boundary. `offset` is the cell coordinate along that edge
  (x for N/S, y for W/E).
- **`NetworkBorderLink { network, link }`** — "this road network reaches the
  boundary at this border link." Produced by `RegionState::network_border_links()`.
- **`RegionNeighborLink { region, edge, neighbor }`** — declared adjacency: "this
  region's `edge` borders that `neighbor` region." This is the **topology** that
  gates which border links may match. Without a declared neighbor link, two
  border links never join — this is what stops non-adjacent regions that happen
  to share an offset from wrongly connecting.
- **topology** — the full set of `RegionNeighborLink`s for the layout.
- **`RegionalLayoutSave { rows, columns }`** — compact row-major grid shape stored
  in the save *instead of* explicit topology. `derive_topology` rebuilds the
  `RegionNeighborLink`s from grid adjacency at load/start time.
- **component** — a set of `RegionRoadNetworkId`s joined transitively through
  matched border links across declared neighbors (union-find). Two road networks
  can share power **iff they are in the same component**.
- **`CrossRegionDiscovery`** — the computed `{ components, availability_hints }`.
  Discovery data only: owned, deterministic, no ECS identity, no side effects.

---

## 3. The registry and availability

The registry is the local source of truth that discovery and sharing read from.

- **`ResourceRegistry`** — per-resource view built from a `World`. Constructed
  narrowly: `for_power` (builds power entries) or `for_jobs` (builds job entries),
  to avoid paying for both when only one is needed.
- **`PowerResolution`** — result of `resolve_local_power()`: who is powered, plus
  `network_capacities`.
- **`PowerNetworkCapacity { road_network, remaining_capacity }`** — a road
  network's spare power **after** local consumers are served. The basis for
  whether a region can export.
- **`RegionalAvailabilityHint { network, has_spare_power, has_spare_jobs }`** — a
  tiny, **stale-tolerant** summary published per network so other workers can
  cheaply guess where to ask. It is only a hint: a producer may have gone empty
  since it was published, so a grant can still be denied (the consumer then tries
  the next candidate). Authoritative truth lives in the request/grant flow, never
  in the hint.

---

## 4. Cross-region power export flow

The authoritative request → grant machinery. Lives on the producer's side except
the consumer-local demand list.

- **`PendingPowerDemand { token, consumer, demand, caller_network }`** — one
  still-unpowered consumer (after local power) that sits on a border network and
  could be served from a neighbor. Built by `RegionState::pending_power_demands()`.
- **`token`** — index of a demand within one tick, unique per caller tick. Lets a
  grant be matched back to the exact pending consumer.
- **`PowerExportRequest { request_id, caller_region, caller_network, token, demand }`**
  — the consumer asking: "can someone in my component export `demand` for me?"
- **`PowerExportAllocationRequest { request, candidates, candidate_index }`** — the
  request as routed to producers, carrying the ordered `candidates` (same-component
  networks with a spare hint) and which one is being tried now. On denial the
  worker advances `candidate_index` to the next candidate.
- **`PowerExportGrant { token, granted, source_region }`** — the producer's reply.
  On `granted`, the consumer sets `powered = true` and
  `source = PowerSource::Imported { source_region }`.
- **`PowerExportAllocation { key, network, demand, caller_generation }`** — a
  producer-owned, transient reservation of its own spare capacity against one
  granted demand. Prevents double-spending capacity within one scheduling round.
- **`PowerExportAllocationKey { caller_region, request_id, token }`** — uniquely
  identifies one reservation so re-claims update in place instead of duplicating.
- **`caller_generation`** — the consumer tick's `request_id` (a `UiRequestId`),
  used as a per-round **version tag**. An allocation tagged with an old generation
  is stale once the caller moves to a new one. Regions don't share a clock, so the
  caller stamps each round and the producer garbage-collects by comparing tags.
- **`PowerExportAllocationRelease { caller_region, request_id }`** — broadcast by a
  consumer at the **start of every tick** (even with zero demands) telling
  producers to drop that caller's allocations from older generations. Needed for
  the case the request path can't cover: a consumer that became self-sufficient
  and sends no request would otherwise leave its reservation pinned forever.

---

## 4b. Cross-region jobs export flow

Jobs share the **same producer-owned export model** as power: a jobless citizen is
the consumer (it *imports* a job), the workplace's region is the producer (it
*exports* a spare slot) and is the authoritative decider, because it owns the slot
and accrues the resulting tax and business profit. The type vocabulary mirrors §4
one-to-one — `JobExportRequest`, `JobExportAllocationRequest`, `JobExportGrant`,
`JobExportAllocation` (+ `JobExportAllocationKey`), `JobExportAllocationRelease` —
and reuses CR1 discovery (the same component graph and the `has_spare_jobs` hint)
and the `caller_generation` release lifecycle.

Four things differ from power and are **not** a blind rename:

1. **The grant carries identity.** `JobExportGrant` returns `{ source_region,
   slot_id }` so the consumer can record the remote-workplace reference. That
   reference is owned data (region + slot id), never a remote ECS entity.
2. **Economic ownership flows to the producer** — the opposite of power's stat
   quirk (§4). The exporting region accrues the tax and business profit; the
   citizen's home region gets salary and rent effects.
3. **A tick can be short on both power and jobs**, so the `TickState` machine has
   a sequential job phase: `WaitingForPowerExports -> WaitingForJobExports -> Idle`,
   power first because it sets `powered`, which jobs and economy then read.
4. **No partial grants** — one citizen fills one whole slot, like one building
   draws its whole demand.

---

## 5. Ownership, runtime, and scheduling

The ownership chain, bottom to top, is `World → RegionState → RegionRuntime →
RegionWorker` (one OS thread). A `World` is only ever touched by the single thread
that owns its region; it is moved between threads, never shared (its `RefCell`
registry cache is `Send`, not `Sync`).

- **`World`** — one self-contained city's **private ECS instance**: entities,
  component maps, grid, `CityResources`/`CityStats`, and the derived
  `ResourceRegistryCache`. It is the substrate every `systems/` function operates on
  (`fn run(world: &mut World)`) and the unit that is serialized on save. It is
  **region-agnostic**: it holds *one region's* data but knows nothing about regions,
  neighbors, or threads. There is one `World` per region; the name is the ECS
  convention for "one simulation instance," not "the whole game" (the multi-region
  container is `RegionalGame`). A single-city game is just a one-region `RegionalGame`.
- **`RegionState`** — the **region coordinator** that owns one `World` plus its
  `RegionId` (its only two fields). It adds the region-scoped operations on top of
  the bare ECS: cross-region export/discovery (`pending_power_demands`,
  `spare_job_slots_on_network`, `availability_hints`, …) and tick-phase
  orchestration (`begin/continue/finish` phases). It is the behavioral seam between
  "the simulation" (`World`) and "the region that runs one and talks to others."
- **`RegionRuntime`** — actor shell owning one `RegionState`, its inbox, and its
  producer-side `power_export_allocations` ledger.
- **`RegionWorker`** — single-threaded scheduler owning several runtimes; routes
  `OutboundMessage`s between them using the `topology`.
- **scheduling pass** — one `process_region_events` call: give each runtime a slice
  of work, then route everything they emitted. Reservation **releases are routed
  before requests** in a pass, so a producer frees a caller's stale generation
  before evaluating anyone's fresh request.
- **`TickState`** — explicit tick lifecycle on each runtime: `Idle`,
  `WaitingForPowerExports(TickPowerContinuation)`, or
  `WaitingForJobExports(TickJobContinuation)`.
- **import-wait tick phase** — a time-advancing tick continuation that has run a
  local derived phase, found cross-region demand, and is waiting for producer
  grants before running downstream systems (population, economy, ...). This is
  not a paused UI view refresh: paused commands/views refresh local derived state
  from local truth plus last applied imports and do not wait on remote producers.
  While in an import-wait phase, the runtime still dequeues import control events
  (`ApplyPowerExportGrant`, `ApplyJobExportGrant`,
  `ProcessPowerExportRequest`, `ProcessJobExportRequest`,
  `ReleasePowerExportAllocations`, `ReleaseJobExportAllocations`) so it can both
  finish its own tick and serve neighbors. A second `Tick` is deferred in the
  inbox until the waiting tick continuation finishes.
- **`RegionEvent`** (inbox) — `Tick`, `ProcessPowerExportRequest`,
  `ReleasePowerExportAllocations`, `ApplyPowerExportGrant`,
  `ProcessJobExportRequest`, `ReleaseJobExportAllocations`,
  `ApplyJobExportGrant`, plus snapshot/command events.
- **`OutboundMessage`** (routing) — `PowerExportRequested`,
  `PowerExportRequestCompleted`, `PowerExportAllocationsReleased`,
  `RegionTickCompleted`, etc. Named in the passive/perfect tense (the runtime
  *reports* what happened; the worker decides where it goes next).
- **tick phase split** — `begin_tick_power_phase` runs time-advance + local power;
  `finish_tick_after_power_phase` runs everything downstream. The gap between them
  is where imported power is applied, so downstream systems read the final
  `powered` state.

---

## 6. Removed legacy push cache (removed in CR6)

Pre-convention push-cache types that no longer exist in code. Do not reintroduce
features on this model.

- **`ImportedResource`** — a copy of a neighbor's resource pushed across the border
  with hop limits and generation-based staleness rejection.
- **`ImportedResourceCache`** — per-region store of received `ImportedResource`s.
- **`RegionalExport` / `RegionalExportChange`** — a region's published exportable
  resources and change notifications.

These provided *visibility* only (never authoritative grants). Patch CR6 removed
them after power and jobs had a real registry + discovery + export-grant path.
Service, shopping, park, road access, and other building-derived resources remain
deferred until they have concrete gameplay consumers and allocation rules.
