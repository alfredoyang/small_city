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

> Legacy exception: `ImportedResource` / `ImportedResourceCache` / `RegionalExport`
> are the **old** visibility-only push cache (see §6). They predate this
> convention and are scheduled for removal in patch CR6.

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

## 5. Runtime and scheduling

How the above moves between regions on the actor-style worker.

- **`RegionRuntime`** — actor shell owning one `RegionState`, its inbox, and its
  producer-side `power_export_allocations` ledger.
- **`RegionWorker`** — single-threaded scheduler owning several runtimes; routes
  `OutboundMessage`s between them using the `topology`.
- **scheduling pass** — one `process_region_events` call: give each runtime a slice
  of work, then route everything they emitted. Reservation **releases are routed
  before requests** in a pass, so a producer frees a caller's stale generation
  before evaluating anyone's fresh request.
- **paused tick** (`pending_tick`) — a tick that ran local power, found unpowered
  border consumers, and is waiting for export grants before running downstream
  systems (population, economy, …). While paused, the runtime only dequeues
  export control events (`ApplyPowerExportGrant`, `ProcessPowerExportRequest`,
  `ReleasePowerExportAllocations`) so it can both finish its own tick and serve
  neighbors — the latter prevents two mutually-importing regions from deadlocking.
- **`RegionEvent`** (inbox) — `Tick`, `ProcessPowerExportRequest`,
  `ReleasePowerExportAllocations`, `ApplyPowerExportGrant`, plus snapshot/command
  and the legacy `ProcessImportedResource` events.
- **`OutboundMessage`** (routing) — `PowerExportRequested`,
  `PowerExportRequestCompleted`, `PowerExportAllocationsReleased`,
  `RegionTickCompleted`, etc. Named in the passive/perfect tense (the runtime
  *reports* what happened; the worker decides where it goes next).
- **tick phase split** — `begin_tick_power_phase` runs time-advance + local power;
  `finish_tick_after_power_phase` runs everything downstream. The gap between them
  is where imported power is applied, so downstream systems read the final
  `powered` state.

---

## 6. Legacy / transitional (retiring in CR6)

Pre-convention push-cache types. Do not build new features on these.

- **`ImportedResource`** — a copy of a neighbor's resource pushed across the border
  with hop limits and generation-based staleness rejection.
- **`ImportedResourceCache`** — per-region store of received `ImportedResource`s.
- **`RegionalExport` / `RegionalExportChange`** — a region's published exportable
  resources and change notifications.

These provided *visibility* only (never authoritative grants). Patch CR6 removes
them once all resource kinds use the registry + discovery + export-grant model.
