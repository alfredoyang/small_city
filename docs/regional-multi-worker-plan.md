# Regional Multi-Worker Plan

This is the active plan for running **N regions on M worker threads**. It composes with the
producer-authoritative import model in
[regional-cross-region-import-plan.md](regional-cross-region-import-plan.md).

The previous "snapshot allocation" direction is rejected. Multi-worker execution still
uses directories and snapshots, but only as stale-tolerant routing hints. Cross-region
power/jobs remain producer-authoritative request/grant flows.

For vocabulary (`World`, `RegionState`, `RegionRuntime`, `RegionWorker`, directory,
snapshot, hints, export grants) see [regional-terminology.md](regional-terminology.md).

## Current State

The codebase already has most of the single-worker foundation:

- `World` is owned by one `RegionState` and is not exposed to UI.
- `RegionRuntime` owns one region's state and inbox.
- `RegionWorker` schedules multiple runtimes on one thread.
- `ThreadedRegionWorker` wraps a worker in an OS thread.
- `RegionalGameRunner` exposes the UI-safe runner API.
- `RegionDirectory` / owner directory style data exists as the right place for topology,
  hints, and region ownership.

The missing production step is that `RegionalGameRunner` still effectively behaves as a
single-worker runner for normal play. This plan makes worker count a deployment choice
without changing UI access rules.

## Target Shape

```text
                         RegionalGame
                              |
                              v
                      RegionalGameRunner
                              |
          +-------------------+-------------------+
          |                                       |
          v                                       v
  RegionOwnerDirectory                    RegionDirectory
  region -> worker                        topology + hints
          |
          | route command/tick/snapshot/import events
          v
  +-------------------+       +-------------------+       +-------------------+
  | ThreadedWorker 0  |       | ThreadedWorker 1  |       | ThreadedWorker M  |
  | RegionRuntime A   |       | RegionRuntime C   |       | RegionRuntime ... |
  | RegionRuntime B   |       | RegionRuntime D   |       |                   |
  +-------------------+       +-------------------+       +-------------------+
          |                           ^
          | import request/grant      |
          +---------------------------+
```

Each worker owns a disjoint set of regions. A region's `World` is touched only by its
owning worker thread. Cross-worker communication is owned data over channels.

## Routing Rules

- UI commands route to the worker that owns the selected region.
- Ticks route to each owned region, or to all workers for tick-all behavior.
- Snapshot/view requests route to the selected region's owner and return UI-safe view
  models only.
- Cross-region import requests route to the producer region's owning worker.
- Cross-region import replies route back to the consumer region's owning worker.
- Directory hints can be stale. The producer runtime is the authority that grants or
  denies.

## Coordination Rules

No global snapshot allocator is introduced. No worker reads another worker's ECS.

The minimum coordination required for correctness:

- A region cannot begin tick `N + 1` while its tick `N` continuation is waiting for
  power/job import replies.
- A worker must keep pumping control events while one of its regions waits, so it can serve
  producer-side requests for other regions and receive replies for the waiting region.
- Cross-worker message ordering must be deterministic for a given input and worker setup.
  If multiple senders target the same producer in one scheduling pass, merge them by a
  stable key such as `(target_region, caller_region, request_id, token)` before the producer
  mutates its ledger.
- Save/load/shutdown must handle pending continuations and producer reservations
  deliberately. They cannot silently drop in-flight import state.

This keeps local determinism strict while allowing cross-region information to arrive
later through the event flow.

## Staged Patches

Each patch should be small and independently reviewable. Split a patch if it touches more
than about five production files or grows past roughly 400 changed lines excluding tests.

### Patch MW1: Refresh Contracts And Test Scaffolding

Goal: make the active docs and tests describe M workers with producer-authoritative
imports.

- Remove stale "multi-worker is superseded" wording.
- Keep the UI boundary rule: UI talks through `RegionalGame`, never worker/runtime/ECS.
- Identify existing single-worker parity scripts that should become worker-count
  parameterized.

Tests: no production behavior change; docs-only if this patch only updates plans.

### Patch MW2: Runner Owns Multiple Threaded Workers

Goal: change `RegionalGameRunner` from one threaded worker to a collection of workers,
while still defaulting to one worker.

Status: implemented. The runner can start multiple threaded workers and assigns regions
round-robin through `RegionOwnerDirectory`. UI behavior still defaults to one worker.
Cross-worker import event delivery is implemented in MW4; MW2 only establishes ownership.

- Add a worker-count setup path with default `1`.
- Start `Vec<ThreadedRegionWorker>`.
- Build a `RegionOwnerDirectory` mapping every region to exactly one worker.
- Keep current UI behavior identical under the default setup.

Tests:

- default one-worker game behaves as before,
- invalid worker counts are rejected,
- every region is assigned exactly once.

### Patch MW3: Route UI Operations By Region Owner

Goal: make commands, ticks, snapshots, save/load operations use the owner directory.

Status: implemented for UI command/tick/snapshot/inspect/recovery routing. Tick-all now
batches requests before collecting correlated replies. Cross-worker import delivery is
handled by the MW4 deterministic barrier.

- Selected-region command goes to the selected region's worker.
- Tick-all fans out to all workers and collects correlated replies.
- Snapshot/view reads go through UI-safe view models from the owning worker.
- No UI module imports worker/runtime/ECS types.

Tests:

- two regions on two workers can each receive commands,
- tick-all returns one result per region or a clearly aggregated result,
- UI contract tests still forbid ECS/runtime imports.

### Patch MW4: Cross-Worker Import Routing

Goal: make power/job import request/grant/release events work when producer and consumer
are on different workers.

Status: implemented. Threaded runner passes use the deterministic barrier: worker
outbound events are collected, sorted by the stable forwarded-event key, then delivered
to the owning worker inbox before the next processing pass.

- Request routes consumer -> producer owner.
- Grant/deny routes producer -> consumer owner.
- Release routes consumer -> known producer owner.
- If producer tracking is missing for a legacy path, broadcast is allowed only as a
  temporary conservative fallback with a TODO.
- Merge same-producer inbound requests deterministically before producer ledger mutation.

Tests:

- region A on worker 0 consumes power from region B on worker 1,
- residents in region A can work in jobs in region B,
- two consumers racing for one producer capacity resolve in deterministic stable order,
- a denied or stale-hint request does not mutate consumer derived state.

### Patch MW5: Configurable Worker Setup

Goal: make worker count and region assignment configurable without making it gameplay
state.

Status: implemented. Existing worker-count entry points keep automatic round-robin
assignment. Explicit setup is available as a region-input-order vector where each
entry names the target worker index; invalid setup is rejected before worker threads
start.

- Reuse existing serialization dependencies; do not add a new production dependency.
- Support default automatic assignment when no setup is provided.
- Validate explicit setup before spawning workers:
  - worker count >= 1,
  - every region assigned exactly once,
  - all worker indexes in range.
- Keep setup outside the save file unless there is a separate reviewed reason to persist
  it.

Tests:

- same save runs under one worker, balanced M workers, and uneven M workers,
- invalid setups fail before any worker starts,
- simulation-visible results are identical for deterministic test scripts.

### Patch MW6: Save, Load, And Shutdown Across M Workers

Goal: make lifecycle operations safe when regions are distributed.

- Save collects all region snapshots through owner workers.
- Load builds region ownership before starting normal scheduling.
- Shutdown drains or rejects pending continuations predictably.
- Producer reservations are released or serialized deliberately.

Tests:

- save/load round-trip with regions spread across workers,
- shutdown during an import wait does not hang,
- no producer reservation remains pinned after load/shutdown cleanup.

### Patch MW7: Optional Live Reassignment

Goal: move a region from one worker to another at a safe boundary.

This is optional and later. It is not required for "M workers run N regions."

- Move only when the region is not in the middle of a tick continuation, or serialize and
  transfer that continuation explicitly.
- Update region owner directory and directory hints at the same boundary.
- Release or transfer producer reservations deliberately.

Tests:

- moving a region preserves its state,
- pending import state is not lost,
- repeated balancing does not oscillate one region between workers.

## Non-Goals

- No snapshot-authoritative allocation.
- No direct ECS sharing across workers.
- No async runtime, rayon, or new production dependency.
- No promise that cross-region visible timing exactly matches old single-worker eager
  behavior.
- No transit-capacity model.

## Review Focus

- The UI boundary remains intact.
- Every `World` has exactly one owning worker.
- Cross-worker routing uses owned messages only.
- Producer regions remain authoritative for power/job grants.
- Request ordering at a producer is deterministic for a given setup.
- A waiting tick is an explicit continuation state, not an implicit blocked thread.
