# Fix cross-region job/power export candidate starvation

Status: **fixed** (implemented as one patch, see "Fix implemented" at the end).
Bug found and root-caused by loading a real save (`city1`) through the
compiled engine and instrumenting the live export routing path
(instrumentation has since been reverted — this plan captures what it
proved). **The plan below's root-cause hypothesis (§1–§4) turned out to be
wrong** — building a fast deterministic repro during implementation
disproved it and found the real mechanism. Kept intact below as an honest
record of the investigation; see the final section for what actually shipped.

---

## 1. Introduction — the problem

### The symptom

In `city1`, region 1 has 30 residents and zero local jobs. Two other regions
in the same connected road network offer jobs: region 4 (4 Commercial
buildings, 8 job slots, **no local power plant** — power_providers is empty)
and region 7 (3 Industrial buildings, 9 job slots, **has its own
PowerPlant**). Ticking the loaded save forward 10 in-game days (240 hourly
ticks) converges to a stable, non-improving state:

- Region 7: 9/9 slots filled, stays filled.
- Region 4: 0/8 slots filled, forever.
- Region 1: 9/30 residents employed, 21 permanently unemployed.

This is not a transient "still converging" state — it is stable from day 1
through day 10. Region 4's buildings are confirmed powered and road-connected
at every point checked, so this is not a connectivity problem.

### Confirmed via live trace (not just static reading)

Instrumenting `route_export_request` (`src/core/regions/worker.rs:723-765`)
and re-running the save showed, for **every single** `JobExport` request
region 1 ever generated across the whole run:

```text
raw_component (region 1's network) = [(region 1, net 0), (region 4, net 0), (region 7, net 0)]
availability_hints = [(region 1, []), (region 4, []), (region 7, [9 slot ids])]
route_export_request candidates = [(region 7, net 0)]   ← region 4 never appears
```

The union-find connectivity graph (`build_component_graph`,
`src/core/regions/directory.rs:710-737`) is correct — all three regions are
in one component. But region 4's **published** `spare_job_slot_ids` hint is
consistently empty at the exact moments region 1's requests read it, so
region 4 never even enters the candidate list `route_export_request` builds
(`worker.rs:728-743`). Region 7's hint is never empty, so it absorbs all
demand and never gives region 4 a chance.

A second trace, printing the **freshly computed** hint for region 4 right
before every publish (`worker.rs:578-582`), showed it cycling through five
distinct values with near-equal frequency over the 10-day run: empty, 2, 4,
6, and the full 8 slots. This is a **repeating, mechanical pattern**, not
noise — and it lines up with an hourly cycle (region 4 processed a pending
event 128 times across ~240 hourly ticks).

### Root cause, traced through the code

Region 4 has zero local population, so its own `JobsRegistry` never has
local job seekers (`job_requests_from_world`,
`src/core/resource_registry.rs:470-492`, filters to `world.citizens` — empty
for region 4). Its `remaining_workplaces` is therefore always "all
workplace slots that are currently *effective*"
(`workplace_slots_from_world`, `resource_registry.rs:429-446`, gated by
`is_effective_workplace`, `resource_registry.rs:526-542`:
`powered && road_connected`). Road connectivity is stable. **Power is not**,
because region 4 has no local plant and imports all of it cross-region.

Every hourly tick, `begin_tick_power_phase` (`src/core/simulation.rs:89-114`)
runs:

```rust
ensure_derived_state(world, local_region);   // line 102 — protected recompute
...
power::run(world);                            // line 107 — RAW, unprotected
```

`ensure_derived_state` → `refresh_derived_state_for_world`
(`simulation.rs:270-293`) already knows this is dangerous: it captures every
consumer currently holding an *imported* grant before calling `power::run`
(which "clears every consumer and re-applies only *local* grants", per the
comment at `simulation.rs:276-278`), then restores the ones local power
didn't cover (`reapply_imported_power`, `simulation.rs:314-328`). That
capture-and-restore exists **specifically** to stop imported-powered
buildings from "flash[ing] unpowered until the next tick" (the comment's own
words).

But `begin_tick_power_phase`'s own call to `power::run(world)` at line 107 —
one line after the protected call — has **no such protection**. It runs raw,
clears region 4's buildings' `powered` flag down to local-only truth (zero,
since region 4 generates nothing locally), and the region then pauses in
`TickState::WaitingForPowerExports` (`src/core/regions/runtime/mod.rs:340`,
entered via `reconcile_power_export_allocations`,
`runtime/mod.rs` around 865-907) waiting for a fresh cross-region power grant
that has not landed yet.

Control returns to `process_region_events_with_mode`
(`src/core/regions/worker.rs:517-632`) still inside the **same** per-region
loop iteration for region 4. Right after `process_some_events` returns, the
worker calls `runtime.ensure_derived_state()` (line 557 — a no-op here, ticks
don't mark the world config-dirty) and then, at lines 578-582, computes and
queues region 4's `availability_hints()` for publish — **while region 4's
buildings are still genuinely, transiently unpowered**, mid-flight, waiting
on the very grant request it just sent out. `is_effective_workplace` says
`false` for all 4 commercial buildings at that instant, so
`spare_job_slots_on_network` (`src/core/regions/mod.rs:1171-1195`) returns
empty, and that empty hint is what gets published
(`RegionDirectory::publish_region`, `directory.rs:189-211`).

Region 7 never has this problem — it owns its own `PowerPlant`
(`power_providers` is non-empty in the save), so its buildings' `powered`
flag never depends on a cross-region grant round trip.

Job-export requests are regenerated fresh only once per **day**
(`RegionalTickJobPhase::is_daily`, `src/core/regions/mod.rs:329`,
`continue_tick_to_job_demand_phase`, `mod.rs:1035-1048`), and
`route_export_request` computes its candidate list once, at request-creation
time, from whatever the directory currently publishes
(`worker.rs:728-743`). A request that lands in this narrow post-clear,
pre-grant window sees region 4 as having zero capacity and never adds it to
the candidate list for that request's lifetime — no re-check, no widening.
Since the window recurs every single hour, region 1's daily batch of 21
job-seeking requests has ample opportunity to keep landing in it, day after
day, which is exactly the stable non-convergence observed.

### Why this matters

This is a real, reproducible engine bug, not a "the player built the city
wrong" situation — a producer region whose businesses depend on imported
(not locally generated) power can have its true, available job capacity
permanently invisible to consumer regions, because the availability hint is
sampled during a transient window the power-side derived-state code already
knows how to avoid (and does avoid, in the paused/view-read path) but the
tick path does not.

### Goal

1. A producer region's published job/power availability hints reflect
   **settled** state — never a mid-flight snapshot captured while that
   region's own tick is paused waiting on its own cross-region grants.
2. Region 4 in `city1` (and any region shaped like it — job/power capacity
   with no local generation) reliably gets discovered and used by consumer
   regions within a small, bounded number of ticks, not never.
3. No change to the documented staleness model: cross-region effects may
   still be **one-(sub-)tick-stale** (read the previous step's published
   snapshot); this fix makes the *published* snapshot trustworthy, it does
   not demand same-tick freshness.
4. No behavior change for regions that are locally self-sufficient (like
   region 7) or for the road-report / route publish paths, which are
   unrelated.

---

## 2. Proposal (diagrams welcome)

### The bug, end to end

```text
region 4's own hourly Tick, inside process_region_events_with_mode's per-region loop
──────────────────────────────────────────────────────────────────────────────────
 process_some_events(region 4)
   └─ begin_tick_power_phase(world, region 4)     simulation.rs:89
        ├─ ensure_derived_state(..)                 (protected: captures/restores imports)
        └─ power::run(world)                        line 107 — RAW, clears ALL consumers
                                                      region 4 buildings: powered = false
   └─ reconcile_power_export_allocations(..)
        └─ sends PowerExportRequested (outbound)     tick_state = WaitingForPowerExports
                                                      (grant has NOT landed yet)
 <process_some_events returns>
 ensure_derived_state()                              worker.rs:557 — no-op (not config-dirty)
 changed_summaries.push((region4, links,
     runtime.state().availability_hints()))           worker.rs:578-582
        └─ is_effective_workplace = powered && road_connected
              powered == false  ⇒  0 effective workplaces ⇒ spare_job_slot_ids = []
 publish_region_summary(region4, ..., [])             worker.rs:585-587 (later this pass)
        └─ RegionDirectory now says region 4 has ZERO spare job capacity
                                                       (true capacity: 8, once the grant lands)
──────────────────────────────────────────────────────────────────────────────────
 (a later pass) region 4 receives ApplyPowerExportGrant, powered flips back true,
 but region 1's TODAY's batch of JobExportRequested already sampled the empty hint
 and got a candidate list of [region 7] only — no way to reconsider region 4 today.
```

### The fix

Gate the hint-publish step on the region's own tick being **settled**
(`TickState::Idle`), not mid-flight. `TickState::is_waiting()` already exists
(`runtime/mod.rs:346-357`) — this only needs a thin accessor and one `if` at
the publish site:

```text
process_region_events_with_mode, per-region loop (worker.rs:530-583)
──────────────────────────────────────────────────────────────────────
 ... process_some_events(runtime) ...
 ensure_derived_state()
 if is_road_topology_dirty() { ... unchanged ... }

 if runtime.is_tick_settled() {                 ◄── NEW gate
     changed_summaries.push((source_region,
         runtime.state().network_border_links(),
         runtime.state().availability_hints()));
 }
 // else: this region's own tick is paused waiting on its own cross-region
 // grants (power/job/goods) — its local derived state is known-transient
 // right now. Keep whatever hint the directory already has (the last
 // settled snapshot) instead of overwriting it with a mid-flight one.
```

Once region 4's tick fully resolves (its power grant lands, `TickState`
returns to `Idle`), the **next** pass where region 4 processes any event
(even just the `ApplyPowerExportGrant` arriving) goes through this same
publish step with `is_tick_settled() == true`, and the corrected,
full-capacity hint gets published. This preserves the one-(sub-)tick-stale
model exactly — it just stops the directory from ever caching a hint that is
known, at publish time, to be a transient artifact of an in-flight
request rather than a real capacity reading.

**Determinism**: `is_tick_settled()` reads the region's own `TickState`,
which is purely a function of what events it has processed so far — same
inputs, same event order, same result, on any thread. No change to the
`ForwardedEventOrderKey` sort or barrier semantics. Nothing here changes
*when* events are delivered, only *whether* a hint gets republished on a
given pass — the directory's snapshot is still exactly one worker-pass stale
at most, same as every other summary it publishes.

---

## 3. Important structures / functions

### `RegionRuntime::is_tick_settled(&self) -> bool` — new, `src/core/regions/runtime/mod.rs`

Thin accessor next to the existing `pending_event_count()`
(`runtime/mod.rs:669-671`): `!self.tick_state.is_waiting()`. Owns nothing;
pure read of the private `tick_state` field. Contract: `true` only when the
region's own tick (power/job/goods phases) has fully resolved and its local
derived state is not a snapshot of a mid-flight cross-region request.
`TickState::is_waiting()` (`runtime/mod.rs:348-357`) already covers all four
paused variants (`WaitingForPowerExports`, `WaitingForPowerSettlement`,
`WaitingForJobExports`, `WaitingForGoodsExports`), so this one accessor
covers power, jobs, and goods uniformly — no per-resource special-casing
needed.

### `process_region_events_with_mode` — changed, `src/core/regions/worker.rs:517-632`

The only change is gating the `changed_summaries.push(...)` at lines
578-582 behind `runtime.is_tick_settled()`. Everything else in the function
(road-report repricing gate, outbound routing, release-before-request
ordering) is untouched. **ponytail ceiling**: this is a coarse, per-region
gate — a region with, say, its power settled but its (separate) goods export
still pending would ALSO skip publishing its (now-correct) job hint, since
`is_waiting()` doesn't distinguish which resource is still in flight. That's
fine for `city1`'s shape (region 4 has no local jobs *or* goods demand to
create a goods-phase delay independent of its power phase) but is a real
simplification — upgrade path: split `TickState::is_waiting()` into
per-resource flags if a region ever needs its job hint to publish while its
goods phase is still separately resolving.

### No change needed to `route_export_request` / `route_export_request_result`

Investigated as a candidate root cause (a request tries only
`candidates[0]` first, escalating on denial) — this is **not** the bug.
`ExportAllocationRequest.candidates` (the escalation list) is computed fresh
per request from the current snapshot; the problem is that the snapshot
itself was wrong at read time, not that escalation through a correct
snapshot is too narrow. Once the published hint is trustworthy, a request
that legitimately finds only one candidate (real momentary scarcity) should
still only see one candidate — that's correct, not a bug.

---

## 4. Pseudocode + interaction with current code

```rust
// src/core/regions/runtime/mod.rs — new method on RegionRuntime, near pending_event_count (line 669)
pub(crate) fn is_tick_settled(&self) -> bool {
    !self.tick_state.is_waiting()
}
```

```rust
// src/core/regions/worker.rs — process_region_events_with_mode, replacing lines 578-582
if runtime.is_tick_settled() {
    changed_summaries.push((
        source_region,
        runtime.state().network_border_links(),
        runtime.state().availability_hints(),
    ));
}
```

Everything downstream of `changed_summaries` (the publish loop at
`worker.rs:585-587`, `RegionDirectory::publish_region`'s idempotent
keyed-upsert-and-rebuild at `directory.rs:189-211`) is unchanged: it already
correctly handles "this region didn't change this pass" (the idempotency
check at `directory.rs:203-205`) — it just needs to stop being *told* a
transient snapshot is a change.

### Interaction with existing tests

- `export_routing_reads_published_directory_without_rebuilding`
  (`worker.rs:1227-1260`) and `goods_export_request_routes_to_producer_and_back_to_caller`
  (`worker.rs:1300-1354`) construct fresh `RegionRuntime`s via
  `RegionRuntime::new(RegionState::new(...))`, which start `tick_state:
  TickState::Idle` (`runtime/mod.rs:553`) and never tick — `is_tick_settled()`
  is `true` throughout, so these are unaffected.
- `add_region`'s initial publish (`worker.rs:286-290`) runs before any tick
  starts on a freshly attached runtime — also always settled, unaffected.
- The road-report tests (`build_road_republishes_road_report`,
  `clean_region_refreshes_routes_after_neighbour_road_change`, etc.) exercise
  `publish_region_road_report`, a completely separate publish path from
  `publish_region`/availability hints — unaffected.

---

## Decisions locked

- Fix the **publish gate**, not the export-request escalation logic — the
  escalation logic is already correct given a trustworthy snapshot.
- `is_tick_settled()` is a single, resource-agnostic flag (mirrors
  `TickState::is_waiting()`'s existing granularity) rather than three
  separate per-resource flags — simplest fix that closes the observed bug;
  named as a ponytail ceiling above with its upgrade path.
- No change to `begin_tick_power_phase`'s raw `power::run` call itself
  (`simulation.rs:107`) — leaving the *local* World state transiently
  unpowered mid-tick is fine and matches the documented model; the bug was
  only in *publishing* that transient state to other regions as ground
  truth.

## Risks / notes

- **Unverified corner**: load-time settlement
  (`RegionalGameRunner::settle_power_imports`,
  `src/core/regional_game_runner.rs:372`) is a separate codepath from
  `process_region_events_with_mode` and was not traced in this
  investigation. P-a should confirm it does not publish a hint while a
  freshly loaded region is still mid-settlement (if it does, it needs the
  same gate).
- **Re-baselining**: none expected — this only makes the directory publish
  *less* often (skips transient-state publishes it currently makes), so no
  existing "republishes on X" assertion should flip to failing; only a new
  regression test asserting the *previous* bug is fixed should change.
- **Perf**: strictly fewer publishes per pass (skips are a no-op, not extra
  work) — no new cost.
- **Staleness edge**: a region that never fully settles (e.g., permanently
  stuck retrying a power request every hour because *no* power producer
  exists anywhere in its component) would now publish its hint even less
  often than today. That's the correct outcome — an unsettled/unpowered
  region genuinely has nothing trustworthy to advertise — but worth a
  regression test confirming it doesn't get worse (e.g., stuck advertising
  a *stale-good* hint forever after going permanently unsettled). Cover this
  in the P-a test suite alongside the main repro.

## Patch split

```text
P-a  Repro + confirm root cause
       - Minimal deterministic 3-region integration test (region A: residents,
         no local jobs; region B: jobs, no local power, imports from region C;
         region C: jobs + own PowerPlant) mirroring city1's shape but without
         the 579KB save file.
       - Assert the CURRENT bug: after N days, region B's jobs stay at 0
         filled while region C saturates and A has residual unemployment
         that never drops further — this test should FAIL before the fix.
       - Add RegionRuntime::is_tick_settled(); assert it correctly reads
         false immediately after begin_tick_power_phase pauses a region in
         WaitingForPowerExports, and true once Idle again (a runtime-level
         unit test, no full simulation needed).

P-b  The fix
       - Gate process_region_events_with_mode's changed_summaries push on
         is_tick_settled() (worker.rs:578-582).
       - P-a's repro test should now PASS: region B's jobs get discovered
         and filled within a small bounded number of days.
       - Add the regression test named in Risks/notes (a permanently-unsettled
         region's hint does not regress to worse-than-today).

P-c  (only if P-a's load-time check in Risks/notes finds a problem)
       - Apply the same settled-gate to RegionalGameRunner::settle_power_imports
         if it turns out to publish a premature hint at load time.
```

P-a is small and self-contained (new test + new accessor, no behavior
change) — land and review it first so the regression test is in place and
red before P-b's actual fix goes in. P-b is the real change but is a
one-line gate plus its test; P-c is conditional on what P-a's audit finds.

---

## Fix implemented (2026-07-03) — as one patch, not the P-a/P-b/P-c split

Diff: 3 files, +315/-5 lines
(`src/core/regions/mod.rs`, `src/core/simulation.rs`,
`tests/regional_multi_region_play_test.rs`). Full gate green
(`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
`cargo test -q` — 313 lib tests, up from 310, plus the new integration test).
Staged, not committed, pending final sign-off. The user asked for a single
patch instead of the plan's P-a/P-b/P-c split,
so everything below landed together.

### The plan's hypothesis was wrong

The plan above proposed gating `RegionDirectory` hint publishing on
`TickState::Idle`. I implemented exactly that (added
`RegionRuntime::is_tick_settled()`, gated the `changed_summaries` push in
`process_region_events_with_mode`), wrote a fast deterministic 3-region repro
test to replace slow iteration on the 579KB save file, and **the repro test
still failed with that fix in place.**

Instrumenting `RegionRuntime::process_job_export_request` directly (a
temporary `eprintln!`, since reverted) showed the *producer's own live*
`self.state.spare_job_slots_on_network(...)` read — not the discovery hint —
was returning empty at grant time, even though the discovery layer had
already correctly listed the producer as a candidate. The hint-staleness
hypothesis was addressing a symptom that, in a working single-worker-or-not
scenario, doesn't actually gate the outcome: candidate discovery was fine;
the producer just couldn't honor the request when asked.

### The real root cause

`src/core/simulation.rs::begin_tick_power_phase` runs at the start of every
hourly tick and calls `power::run(world)` raw and unprotected. `power::run`
(`src/core/systems/power.rs:18-19`) unconditionally clears **every** power
consumer's `powered` flag and `source`, then only reapplies grants backed by
*local* generation. A sibling function a few lines above in the same file,
`refresh_derived_state_for_world` (used by the paused-config-read path, e.g.
`inspect`/`view`), already guards against exactly this hazard with a
capture-then-restore pattern, and says so explicitly in its own comment:
clearing a consumer's already-valid imported grant here "would drop
still-valid imports and make imported-powered buildings flash unpowered
until the next tick." `begin_tick_power_phase` was simply missing that same
guard — an omission, not a deliberate design choice.

Concretely: region B (no local `PowerPlant`, all power imported) processes
its own hourly `Tick`. `begin_tick_power_phase` clears its Commercial
building's power. B then enters `TickState::WaitingForPowerExports` to
re-request its import. **By deliberate, documented design**
(`pop_next_runnable_event`, `runtime/mod.rs:801-804`: *"producer-side
requests must also run [while waiting], otherwise two regions that both
consume and export on different networks can deadlock each other"*), B is
still required to answer *incoming* requests — like region A asking B for a
job slot — while its own power negotiation is unresolved. At that exact
moment B's Commercial building is unpowered, so `is_effective_workplace`
(`powered && road_connected`) is false, so B correctly-given-its-current-state
reports zero spare job slots and denies A. Because this ordering is fully
deterministic (same region/event iteration order every time), the same
denial recurs identically every single day, forever — matching the observed
"stuck at 0 forever" pattern exactly, and explaining why the *far* region
(self-powered, never in this position) absorbed all demand while the *near*
region (power-importing) got none.

Deferring incoming producer-side requests until the producer's own tick
settles was **not** a viable fix — that is precisely the deadlock the
existing code comment above warns about.

### Diagram — the bug, one hourly tick for region B

```text
THE SETUP — a three-region chain, all road-connected
════════════════════════════════════════════════════

   Region A                Region B                  Region C
  ┌─────────┐             ┌─────────┐               ┌─────────┐
  │ houses   │──road──────│ jobs     │───road────────│ jobs     │
  │ 30 pop   │             │ 2 slots  │               │ 2 slots  │
  │ 0 jobs   │             │ NO plant │               │ has own  │
  └─────────┘             └─────────┘               │ PowerPlant│
                                  ▲                   └─────────┘
                                  │
                          imports power from C
                          across this border


OBSERVED RESULT after any number of days:
  Region C: 2/2 jobs filled, stays filled.       ✔ works
  Region B: 0/2 jobs filled, forever.             ✘ never fills
  Region A: some residents permanently unemployed, even though
            B genuinely has open jobs the whole time.
```

```text
ONE HOURLY TICK FOR REGION B (this repeats identically every hour)
════════════════════════════════════════════════════════════════

  B's inbox (FIFO):  [ Tick ]  [ ProcessJobExportRequest (from A) ]
                         │              ▲
                         │              └── arrived earlier this same
                         │                  pass, because A got its turn
                         │                  before B and already asked
                         ▼
  ① begin_tick_power_phase(B's world)
        power::run(world)
          → clears EVERY consumer's `powered` flag, including
            B's Commercial building, which has no local plant
        B's Commercial building is now: powered = false
                         │
                         ▼
  ② B enters TickState::WaitingForPowerExports
        sends PowerExportRequested → C          (just QUEUED, not
                                                   answered yet — a
                                                   full round trip)
                         │
                         ▼
  ③ B is now "waiting," but by EXISTING, DELIBERATE design:

        "while waiting, only export control events run —
         producer-side requests must also run, otherwise two
         regions that both consume and export on different
         networks can deadlock each other"

        So B does NOT defer A's queued request. It answers it
        RIGHT NOW, using B's CURRENT state:
                         │
                         ▼
  ④ process_job_export_request(A's request)
        available = spare_job_slots_on_network(B)
                   = [ ]        ← B's building is unpowered
                                  RIGHT NOW (see step ①), and its
                                  own power request from step ②
                                  hasn't even reached C yet
        → DENY  (not because B lacks jobs — because B hasn't
                  gotten its OWN power back yet)
                         │
                         ▼
  ⑤ route_export_request_result: candidates exhausted for B,
        escalate to next candidate → region C
        C is self-powered, always available → GRANTS
        → the citizen ends up employed in C, never in B
                         │
                         ▼
  ⑥ (later, same or next pass) B's OWN power request to C
        finally resolves and B's building gets repowered —
        but too late: today's batch of job requests from A
        already got denied and rerouted to C. Nothing retries
        B until TOMORROW's fresh batch — which hits the exact
        same race, in the exact same order, since everything
        here is fully deterministic.

  RESULT: B is *never* actually broken — it just always answers
  at the one moment per hour when it looks broken.
```

Two tempting fixes, both wrong (and why):

```text
  ✘ Don't clear power at the top of the tick
      → then B's OWN fresh power request never gets triggered
        (pending_power_demands skips anything already marked
        "powered"), while the OLD reservation on C's ledger
        still gets released unconditionally every tick anyway
        → B ends up "powered" forever with NOTHING backing it
        on the producer side. (This is exactly what the first
        attempted fix did — caught in review, round 1.)

  ✘ Defer A's request until B's own power settles
      → forbidden by the EXISTING deadlock-avoidance comment:
        if B is waiting on C, and C is ALSO simultaneously
        waiting on B for some other resource, and each defers
        the other's request until it settles, NEITHER ever
        settles. Real risk, already documented, off the table.
```

The fix threads the needle: capture B's held import *before* the clear,
let the fresh-demand collection see the true cleared state (so B still
correctly re-requests every tick, no desync), and only *afterward*
optimistically restore `powered = true` — protecting exactly the narrow
window in step ④ where another region might ask, without ever suppressing
B's own renewal:

```text
        capture (old import, still "valid" — not yet released)
               │
               ▼
        power::run   (raw clear, unchanged)
               │
               ▼
        collect fresh demand   ← sees the TRUE cleared state,
               │                 so B's own renewal still fires
               ▼
        restore "powered=true"  ← now protects step ④'s read,
                                   without breaking renewal
```

(Two more edge cases fell out of review on this exact sequence — a denied
replacement grant needs to actively *unclear* the optimistic restore
[round 2], and a consumer that's lost its border connection entirely must
never get the optimistic restore in the first place, since no reply will
ever arrive to correct it [round 4]. Both are covered by their own
regression tests — see "The fix," "Tests," below.)

### The fix (two coordinated changes, found via codex review)

**1. `RegionState::begin_tick_power_demand_phase`** (`src/core/regions/mod.rs`)
now captures each consumer's currently-held imported grant *before* the raw
clear, but — unlike the naive first attempt — restores it *after*
`pending_power_demands()` has already run, not before:

```text
capture imported_power_grants(&world)      (pre-clear snapshot)
        |
        v
begin_tick_power_phase(&mut world, id)     (raw power::run — unchanged)
        |
        v
pending_power_demands()                    sees the TRUE cleared state,
        |                                  so an already-imported consumer
        |                                  IS correctly re-included in this
        |                                  tick's fresh demand batch
        v
reapply_imported_power(&mut world, ..)     restore AFTER demand collection —
                                            protects only reads that happen
                                            LATER this same pass (e.g. B
                                            answering A's incoming job
                                            request before B's own fresh
                                            grant round-trips back)
```

The first version of this fix (reviewed and rejected by codex, HIGH
severity) restored *before* demand collection, which made an already-powered
consumer invisible to `pending_power_demands()` — so it silently dropped out
of the fresh request batch entirely, while `reconcile_power_export_allocations`
*still* unconditionally released the producer's old reservation every tick
(`runtime/mod.rs:870-876`, `931-951`, "release all previous allocations for
this caller generation, then request all current demands" — an intentional,
documented eager-reconciliation policy). Net effect of the first attempt:
permanent "free," unbacked imported power. Reordering around the existing
`pending_power_demands()` call closes that gap — demand collection sees
truth, only later same-pass reads see the optimistic restore.

**2. `RegionState::apply_power_export_grant`** (`src/core/regions/mod.rs`) —
codex's second-round finding: the fresh request that follows the optimistic
restore might come back **denied** (the producer genuinely has no spare
capacity). The existing denial branch only invalidated the jobs-registry
cache and returned — it never touched the optimistically-restored
`powered`/`source` fields or the `total_power_supplied` stat. Fixed: on
denial, if the consumer is currently marked powered, clear `powered`/`source`
and subtract the phantom `total_power_supplied`, mirroring the bookkeeping
shape of the existing granted-path code just above it.

`imported_power_grants` and `reapply_imported_power` (`src/core/simulation.rs`)
were bumped from private `fn` to `pub(crate) fn` so `regions/mod.rs` — a
different module — can call them; no behavior change to either function.

### Diagram — the fixed sequence, one region's hourly tick

```text
begin_tick_power_demand_phase (mod.rs)
  imported = capture(world)              ["still valid, not yet released"]
  begin_tick_power_phase(world)          [raw clear — unchanged]
  power_demands = pending_power_demands()  [sees TRUE cleared state]
  reapply_imported_power(world, imported)  [optimistic restore, same-pass reads only]
        |
        v  (returned to caller, e.g. reconcile_power_export_allocations)
  release OLD reservation (unconditional, every tick — unchanged)
  send FRESH PowerExportRequested for power_demands (now correctly includes
    the already-imported consumer — this is what closes the desync)
        |
        v  (later, same or a subsequent pass)
  apply_power_export_grant(fresh_reply)
    granted  -> re-confirm powered=true (existing path, unchanged)
    denied   -> clear powered/source, unwind total_power_supplied  [NEW]
```

### Review

- **codex, round 1**: HIGH — the naive capture/restore desynced the
  caller's local "still powered" flag from the producer's reservation
  ledger (permanent free power). Root-caused and fixed by reordering the
  restore to after demand collection, per above.
- **codex, round 2**: HIGH — the reordered fix didn't unwind the optimistic
  restore on a subsequent denial, so a lost export could stay locally
  powered forever with nothing backing it. Fixed by adding the denial
  branch to `apply_power_export_grant`.
- **codex, round 3**: no findings, confirmed ready to commit. Also confirmed
  my reasoning that already-issued job/goods export grants are not
  retroactively unwound when a power denial lands later — this is a
  pre-existing, already-accepted property of the power/jobs decoupling in
  this codebase (effective-workplace status is recomputed fresh on every
  read, not cached in a way that needs explicit cross-resource
  invalidation), not a new hazard this patch introduces.
- **Human review, round 4** (found and reported directly, before the round-3
  "ready" commit landed — the commit was reverted with `git reset --soft` and
  the patch kept staged for this exact reason): HIGH — `pending_power_demands`
  can skip a consumer entirely, not just return `granted: false` for it later
  — either the whole region has no border link at all
  (`mod.rs:1316-1318`, early return), or this specific consumer's local
  network isn't adjacent to any border-linked network
  (`mod.rs:1337-1349`, skipped via `let`-`else`). Either way, no fresh
  `PowerExportRequested` is ever sent for that consumer, so no reply —
  granted or denied — ever arrives, so round 2's denial-cleanup branch never
  runs (it's only reachable from an incoming reply). The unconditional
  `reapply_imported_power(&mut self.world, &imported)` call restored it
  anyway, leaving a disconnected consumer (e.g. after a connecting road gets
  bulldozed) optimistically "powered" forever with the old, already-released
  reservation behind it. Fixed with the reporter's own suggested minimal fix:
  build a `HashSet<Entity>` from `power_demands.iter().map(|d| d.consumer)`
  and filter the captured `imported` list down to only entries whose entity
  is in that set before passing it to `reapply_imported_power` — a consumer
  gets the optimistic restore only if a fresh request is actually in flight
  for it this tick. Re-reviewed by codex (round 4): no findings, confirmed
  the filter is correct and complete, and confirmed the two other
  capture-then-restore call sites (`refresh_derived_state_for_world`,
  `power_import_settlement_demands`) are exempt from this same class of bug
  for reasons specific to their own call sites (see Risks/notes).
- **Self-review**: mission-scoped to the actual root cause plus the three
  directly-caused correctness fixes review surfaced (no unrelated refactors);
  UI untouched (pure `core` simulation logic); deterministic (reorders/filters
  operations within one region's own tick, no new cross-thread dependency);
  tests meaningful (all four confirmed to fail without their respective fix,
  via temporary revert-and-rerun); no balance/economy change — this restores
  intended behavior, it doesn't change capacity numbers.

### Tests

- `tests/regional_multi_region_play_test.rs::power_importing_producer_region_eventually_gets_remote_workers`
  — the original repro: a 3-region chain (A: residents/no jobs; B: jobs/no
  local power, imports from C; C: jobs + own PowerPlant), 5 in-game days,
  asserts B gets at least one remote worker (previously always 0) and C also
  does (confirming the fixture's demand genuinely exceeds one producer's
  capacity alone). Fast (well under a second) — this is what replaced
  iterating on the real 579KB save file for the rest of the investigation.
- `core::regions::tests::begin_tick_power_demand_phase_still_requests_fresh_demand_for_an_imported_consumer`
  (`src/core/regions/mod.rs`) — codex round-1 regression: an already-imported
  consumer must still appear in the freshly-collected `power_demands`.
- `core::regions::tests::apply_power_export_grant_denial_clears_the_optimistic_restore`
  (`src/core/regions/mod.rs`) — codex round-2 regression: a denied
  replacement grant must clear the optimistic restore and unwind the
  supplied-power stat, not leave it stuck powered forever.
- `core::regions::tests::begin_tick_power_demand_phase_does_not_restore_a_disconnected_consumers_import`
  (`src/core/regions/mod.rs`) — round-4 regression: a consumer with no
  border-connected network to request through must not be optimistically
  restored, since nothing will ever arrive to confirm or deny it.

### Risks / notes

- The plan's original "gate hint publish on `TickState::Idle`" idea
  (`RegionRuntime::is_tick_settled()`) was implemented, tested working
  against a stale hint scenario, then found unnecessary once the real root
  cause was identified, and was fully reverted (confirmed via `git stash`
  test-and-drop) rather than left in as unused/speculative code.
- `settle_power_imports` (`src/core/regional_game_runner.rs:372`, load-time)
  was audited per the plan's own risk note and found **not** to share this
  bug — its own doc comment already states its purpose is a deliberate,
  one-shot "clear and force fresh renegotiation," with no "already-valid,
  in-flight-this-tick" grant to protect (unlike `begin_tick_power_phase`,
  which runs mid an already-ongoing tick). No change made there.
- `tick_local` (`src/core/regions/mod.rs:407-412`, the single-region-only
  tick path with no cross-region export capability at all) also calls
  `begin_tick_power_phase` directly and was deliberately left untouched —
  a single region has no neighbor to import power from, so
  `imported_power_grants` would always be empty there regardless.
