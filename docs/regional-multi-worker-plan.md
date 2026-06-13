# Regional Multi-Worker Plan

This plan stages the move from **one** region worker thread to **several**, so a
large city (many regions) can simulate in parallel. It builds on the cross-region
mechanism already shipped in
[regional-resource-registry-plan.md](regional-resource-registry-plan.md)
(registry -> discovery -> producer-owned export allocation), which was deliberately
designed to be *correct under* multi-worker but does not yet spin up a second
worker. That earlier plan lists "true parallelism" as a Non-Goal; this plan is where
that work lives.

For the vocabulary (`World`, `RegionState`, `RegionRuntime`, `RegionWorker`,
discovery, export allocation, hints, generations) see
[regional-terminology.md](regional-terminology.md).

## Why this is separate work

The cross-region patches (CR1-CR6) all run on a single `RegionWorker`:

- discovery is computed by walking the regions one worker owns
  (`cross_region_discovery` over `self.regions`),
- authoritative routing only reaches regions that worker owns (a target it does not
  own becomes `MissingTargetRegion`, gracefully denied),
- the whole tick/event schedule is one deterministic FIFO drain on one thread.

None of that is wrong -- it is the correct single-worker collapse of a design meant
to scale. Multi-worker is a distinct mission with its own central risk
(**cross-thread determinism**), so it gets its own plan rather than bloating the
foundation.

## Current state (what already supports this)

- **Ownership model is ready.** `World` is `Send` (movable to a thread) but not
  `Sync` (never shared); each region's `World`/cache is owned by exactly one thread.
- **Only owned summaries cross boundaries.** Hints and export request/grant/release
  are owned data with no ECS identity, already routed as `RegionEvent`s /
  `OutboundMessage`s.
- **The directory product exists.** `CrossRegionDiscovery { components,
  availability_hints }` is already the thing a coordinator would own; today it is
  recomputed per pass on the single worker.
- **Scaffolding exists, unused.** `load_manager.rs` (`WorkerLoad`, `RegionMove`) is
  the seed of region->worker assignment and reassignment.

## Target shape

```text
              RegionalGameRunner
                   | owns
        +----------v-----------+        +------------------------+
        | RegionDirectory       |<------| region -> worker map    |
        |  (coordinator)        |       | (who owns whom)         |
        |  component graph,     |       +-----------+------------+
        |  per-network hints    |                   | route by owner
        |  Arc + lock/atomic    |                   |
        +---^-------------^-----+                   |
   publish  | hint   read | candidates              |
        +---+----+   +----+---+   cross-worker event channel
        |Worker 1|   |Worker 2| <----------------------+
        | Reg A,B|   | Reg C,D|
        +--------+   +--------+
   each worker = one OS thread, owns a disjoint set of regions (Worlds pinned)
```

The coordinator owns the **directory** (small owned summaries), never the regions'
`World`s. Regions publish hints *up*; discovery reads candidates *down*; the
authoritative claim still rides the event flow and is re-validated at the producer's
`ExportAllocations` ledger. Determinism lives in the event flow, not the directory.

## Decision Record

- The coordinator owns discovery (component graph + hints) only. Worlds stay sharded
  on worker threads. "Owns all regions" means owns their published summaries and the
  routing map, not their ECS.
- The hint stays tiny and stale-tolerant; cross-thread reads need no barrier. A wrong
  guess costs one declined request, never a wrong allocation.
- Cross-worker routing forwards an authoritative event to the owning worker; it does
  not read or copy the target's `World`.
- Determinism is a hard requirement, not an afterthought: the multi-worker schedule
  must produce identical results to the single-worker schedule for the same inputs,
  or the difference must be a documented, intentional contract. This gates M3+.
- No new external dependencies (no async runtime, no rayon); use `std::thread` and
  channels as the existing `ThreadedRegionWorker` already does.

## The determinism problem (the gating risk)

Single-worker today is deterministic because every region ticks and every event is
processed in one FIFO order on one thread. With several threads, two questions arise:

1. **Within one logical step (e.g. one `tick_all`), do regions on different threads
   observe each other's exports identically regardless of thread timing?** The export
   allocation is already authoritative and producer-serialized, and the hint is
   stale-tolerant, so a single producer's grants are deterministic. The risk is the
   *interleaving* of cross-worker requests reaching a producer in a
   timing-dependent order.
2. **Is the cross-worker delivery order itself deterministic?** Channels deliver in
   send order per sender, but merge order across senders can vary by thread timing.

The plan's answer is a **deterministic barrier per logical step**: workers run their
local pass, then a coordinator-driven merge point collects and orders cross-worker
events by a stable key (e.g. `(target_region, caller_region, request_id, token)`)
before delivery, so the producer sees a thread-timing-independent order. This trades
some parallel overlap for reproducibility. M3 must land with a parity guard
(multi-worker result == single-worker result over a scripted run) exactly like R5's.

## Staged patches

Each patch is independently shippable, behavior-preserving where marked, and gated on
tests. Keep diffs within the repo's ~5 files / ~400 line guideline; split if larger.

### Patch M1: Extract the coordinator directory (single worker, behavior-preserving)

Goal: move discovery out of `RegionWorker` into a `RegionDirectory` owned by
`RegionalGameRunner`, shared into the worker. Still one worker; the worker reads the
directory instead of computing discovery inline.

- Add `RegionDirectory` owning `topology` + the built `CrossRegionDiscovery`.
- `RegionWorker` borrows the directory for routing/candidate selection instead of
  calling `cross_region_discovery` on itself.
- No behavior change: the existing CR2/CR3 suites must pass unchanged (the proof).

Tests: existing cross-region tests green; a unit test that the directory yields the
same components/candidates the worker computed before.

### Patch M2: Publish hints into the directory on change

Goal: stop recomputing hints by walking owned regions every pass; have each region
publish its `RegionalAvailabilityHint`s into the directory when they change.

- Publish on registry recompute (tie into the R5 invalidation chokepoints) or once
  per scheduling pass; store behind a double-buffer/atomic for stale-tolerant reads.
- Still one worker, so still trivially consistent; this is the cross-thread-ready
  storage shape, proven first in the easy case.

Tests: a stale hint still yields a correct (re-validated) grant or a clean decline;
publishing is idempotent; reads never block writers.

### Patch M3: Cross-worker event routing + the determinism barrier

Goal: route an authoritative event to the worker that owns the target region, with a
deterministic merge point. **This is the gating patch.**

- Add the region->worker map (owned by the runner/coordinator).
- Replace `MissingTargetRegion -> deny` with forward-to-owning-worker over a
  cross-worker channel.
- Insert the per-step barrier that orders cross-worker events by a stable key before
  delivery.
- Still may run two workers only in tests here; production can stay one worker until
  M4.

Tests (the hard gate): a **parity guard** -- run a scripted multi-region sequence
(builds, ticks, cross-region power and jobs, save/load) on 1 worker and on 2 workers
and assert identical `powered`, job assignments, and `world.stats` at every step.
Any divergence is a determinism bug.

### Patch M4: Spawn N workers and assign regions

Goal: the runner starts several `ThreadedRegionWorker`s and shards regions across
them at startup/load.

- Use `WorkerLoad` to pick an assignment (e.g. balanced region count, or border-aware
  grouping so same-component regions tend to share a worker and cut cross-worker
  traffic).
- Assignment is fixed for the session in this patch (no live moves yet).

Tests: an N-region game on K workers round-trips and matches the 1-worker result
(reuse the M3 parity guard with K>1); assignment is deterministic.

### Patch M5: Configurable worker setup from a file

Goal: let an operator choose the number of worker threads and how regions are
distributed across them (including *uneven* per-worker counts) from a setup file,
instead of only the programmatic assignment M4 picks.

Crucial invariant: **the setup is a performance knob, never a gameplay input.** The
M3 determinism barrier orders cross-worker events by a stable key, not by thread or
worker, so the simulation result is identical for *any* worker count and *any* valid
assignment. The setup file therefore changes only how work is parallelized, not what
happens in the city. It is also **separate from the game save**: a save carries the
regions and their state; the worker setup is a per-machine deployment choice, so the
same save runs identically whether loaded on a 2-thread laptop or a 16-thread server.

- Reuse `serde_json` (already a dependency for saves) -- no new external crate. The
  file gives `worker_count` and an assignment: either an explicit
  `region -> worker_index` map or per-worker region lists, so workers may own
  different numbers of regions.
- Validate before spawning (mirroring the save-layout validation): `worker_count >=
  1`, every region assigned to exactly one in-range worker, no region omitted or
  doubled. A malformed setup is rejected with a clear error, never silently
  half-applied.
- When the file is absent, fall back to M4's default auto-assignment, so the setup
  file is optional.

Tests:

- the same scripted run under several setups -- 1 worker; K workers balanced; K
  workers *uneven*; a different `region -> worker` map -- all produce identical
  `powered`, job assignments, and `world.stats` (parameterize the M3 parity guard
  over the setup, proving results are assignment-independent).
- invalid setups (worker_count 0, an unassigned region, an out-of-range worker,
  a region assigned twice) are rejected before any worker spawns.
- an absent setup file uses the default assignment and still matches.

### Patch M6 (optional, later): Live region reassignment

Goal: move a region (its `World`) from one worker to another at a safe point, for
load balancing.

- Use `RegionMove`; move at a step boundary so no in-flight export allocation is
  stranded (drain or transfer the region's paused-tick state and its inbound events).
- Re-point the region->worker map and the directory.

Tests: a move at a safe point preserves all state and keeps the parity guard green;
a move never strands a pending export (it is denied or carried, never lost).

## Non-Goals

- No async runtime, no work-stealing scheduler, no lock-free exotica; `std::thread` +
  channels only.
- No cross-region transit-capacity model (still binary connectivity, as before).
- No change to the resolution math, balance, or the single-worker observable behavior.
- M6 (live moves) is explicitly optional and may never be needed.

## Review focus

- The coordinator owns summaries and the routing map, never a region's `World`.
- Determinism: the multi-worker result equals the single-worker result, proven by the
  M3 parity guard; the merge-point ordering key is stable and documented.
- Stale hints only misdirect a request; they never produce a wrong allocation.
- No region reads another region's ECS across a thread boundary.
