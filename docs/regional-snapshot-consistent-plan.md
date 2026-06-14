# Regional Snapshot-Consistent Cross-Region Plan (one-tick-stale)

This is the **active forward plan** for cross-region resolution in the multi-worker
engine. It supersedes the live-barrier model staged in
[regional-multi-worker-plan.md](regional-multi-worker-plan.md), which remains the record
of what patches M1-M3 shipped under. This plan builds on infrastructure already shipped
there -- the coordinator `RegionDirectory` and its double-buffered snapshot (M1/M2) -- and
reuses parts of M3 (the region->worker ownership map and the stable order key).

For vocabulary (`World`, `RegionState`, `RegionRuntime`, `RegionWorker`, directory,
snapshot, hints) see [regional-terminology.md](regional-terminology.md).

## The model

Each region resolves its cross-region imports against the directory snapshot **as
published at the end of the previous step**, not against live cross-region state. A
neighbor's capacity change is reflected one tick later.

### It drops synchronicity, not determinism

Two properties usually travel together under "deterministic":

- **(A) Reproducibility / assignment-independence** -- same inputs produce the same
  outputs regardless of worker count, region->worker assignment, or thread timing.
  Non-negotiable: lose it and replays diverge and the parity guard is meaningless.
- **(B) Within-tick synchronous consistency** -- a neighbor's change is visible the same
  tick, with no lag.

This model keeps **(A)** and drops **(B)**. A one-tick lag is still fully deterministic as
long as the staleness rule is uniform: "resolve against the previous step's published
snapshot" is a pure function of prior state, independent of who owns what, applied
identically on one worker or many. Single-worker therefore still equals multi-worker; the
shared reference just shifts by one tick.

### Why it is dramatically simpler

Reading a *frozen* prior snapshot removes three hard pieces at once:

- **No barrier-for-correctness.** A frozen snapshot is order-independent -- there is
  nothing to order, so the M3 determinism barrier is unnecessary.
- **No within-tick fixed-point.** Each region resolves its imports once, against last
  step's numbers; a B->A->C ripple cannot form because B's change this step does not
  affect A this step.
- **No synchronous invalidation.** The producer just publishes its new capacity; consumers
  read it next step. Eventual consistency, reusing the stale-tolerant directory.

### What it retires, keeps, and reuses

Retired from the live-barrier model:

- the M3 determinism barrier,
- the request / grant / release export event choreography,
- the producer-owned live allocation ledger (and the `producer_regions` release fan-out),
- the `TickState` pause-tick machine (a tick never waits on a cross-region value),
- the multi-pass-per-tick drain (one tick = one pass).

Kept / reused:

- **M1 + M2** -- the `RegionDirectory` and its double-buffered `Mutex<Arc>` snapshot are
  exactly the read-frozen / write-next primitive this model needs.
- the **region->worker ownership map** and general cross-worker forwarding (commands,
  ticks, and snapshot requests still route to the owning worker),
- the **stable order key** (M3), repurposed as the deterministic tiebreak inside the
  allocation function,
- a coarse **step-level join** (all regions tick against the frozen snapshot, then their
  new summaries become the next snapshot) -- a simple phase boundary, not a state machine.

## Decision record (2026-06-14)

- **Adopt the one-tick-stale model** over both the live-barrier model and the attempt to
  optimize that model's per-tick churn in place. (For why optimizing the strong-consistency
  model is harder -- a distributed cache-coherence problem -- see "Change-driven
  export-allocation reconciliation" in the multi-worker plan.)
- **Double-spend: B1 (authoritative by shared recompute).** Every region computes the
  *same* deterministic allocation function over the *same* frozen snapshot -- spare filled
  in a stable order (region, network, then the M3 order-key fields) until exhausted -- and
  reads off its own slice. Identical inputs produce an identical result on every worker, so
  there is no over-subscription and no arbitration messages. The redundant per-region
  recompute is deterministic and embarrassingly parallel; it is what lets OB4 delete the
  message path entirely.
  - Rejected: **B2 (optimistic)** -- consumers take against published spare and the producer
    corrects over-subscription next step. Allows one tick of over-spend; we prefer B1's
    no-over-subscription guarantee, and the recompute cost is negligible at the region
    counts in play.
- **Chained dependencies resolve whole-component.** The allocation function resolves a
  whole road component from the snapshot in one shot, so a multi-hop effect (A->B->C) lags
  one tick total, not one per hop. (Cost: more compute per step; revisit if profiling
  bites.)

## Accepted cost and non-goals

- A **one-tick lag** on imported power/jobs. To preserve (A), the **single-worker path
  adopts the same stale rule** -- a deliberate, reviewed change to single-worker observable
  behavior (it revises the corresponding Non-Goal in the multi-worker plan).
- Watch for **oscillation** (a consumer flipping powered/unpowered each tick on stale
  numbers); add a damping rule if it appears.
- No async runtime / work-stealing / lock-free exotica; `std::thread` + channels only.
- No cross-region transit-capacity model (binary connectivity, as before).

## Staged patches

Each patch is independently shippable, small (~5 files / ~400 lines; split if larger), and
gated on tests. The one behavior change (the one-tick lag) lands in exactly one patch, OB3;
everything before it is additive and behavior-preserving, everything after it is dead-code
removal.

### Patch OB1: Publish quantitative cross-region summaries

Goal: extend the directory's published per-region summary from boolean `has_spare` hints
to the quantities the allocation function needs -- per regional road-network: producer
**spare capacity** (power units, job slots) and consumer **import demand**.

- `RegionalAvailabilityHint` grows (or is replaced by a `RegionalNetworkSummary`) with
  `spare_power: i32`, `spare_job_slots: u32`, `power_demand`, `job_demand`, computed in
  `RegionState::availability_hints` from the cached registry. Reuses M2's double-buffer.
- **No behavior change:** the new fields are published but unconsumed; the live export path
  still drives allocation.

Tests: a region's published summary carries correct per-network spare and demand; all
existing cross-region tests green unchanged.

### Patch OB2: Snapshot allocation function (pure, offline)

Goal: a pure `fn allocate_power(component_snapshot) -> map<(region, network), units>` and
the job analog, resolving a whole component deterministically -- spare filled in stable
order (region, network, then the M3 order key fields) until exhausted. No `World` access,
no messages, not yet wired into the tick.

- New module `core/regions/snapshot_allocation.rs` with the function and its tests.
- **No behavior change:** the function exists, unused in production.

Tests (the correctness anchor): characterization cases, **plus a cross-check that for the
same frozen inputs the function's allocation equals what the live eager path grants** --
proving the function faithfully reimplements the allocation semantics *before* the tick
switches to it.

### Patch OB3: Straight-through tick reads the snapshot (the model switch)

Goal: a consumer tick resolves its imports by calling the allocation function over the
**previous step's** snapshot instead of pausing for grants. Retire the `TickState`
pause/resume and the multi-pass-per-tick drain for the export path.

- `simulation.rs`: collapse `begin_tick_power_phase` / `continue_to_job_phase` /
  `finish_tick_after_*` into a straight-through tick that reads imports from the snapshot;
  `RegionRuntime` drops the `WaitingFor*` states.
- **This is the behavior change:** introduces the one-tick lag, applied uniformly so
  single-worker adopts the same rule. Revises the "no single-worker behavior change"
  Non-Goal in the multi-worker plan.
- The request/grant/release events and producer ledger may still *exist* here (now unused)
  to keep OB3's diff bounded; OB4 deletes them.

Tests: the parity reference shifts to the stale model -- a guard that single-worker(stale)
== multi-worker(stale) over a scripted run. **Plus a documented characterization** of how
the stale model differs from the old eager model on a known scenario, so the behavior
change is explicit and reviewed, not silent. Add a damping rule if oscillation
(powered/unpowered flip on stale numbers) shows up.

### Patch OB4: Retire the message-passing export path

Goal: delete the now-unused choreography -- `*ExportRequested/Completed/Released` events,
the producer-owned `ExportAllocations` ledger, `apply_*_export_grant`,
`process_*_export_request`, and the `producer_regions` release fan-out. The barrier's
export-event handling goes; cross-worker forwarding survives for commands/ticks/snapshots.

- `runtime/mod.rs`, `worker.rs`, `mod.rs` (`*ExportGrant` / `Pending*Demand` types).
- **No behavior change:** removing code OB3 made unreachable.

Tests: stale-parity guard stays green; a test/grep asserting no export events remain.

### Patch OB5: Step-level join and N workers

Goal: replace barrier-driven multi-pass scheduling with the coarse **step join** -- each
worker ticks all its regions against the frozen snapshot and publishes new summaries; at
the step boundary the next snapshot becomes active via M2's swap -- and spawn N workers
sharing regions (folds in the old M4).

- `worker.rs` / `threaded.rs` scheduling, `regional_game_runner` (spawn N + own the
  snapshot swap point).
- **No behavior change** vs OB3/OB4: the join only changes how the same computation is
  parallelized.

Tests: an N-region game on K workers == 1 worker, identical over the stale-parity script;
assignment-independent.

### Carried forward (from the old plan, re-scoped to no barrier)

- **Configurable worker setup from a file** (old M5): still a performance knob, still
  assignment-independent -- now trivially so, since the result is a function of the
  snapshot, not of scheduling.
- **Live region reassignment** (old M6): simpler here -- a moved region just starts
  publishing/reading on its new worker at a step boundary; there is no in-flight export
  allocation to strand (there are no allocations).

## Gate and review focus

- **Determinism (A):** single-worker(stale) == multi-worker(stale), for any worker count
  and any assignment, proven by the stale-model parity guard. The oracle is single-worker
  under the *same* one-tick rule -- **not** the old eager model.
- The allocation function is a **pure function of the published snapshot**; no region reads
  another region's ECS, on any thread.
- **OB2's cross-check** against the eager allocator de-risks the model switch before OB3
  flips the tick over.
- The one-tick behavior change is documented via an explicit eager-vs-stale
  characterization, not landed silently.
