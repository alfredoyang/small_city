# Regional Producer-Authoritative Cross-Region Plan

This replaces the rejected "snapshot-consistent allocation" direction. That idea was:
publish quantitative summaries, allocate imports from a frozen global snapshot, and remove
request/grant events. It made the snapshot the authority for cross-region resources, which
is backwards for the current architecture.

The active direction is **producer-authoritative imports**:

- snapshots and directories are stale-tolerant hints,
- producer regions own their live export capacity,
- consumer regions request imports from producers,
- the producer grants or denies from its own runtime state,
- the consumer applies imported derived state only after a grant.

For vocabulary (`World`, `RegionState`, `RegionRuntime`, `RegionWorker`, directory,
snapshot, hints, export grants) see [regional-terminology.md](architecture/regional-terminology.md).
For M workers running N regions, see
[regional-multi-worker-plan.md](regional-multi-worker-plan.md).

## Decision Record

The snapshot-allocation model is rejected.

It solved ordering by making every worker compute from the same frozen input, but it also
moved allocation authority out of the producer. That conflicts with the producer-owned
resource model: only the producer knows whether its current power/job capacity remains
available after local changes and previous grants.

The accepted tradeoff is:

- **Inside one region:** deterministic derived/time ordering remains strict.
- **Across regions:** timing can lag by event delivery. A cross-region import may be denied
  or may arrive one or more scheduling passes later. That is acceptable as long as producer
  authority is preserved and the event order is deterministic for a given run configuration.
- **Paused reads:** local derived state can refresh without waiting for remote producers.
  Only a time-advancing tick continuation can wait for cross-region grants.

## Model

```text
Consumer region A                         Producer region B
-----------------                         -----------------
local derived phase
find import demand
read directory hint
        |
        | ConsumePowerRequested / ReserveJobRequested
        v
                                      check live producer state
                                      reserve from producer ledger
                                      grant or deny
        ^
        | ConsumePowerCompleted / ReserveJobCompleted
apply grant to imported state
continue local tick
```

The directory points a consumer toward candidate producers. It never grants resources.

```text
          RegionDirectory / snapshot
          --------------------------
          topology
          component membership
          candidate producer hints
          optional UI/debug summaries
                    |
                    | hint only
                    v
Consumer request -----------------> Producer runtime
                                      authoritative capacity check
```

## Resource Ownership Rules

- A producer owns its export capacity and reservation ledger.
- A consumer may record where an accepted import came from, but that record is for release,
  display, and debugging. It is not authority to keep consuming if the producer later
  denies or revokes.
- A stale hint can cause a wasted request. It must not cause a wrong powered/job state.
- Consumer-side derived state changes only after producer confirmation.
- Release/re-request every tick is correct and simple. Incremental reuse can come later
  only if producer-owned invalidation is explicit and tested.

## Tick State Machine

Cross-region imports are part of a time-advancing tick, not a paused view refresh. The tick
continuation should be explicit:

```text
Idle
  |
  v
LocalPowerDerived
  |
  +-- no remote demand ----------------------+
  |                                         |
  v                                         |
AwaitingPowerGrants                         |
  |                                         |
  v                                         |
ApplyPowerGrants                            |
  |                                         |
  v                                         |
LocalJobDerived                             |
  |                                         |
  +-- no remote demand ------------------+  |
  |                                      |  |
  v                                      |  |
AwaitingJobGrants                        |  |
  |                                      |  |
  v                                      |  |
ApplyJobGrants                           |  |
  |                                      |  |
  +--------------------------------------+--+
                                         |
                                         v
                              EconomyPopulationTimePass
                                         |
                                         v
                                      Completed
```

This is intentionally a state machine, not a "random pause." If a tick waits, the runtime
state must say exactly which import phase is waiting and what continuation remains.

While a region is waiting, the worker must still route control events so other regions can
make progress:

- incoming producer-side import requests,
- incoming grant/deny replies for the waiting continuation,
- release messages for producer ledgers,
- snapshot and shutdown/control messages that do not mutate the waiting tick incorrectly.

A second tick for the same region must not start until the current tick continuation
finishes.

## Paused Mode Semantics

Paused mode should not use remote waits.

```text
paused command / view read
        |
        v
local derived refresh only
        |
        v
view updates from local truth + last applied imports
```

Examples:

- Building or bulldozing a local road can refresh local connectivity immediately.
- A local power plant can power local consumers immediately.
- A cross-region house does not block the UI while asking a neighbor for power.
- The next time-advancing tick may request the remote import and apply the grant.

This keeps pause mode responsive and keeps cross-region waiting isolated to the tick
continuation.

## Coordinator Progress Rules

There is no global snapshot allocator, and no worker may read another worker's `World`.

The coordinator/runner owns:

- region to worker ownership,
- shared directory/snapshot hints,
- channels for cross-worker routing,
- lifecycle operations such as startup, save/load, and shutdown.

Progress rules:

- A region cannot start tick `N + 1` while tick `N` is waiting for import replies.
- A worker with one waiting region keeps processing other owned regions and routing control
  events.
- Cross-region messages route to the worker that owns the target region.
- The directory may be stale; producer grants are the authoritative correction.
- Save/load and shutdown must either drain, serialize, or reject pending continuations
  deliberately. They must not silently drop producer reservations.

## Staged Patches

Each patch should stay small enough to review under the repo guideline. Split a patch if it
touches more than about five production files or grows past roughly 400 changed lines
excluding tests.

### Patch PA1: Clean Up The Plan And Keep Producer Authority

Goal: remove the snapshot-authoritative direction from the forward plan and document the
producer-authoritative event model.

- No behavior change.
- Update terminology so "snapshot" means hint, not authority.
- Keep request/grant/release events as the active design.

Tests: none required for docs-only work.

### Patch PA2: Rename Import Events Around Consumer Intent

Goal: make event names reflect consumer intent and producer authority.

Suggested naming:

- `ConsumePowerRequested`
- `ConsumePowerCompleted`
- `ReserveJobRequested`
- `ReserveJobCompleted`
- `ReleasePowerImportAllocations`
- `ReleaseJobImportAllocations`

The exact names can differ, but they should avoid implying that the consumer owns the
export. The producer still owns the ledger.

Tests: existing power/job import tests should pass unchanged in behavior; add a focused
test that a denied grant does not mutate consumer derived state.

### Patch PA3: Stale Hint Denial Behavior

Goal: define what happens when a directory hint points at a producer that can no longer
grant.

Default rule:

- the producer denies,
- the consumer leaves the demand unserved for this tick phase,
- the next tick may request again from the refreshed hint set.

Do not add retry loops inside the same tick until there is a concrete gameplay need. A
single denial is simpler and keeps the waiting state bounded.

Tests: stale hint -> denied request -> consumer remains unpowered/unemployed for that
phase; next tick can recover after hints refresh.

### Patch PA4: Allocation Lifecycle Cleanup

Goal: keep the current full release/re-request behavior correct and explicit.

- Release old import allocations before requesting the new generation.
- Route releases to known producer regions when that information exists.
- If producer tracking is missing for a path, broadcast is allowed only as a temporary
  conservative fallback and must be documented with a TODO.

Tests: a producer grant that the consumer later rejects locally still gets released; a
producer capacity drop cannot leave stale consumer grants pinned forever.

### Patch PA5: Optional Incremental Import Reuse

Goal: optimize full release/re-request only after correctness is locked down.

This is optional. It requires producer-owned invalidation:

- the producer detects capacity or topology changes that affect granted consumers,
- the producer revokes or marks affected grants dirty,
- consumers reuse an import only if their local demand is unchanged and no producer
  invalidation arrived.

Do not implement consumer-only reuse. The consumer cannot know whether the producer's spare
capacity changed.

Tests: unchanged consumer + changed producer revokes correctly; unchanged consumer +
unchanged producer reuses without changing results.

## Non-Goals

- No global snapshot allocation.
- No direct cross-worker ECS access.
- No async runtime or new production dependency.
- No guarantee that cross-region visible timing matches the old single-worker eager path.
- No transit-capacity model; road components remain binary connectivity for now.

## Review Focus

- Producer authority is preserved: only the producer grants capacity.
- Consumer state changes only after a grant.
- Waiting is represented by an explicit tick state and continuation.
- Paused view/local derived refresh never waits on remote imports.
- Stale hints are harmless: they can waste a request, not create a wrong allocation.
