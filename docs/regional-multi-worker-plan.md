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

## Direction (decided 2026-06-14): adopt the one-tick-stale cross-region model

We are switching the target cross-region model from **live, strongly-consistent**
resolution to **snapshot-consistent, one-tick-stale** resolution ("Option B", written up
in full under [Deferred optimizations](#deferred-optimizations-only-if-profiling-at-scale-shows-they-matter)
-- now promoted from a deferred alternative to the chosen direction).

**What changes.** Each region resolves its cross-region imports against the directory
snapshot **as published at the end of the previous step**, not against live cross-region
state. A neighbor's change is reflected one tick later. This keeps determinism
(reproducibility / assignment-independence) and drops only within-tick synchronicity --
see the Option B writeup for why a uniform staleness rule stays deterministic.

**Why.** It collapses most of the cross-region machinery. Allocation becomes a
deterministic function of a frozen shared snapshot that every region computes the same
way, so the following are **retired**:

- the M3 **determinism barrier** (nothing to order -- delivery order stops mattering),
- the **request / grant / release** export event choreography,
- the **producer-owned live allocation ledger** (and the `producer_regions` release
  fan-out),
- the **`TickState` pause-tick machine** (a tick never waits on a cross-region value --
  it reads the snapshot and runs straight through), and
- the **multi-pass-per-tick drain** (one tick = one pass again).

**What is kept / reused:**

- **M1 + M2** -- the coordinator `RegionDirectory` and especially its **double-buffered
  `Mutex<Arc>` snapshot** are exactly the read-frozen / write-next primitive this model
  needs. M2 is now load-bearing, not just cross-thread-ready storage.
- the **region->worker ownership map** and general cross-worker forwarding (for
  commands / ticks / snapshots -- needed by any multi-worker model),
- the **stable order key** (M3), repurposed as the deterministic tiebreak *inside* the
  allocation function,
- a coarse **step-level join** (all regions tick against the frozen snapshot, then their
  new summaries become the next snapshot) -- a simple phase boundary, not a per-tick
  state machine.

**Accepted cost (a balance decision, not a free optimization):**

- A one-tick lag on imported power/jobs. To preserve determinism the **single-worker
  path adopts the same stale rule**, so this revises the Non-Goal "no change to the
  single-worker observable behavior."
- Chained cross-region effects (A->B->C) lag one tick **per hop** unless the allocation
  function resolves a whole component from the snapshot in one shot (a lag-vs-compute
  knob).
- Watch for **oscillation** (a consumer flipping powered/unpowered each tick reacting to
  stale numbers); add a damping rule if it shows up.

**Status of the in-flight patches.** M1 and M2 are committed and are kept as-is. **M3
(the barrier) is in the working tree and is now superseded by this decision** -- its
barrier, the live ledger, and the pause-tick are things this model deletes rather than
extends. Whether to still commit M3 as a checkpoint, or to pivot directly to Option B, is
called out as an open decision below; the directory/ownership-map/order-key parts of M3
survive regardless. The staged patches M4-M6 are re-scoped to the new model (spawn N
workers + step join; configurable setup; live moves) and no longer carry the barrier.

The "Target shape", "The determinism problem", and the M3 description further down
describe the **superseded** live-barrier model; they are retained for history. The
authoritative target is this section plus the Option B writeup.

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
- **Subsumes `worker.rs:316 TODO(CR2 perf)`** (discovery rebuilt per export request).
  Today `route_export_request` calls `cross_region_discovery` for every request;
  routing is topology-stable within a pass, so the directory is built once and reused
  for all requests in that pass (and later refreshed only on topology change). Post-R5
  the registry resolves behind the hints are already cached, so the rebuild that
  remains is `network_border_links` road-network discovery + the union-find graph --
  which the directory removes from the per-request path.
- No behavior change: the existing CR2/CR3 suites must pass unchanged (the proof).

Tests: existing cross-region tests green; a unit test that the directory yields the
same components/candidates the worker computed before, built once per pass rather
than once per export request.

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
- **Narrow the allocation-release fan-out** (subsumes `worker.rs:388 TODO(CR2
  scale)`). Today a caller's `ExportAllocationRelease` is broadcast to every owned
  region; cross-worker that would mean broadcasting to every region on every worker.
  Instead, the caller tracks the producer regions it received `granted` replies from
  (recorded in `apply_*_export_grant` on every granted reply, *not* only on grants it
  locally applies -- a producer reserves on grant even when the caller's apply
  early-returns), carries that set on the release, and the worker routes the release
  only to those producers. Invariant: the targeted set must be a superset of
  producers holding the caller's stale allocations, or a forgotten producer pins a
  reservation forever (a silent leak). Adding a `Vec<RegionId>` to
  `ExportAllocationRelease` drops its `Copy` derive -- clone it per target at the
  routing call site.
- Still may run two workers only in tests here; production can stay one worker until
  M4.

Tests (the hard gate): a **parity guard** -- run a scripted multi-region sequence
(builds, ticks, cross-region power and jobs, save/load) on 1 worker and on 2 workers
and assert identical `powered`, job assignments, and `world.stats` at every step.
Any divergence is a determinism bug.

Plus, for the narrowed release fan-out: a **silent-leak regression test** -- a
producer grants an allocation, the caller's local apply rejects it (e.g. the
consumer is already powered), and the next-generation release must still reach that
producer so its reservation is dropped, not pinned. This is the test that proves the
"record on every granted reply" invariant above.

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
- **Subsumes `runtime/mod.rs` `TODO(CR2 lifecycle)`**: today export reservations only
  clear when the caller starts a new generation, so a region that is removed,
  reassigned, or stops ticking would leave its reservations pinned on producers. M6
  must explicitly release a moved/removed region's allocations (and drop allocations
  held *for* it) at the move boundary. Not reachable single-worker (regions are never
  removed and all tick together), which is why it is deferred to here.
- **Subsumes `load_manager.rs` `TODO(multi-worker)`**: when reassignment is wired to a
  scheduler, add a post-move balance check so repeated `RegionMove`s cannot oscillate a
  region between workers.

Tests: a move at a safe point preserves all state and keeps the parity guard green;
a move never strands a pending export (it is denied or carried, never lost); a region
removed/moved leaves no pinned reservation on any producer; repeated balance passes do
not oscillate a region.

## Deferred optimizations (only if profiling at scale shows they matter)

These are not scheduled patches. They are recorded so the corresponding code TODOs
are tracked rather than dangling. Attempt them only after M1-M3 exist and only if
measurement shows the cost is real -- the current simple versions are correct and
cheap at small region counts.

### Change-driven export-allocation reconciliation

Tracks `runtime/mod.rs` `TODO(CR allocation lifecycle)` on
`reconcile_power_export_allocations` and `reconcile_job_export_allocations`.

Today reconciliation is **eager**: every tick (power) / daily tick (jobs) a caller
releases all its previous-generation allocations and re-requests all current demands.
This is correct by construction -- a full teardown+rebuild in deterministic order
can never go stale -- but it churns allocations every tick even when nothing changed.

The goal is to skip that churn for regions that did not change. There are two
granularities for doing so; the **region/component-grained** one below is the
recommended shape, and **per-allocation incremental** is recorded only to say why we
do *not* pick it.

#### Recommended: region/component dirty-recompute

Detect which regions had a producer- or consumer-affecting change this step, and re-run
the *whole* export reconciliation only for those regions (and the regions a change
ripples into), leaving quiescent regions' allocations untouched.

Why this granularity, not per-allocation:

- It keeps **correct-by-construction within a region**: a dirty region still does a
  full teardown+rebuild of its own exports, so there is no partial stale allocation
  state -- we only skip *whole regions* that did not change. That is far less
  error-prone than surgically invalidating individual allocations.
- The "did this region change?" signal already exists: R5 invalidates the registry
  cache at the mutation chokepoints, and the directory (M1/M2) already tracks component
  membership and per-region summaries.
- **Producer-owned allocation makes invalidation natural.** The producer holds the
  reservation ledger keyed by caller, so it *already knows its consumers*. When a
  producer's capacity changes -- a local event the producer sees directly -- it drops
  over-committed reservations and notifies exactly those consumers. Consumers do **not**
  need to track which producer each allocation depends on; the producer drives it.

What this still does not avoid (the genuinely hard parts, identical at any granularity):

- **Cross-region / cross-worker invalidation.** A consumer's reconcile depends on a
  producer's spare that lives in another region's `World` (post-M3, another thread), so
  the producer -> consumer notification still rides the M3 barrier and has the same
  silent-determinism-bug failure mode as the R5 cache if one is dropped.
- **Ripple to a fixed point.** A change in region B that re-takes capacity from producer
  A shrinks A's spare, which can invalidate consumer C *even though C did not change*. So
  the dirty set propagates B -> A -> C ... possibly across the whole component, and must
  reach a fixed point. Eager avoids this by rebuilding everything in one deterministic
  pass; any change-driven scheme must reproduce that fixed point in a deterministic,
  cross-worker order and prove it matches eager.

Workload caveat: the win is proportional to how many regions are **quiescent**. In an
active city, producer/consumer state shifts almost every tick (population growth, rising
consumption, business upgrades), so the dirty set can be most of the active regions and
the scheme collapses toward eager with extra bookkeeping. The payoff is in a **mature
city** -- many settled regions plus a few growing on the frontier. Only profiling tells
us the stable fraction is large enough to matter.

#### Not chosen: per-allocation incremental

Tracking each allocation's producer dependency and surgically invalidating single
allocations is finer-grained but strictly harder and more bug-prone: it reintroduces
partial stale state inside a region, and it needs consumer-side dependency tracking that
the producer-owned ledger otherwise gives us for free. It does not reduce the two hard
parts above, so it buys nothing over region/component dirty-recompute.

#### Shared requirements (either way)

- Prerequisites: M1 (directory, for component-change signals) and M3 (cross-worker
  routing, for the producer -> consumer invalidation notification).
- Gate: an **eager-vs-change-driven parity guard** (change-driven result == eager result
  at every step), mirroring R5's parity guard.
- Keep the eager version as the reference implementation and the fallback.

#### Why there is no safe "consumer-only" shortcut

It is tempting to skip a caller's release+re-request whenever *its own* demand set and
reachable component are unchanged -- all locally observable, no cross-region protocol.
**This is unsafe on its own and must not ship alone.** The grant being reused depends on
the producer's spare, which the consumer cannot see. If the consumer skips its release,
the producer never bumps the generation and keeps the old reservation; should the
producer's capacity have dropped meanwhile (it added local consumers, or granted another
caller), the producer is now over-committed and the consumer silently keeps a stale grant
-- staying powered when eager would have re-resolved and denied it. That is a direct
divergence from eager, exactly what the parity guard catches.

A stale grant is safe only if the producer is *also* unchanged, and the consumer cannot
know that without the producer -> consumer notification -- the expensive half. So the
consumer-local skip cannot be decoupled from it. The minimal *correct* increment is the
pair:

1. a consumer skips its release+re-request only while its local demand/component are
   unchanged **and** it has received no invalidation from any producer it holds an
   allocation on, and
2. a producer, on a local capacity change, detects over-commitment and revokes/notifies
   the affected consumers (which re-enter reconciliation and ripple to a fixed point).

Both halves land together or not at all; (1) without (2) is a determinism bug.

Everything above optimizes *within* the current consistency model: cross-region effects
resolve **within the same tick**, exactly as a single thread would. That within-tick
consistency is what forces the M3 barrier, the eager teardown, and the fixed-point
ripple. The next subsection records the alternative of relaxing it.

### Option B: snapshot-consistent (one-tick-stale) cross-region resolution -- CHOSEN (2026-06-14)

This is now the **chosen target model** (see "Direction" near the top), not a deferred
alternative. The change-driven reconciliation subsections above are retained for context
on why the strong-consistency optimization was harder; they are no longer the plan.

A different model, not just an optimization of the one above. Each region resolves its
imports against the directory snapshot **as published at the end of the previous step**,
not against live cross-region state. A neighbor's capacity change is reflected in your
import *next* tick, not this one.

The key is that this drops *synchronicity* without dropping *determinism*. Two properties
are usually bundled under "deterministic":

- **(A) Reproducibility / assignment-independence** -- same inputs produce the same
  outputs regardless of worker count, region->worker assignment, or thread timing. This
  is non-negotiable: lose it and the parity guard is meaningless and replays diverge.
- **(B) Within-tick synchronous consistency** -- a neighbor's change is visible to you
  the same tick, with no lag.

The eager+barrier design enforces both. Option B keeps **(A)** and drops **(B)**. A
one-tick lag is still fully deterministic *as long as the staleness rule is uniform and
well defined* -- "resolve against the previous step's published snapshot" is a pure
function of prior state, independent of who owns what, applied identically on one worker
or many. So single-worker still equals multi-worker; the shared reference just shifts by
one tick.

Why this is dramatically simpler -- reading a *frozen* prior snapshot removes the three
hard pieces at once:

- **No barrier-for-correctness.** The M3 barrier exists to give live cross-worker
  resolution a canonical within-step order. A frozen snapshot is order-independent --
  there is nothing to order.
- **No within-tick fixed-point.** Each region resolves its imports once, against last
  step's numbers; the B->A->C ripple cannot form because B's change this step does not
  affect A this step.
- **No synchronous invalidation.** The producer just publishes its new capacity (it
  already publishes summaries for hints); consumers read it next step. This is eventual
  consistency and reuses the stale-tolerant directory directly.
- Change-driven skipping becomes *safe*: a region recomputes against the latest snapshot,
  and if its local inputs and the frozen import figures it reads are unchanged, it skips
  -- there is no hidden live dependency that can shift underneath it.

The one knob still to decide -- double-spend. Staleness fixes consistency, not
arbitration: two consumers reading the same published spare could both claim it.

- **B1 (authoritative, recommended):** the producer still arbitrates who gets its real
  capacity, but once per step against a *frozen, sortable* request set (sort by the M3
  order key offline), not via a live barrier. Deterministic, no over-subscription, and it
  still deletes the barrier and the fixed-point. The sweet spot.
- **B2 (optimistic):** consumers take against the published spare; the producer detects
  over-subscription and corrects it next step. Simplest to build, but allows one tick of
  over-spend (too much imported power for a tick).

The cost is real and is a **balance decision, not a free optimization**. A one-tick lag
on imported power/jobs is observable, so to preserve (A) the single-worker path must
adopt the *same* stale rule -- which means Option B **revises the Non-Goal "no change to
the single-worker observable behavior"** below. A 1-tick delay is usually fine and even
realistic for a city sim, but watch for **oscillation**: a consumer that flips
powered/unpowered every tick because it always reacts to stale numbers (bounded, but may
want a damping rule).

Relationship and gate:

- Option B and the change-driven reconciliation above are *alternatives*, not layers:
  change-driven keeps strong consistency and optimizes the churn; Option B changes the
  consistency model so the churn problem largely dissolves. Pick one consistency model.
- Gate: a parity guard against the **stale-model reference** (snapshot-consistent result
  matches a single-worker run that uses the same one-tick rule), not against eager --
  eager is a *different* model under Option B, so it is no longer the oracle.
- Prerequisite: M1/M2 (the published per-step snapshot is exactly the directory product).
  Option B notably does **not** need the M3 barrier for correctness, though M3's order key
  is still the stable tiebreak for B1's offline arbitration.

## Non-Goals

- No async runtime, no work-stealing scheduler, no lock-free exotica; `std::thread` +
  channels only.
- No cross-region transit-capacity model (still binary connectivity, as before).
- No change to the resolution math, balance, or the single-worker observable behavior.
  (Exception: Option B in Deferred optimizations would deliberately revise this, trading
  a one-tick cross-region lag for a much simpler model -- a balance decision, gated
  separately.)
- M6 (live moves) is explicitly optional and may never be needed.

## Review focus

- The coordinator owns summaries and the routing map, never a region's `World`.
- Determinism: the multi-worker result equals the single-worker result, proven by the
  M3 parity guard; the merge-point ordering key is stable and documented.
- Stale hints only misdirect a request; they never produce a wrong allocation.
- No region reads another region's ECS across a thread boundary.
