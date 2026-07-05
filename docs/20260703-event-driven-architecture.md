# Event-driven state propagation — slim the tick down to the time pass

Status: **done.** All six patches (P-1..P-6) implemented and committed on
branch `event-driven-architecture`.

## The idea

The ask, paraphrased: *build a power plant → that should ripple out as
events (power → jobs → hints → other regions notice), not get rediscovered
from scratch every hour.*

Half of this already existed. **Within** one region, the codebase already
works this way: a change flips a dirty flag, the next read recomputes lazily.
What wasn't event-driven was everything **between** regions — every hour,
every region re-announced its status and re-negotiated its imports/exports,
whether or not anything had changed. This plan makes cross-region
communication follow the same rule the rest of the codebase already uses.

**Naming this precisely: this is *not* genuine event-driven architecture.**
Nothing here pushes work to happen the instant something changes,
independent of anything else. The system is still entirely **tick-driven**
— the tick loop is the only thing that ever looks at any flag or counter
this plan adds. What changed is that the tick got *cheap*: instead of
unconditionally redoing every resource's work each time it runs, it now
asks "did anything change since I last checked?" and skips if not. That's
**lazy recomputation via dirty flags**, not event dispatch. A truly
event-driven version would have building a power plant *directly* trigger
the recompute and *directly* notify the specific regions that care, with no
tick in the loop at all — this plan deliberately doesn't build that (no new
event bus, no message types — see "The fix, mentally," right below).
"Event-driven" stays in this doc's title because it's the name the mission
started with, not because the result earns the term literally.

## The problem, in one sentence

Every tick, for every region, whether or not anything changed: power state
gets wiped and rebuilt, import/export deals get cancelled and
re-negotiated, and availability hints get recomputed and republished. A
quiet city paid the same cost as a busy one.

## The fix, mentally

Two dirty flags, reused everywhere — no new event bus, no new message types:

- **Local**: one `Cell<bool>` per resource (power/jobs/goods) per region,
  flipped by the *same* functions that already run when something relevant
  changes (placing a building, bulldozing a road). One extra line inside
  code that already exists — nothing new to call.
- **Cross-region**: one counter that ticks up whenever the shared directory
  publishes a change. Each region remembers the last counter value it acted
  on. Counter unchanged + nothing local flagged ⇒ nothing new to react to ⇒
  skip.

That's the whole mechanism: "did anything change" flags read right before
the expensive work, instead of doing the expensive work unconditionally.

```text
 LAYER                     EXISTING (reused as-is)              ◆ NEW
 ─────                     ───────────────────────              ────
 World (one region's ECS)  invalidate_*/mark_* chokepoints       ◆ hints_dirty,
                              already fire on every relevant       power/jobs/goods_
                              mutation (attach/remove/upgrade)     exports_dirty —
                                                                    set INSIDE those
                                                                    same chokepoints
 RegionRuntime              TickState machine,                   ◆ seen_power/jobs/
 (event loop)                 reconcile_*_export_allocations        goods_generation;
                                                                    reconcile gate in
                                                                    front of each phase
 RegionWorker               per-region event slice,               ◆ hint publish
 (scheduler)                   road-report repricing gate            gated on hints_dirty
 RegionDirectory            publish idempotence check             ◆ generation: u64,
 (shared)                                                            bumped per publish
```

A mutation flips a flag → the worker's next pass notices → it publishes →
the generation bumps → every other region's next tick gate notices.

## One hour, before and after

```text
 TODAY (every region, every hour,             AFTER (same hour,
 whether or not anything changed)             nothing changed anywhere)
 ══════════════════════════════════          ══════════════════════════════
 power::run: clear ALL, reapply grants  ✗     power::run                 → no-op
   (drops valid imports — the bug source)       (imports simply survive)
 release + re-request ALL power         ✗     gate: clean → skip
 [daily] release + re-request ALL jobs   ✗     [daily] gate: clean → skip
 [daily] release + re-request ALL goods  ✗     [daily] gate: clean → skip
 per-pass hint recompute + publish       ✗     hints_dirty clean → skipped
 [daily/weekly] happiness, population,         unchanged — these were
   jobs, economy, business growth               always meant to run daily

 ✗ = unconditional work this plan gates       "the tick just progresses time"
```

On an hour where something *did* change, the right column runs exactly
today's logic — nothing new is invented, the plan only earns the right to
skip the clean path.

## The mechanism, in code

**1. Flip the flag inside code that already runs** — no new call sites:

```rust
pub(crate) fn invalidate_resource_registry(&self) {
    self.registry_cache.borrow_mut().invalidate_all();
    self.hints_dirty.set(true);          // NEW
    self.power_exports_dirty.set(true);  // NEW
}
```

**2. Gate the reconcile on the flag (or the generation), decided *before*
collecting demand.** This order matters once step 3 (below) lands —
collect first, and a kept import looks "already fine" so it never gets
re-requested, even though its producer-side reservation is about to be
torn down underneath it:

```text
 WRONG — collect, then gate:              RIGHT — gate, then collect:
 scan demand: import already powered,     dirty? no → skip entirely
   so it's NOT in the list                dirty? yes → clear imports first,
 dirty → release ALL, re-request            THEN scan demand — now the
   only what was listed                     cleared ones show up correctly
 kept import: grant held,                 release + re-request covers
   reservation GONE — unbacked ✗            every import, in sync ✓
```

```rust
fn start_tick_power_phase(&mut self, request_id: UiRequestId) -> Vec<OutboundMessage> {
    let dirty = self.state.is_power_exports_dirty()
        || self.discovery_generation > self.seen_power_generation;

    if !dirty {
        // quiet: time still advances, power::run still runs (now a no-op),
        // but no demand scan, no release, no request.
        let phase = self.state.begin_tick_power_phase_quiet();
        return self.enter_job_phase(request_id, phase);
    }

    self.seen_power_generation = self.discovery_generation;
    self.state.clear_power_exports_dirty();
    let phase = self.state.begin_tick_power_demand_phase(); // today's code, unchanged
    self.reconcile_power_export_allocations(request_id, phase)
}
```

Jobs and goods gate `enter_job_phase` / `enter_goods_phase` the identical
way, each with its own flag and its own `seen_*_generation` — **not
shared**, because the three gates run at different cadences (power hourly,
jobs/goods daily). A shared marker would let the hourly power gate "use up"
a generation bump before the daily job gate ever sees it, silently
swallowing a change meant for jobs.

**3. Stop wiping power state every tick — diff-apply instead of
clear-then-rebuild:**

```rust
for (entity, consumer) in world.power_consumers.iter_mut() {
    match local_grant_for(entity) {
        Some(grant) => consumer.set_powered(Local(grant.source)),   // fresh local grant wins
        None if consumer.is_imported() => { /* keep it — do nothing */ }
        None => consumer.set_unpowered(),
    }
}
```

That third piece is *why* the quiet path can be a true no-op: nothing ever
clears a valid import out from under itself anymore.

## Two records, one deal

A cross-region import is really *two* records in *two* regions: the
consumer's **grant** (its own flag saying "I'm powered, from region C") and
the producer's **ledger** entry (the reservation stopping it from
double-selling that capacity). The rule this whole area lives by: **grant
held ⇔ reservation held**, never one without the other.

```text
 TODAY: torn down and rebuilt EVERY hour — clear grant → release ledger →
        re-request → re-grant — 24 rebuilds/day of the same arrangement,
        each mid-rebuild moment a transient window (where past bugs lived).
 AFTER, quiet tick: no clear, no release, no request — both records simply
        persist, still matching.
 AFTER, dirty tick: today's full teardown + rebuild, both sides together.
```

## Watching a plant get built

```text
 Build(PowerPlant)
   │ attach_power_provider → invalidate_resource_registry()
   │   (the existing "power changed" signal) → also flips hints_dirty
   ▼
 next worker pass: derived state recomputes → consumers powered
   │ power state changed → invalidate_jobs_registry()  (already existed:
   │   "a factory has power" → jobs notices, for free)
   ▼
 hints_dirty → worker recomputes + publishes → directory generation += 1
   ▼                                        (one-tick-stale from here on)
 every region, at its next tick:
   gate: local change? OR my last-seen generation < current?
     YES → reconcile (today's release+request logic, now gated)
     NO  → skip, touch nothing
   ▼
 unemployed citizens elsewhere get routed to the newly powered producer —
 same request/grant machinery as always, just no longer running blind.
```

## Different buildings, different reach

```text
 kind          what it fires on add                 cross-region reach
 ───────────   ───────────────────────────────       ─────────────────────
 PowerPlant    registry invalidation + hints          capacity hint ↑, importers notice
 Commercial    registry invalidation + hints          +job slots, +goods demand
 Industrial    registry invalidation + hints          +job slots, +goods supply
 Residential   population + power registered          two-stage (see below)
 Road          topology dirty + route cache clear      widest: power pooling, workplace
                + registry invalidation                 eligibility, routing, goods routes
 Park          happiness effect ONLY                   NONE — proves the gates are
                                                         correct, not just fast: zero
                                                         chokepoints touched, zero traffic
 remove (any)  always registry invalidation             + road → topology dirty;
                                                         + residential → citizens removed
```

Every row funnels through the same handful of existing chokepoint
functions — the new flags live *inside* those functions, so nothing needed a
new call site.

## The subtle parts

**Houses are two-stage.** Building a house is event-driven immediately (its
power demand counts right away); *people moving in* is not, and stays that
way on purpose:

```text
 stage 1 — CONFIG (event-driven)          stage 2 — PEOPLE (time-driven, unchanged)
 Build(Residential)                       next daily tick: population growth
   → power demand counted, no residents     → citizens spawn → THEY are the
     yet, nothing to hire out                 job demand → next daily job phase
                                               requests remote slots if needed
```

Event-driven speeds up *propagation*, not demographics.

**Losing capacity must reach everyone who depends on it.** Gaining capacity
is easy — a new producer just gets discovered next pass. Losing it is the
direction that breaks naive designs: a consumer holding a grant from a plant
that just got bulldozed must find out, or it keeps a grant backed by
nothing. The generation counter closes this without any dedicated
"revoked!" message:

```text
 Region A (holds a grant)      Directory            Region C (plant bulldozed)
                                              bulldoze → registry invalidation
                                              + hints_dirty
                              gen 8 → 9  ◄─── C's shrunken hint publishes
 A's next tick:
 gate: 9 > seen(A)=8 → DIRTY
   release + re-request ──────────────────►  C's ledger cleared, then:
                                              remaining capacity = 0
   ◄────────────────────────────────────────  denied
 A: unpowered, stats corrected, jobs notice → A's OWN hint updates → gen 9→10
   → A's other dependents reconcile in turn, and the cascade dies out once
     nothing further changes.
```

Same latency as today (A finds out at its next tick either way) — the only
difference is the 999 hours where nothing was lost now cost nothing. Losing
a *job* slot converges the same way, just on the daily gate instead of
hourly.

**Why a counter instead of a "such-and-such changed" notification?** A
targeted revocation event would need new message types, ordering rules, and
producer-side bookkeeping of who to notify. The counter gets the same
result for free: *any* change anywhere bumps it; every region's next tick
notices and re-negotiates against the producer's *current*, authoritative
state. Coarser (one region's change wakes every region's next check) but
free in the steady state, and no worse than today's "recheck everyone,
every hour" in the worst case.

**A quiet city really does cost nothing.** Three regions, five hours. Only
region B changes anything (builds a road, hour 3):

```text
 hour            1        2        3          4        5
 directory gen    7        7        7 ──► 8    8        8

 region A        skip     skip     skip       DIRTY      skip
 (seen=7)                                     (gen moved, reconciles once)
 region B        skip     skip     DIRTY      skip       skip
 (the builder)                     (local change fires)
 region C        skip     skip     skip       DIRTY      skip
 (seen=7)                                     (one tick behind A, as designed)

 TODAY: all 15 cells above are a full release+request cycle.
 AFTER: 3 reconciles total, 12 skips — everyone consistent by hour 4.
```

## What never becomes event-driven (on purpose)

Time itself, and everything that's *supposed* to move on a calendar:
daily happiness decay, daily population growth, daily job assignment and
economy settlement, weekly business growth. These are gameplay pacing, not
tick debt — a citizen changing jobs at most once a day is a feature
(prevents thrashing), not a limitation this plan should remove.

## What shipped

```text
  4684c9e  P-1  hints_dirty + gated cross-region hint publishing
  3add126  P-2  discovery generation + power reconcile gate
  ba00d6d  P-3  diff-apply power::run, delete the starvation-fix workaround
  83ebc98  P-4  job reconcile gate (daily cadence kept)
  4f6a7ad  P-5  goods reconcile gate
  ed9f6a0  P-6  slim the tick, absorb the two old TODOs, documentation
```

A quiet tick now costs: time advance, turn increment, and the always-daily
systems above. Zero export traffic, zero hint recomputes, zero power-state
churn, for any resource with nothing to reconcile.

## Six things the plan didn't see coming

Each caught and fixed *during* implementation, not assumed correct up
front:

1. **P-1** — a first-draft regression test can pass for the wrong reason.
   Caught by deliberately breaking the fix and confirming the "regression"
   test still (wrongly) passed.
2. **P-2** — deciding "should I reconcile?" *after* peeking at demand,
   instead of before, would hide an import from its own renewal request —
   reintroducing an old starvation bug in reverse. Order matters: decide
   first, look at demand second.
3. **P-3** — the power-stats calculation quietly stopped counting kept
   imports as "supplied" once the old clear-and-rebuild dance was removed
   (a real UI-visible bug, caught in review). Also: splitting the "clear"
   step out of `power::run` broke a side-effect that used to invalidate the
   jobs cache for free — had to make that explicit.
4. **P-4** — a region with even one worker holding a job in another region
   can *never* go quiet: job assignments are unconditionally wiped and
   rebuilt every single day by an older, unrelated mechanism, so there's
   always something to re-negotiate. Not a bug — a real, permanent limit on
   where the savings reach.
5. **P-5** — naively copying P-1's "mark this dirty every day, no matter
   what" pattern to goods would have made the entire patch a no-op: that
   marking site fires for every region whether or not it has any
   commercial/industrial building at all. Caught *before* writing it, by
   asking "does this fire regardless of the thing I'm gating on?"
6. **P-6** — the obvious way to "skip power::run when nothing changed"
   (check the flag right there) doesn't work: something upstream already
   clears the flag before that check would run, so it would silently read
   "clean" forever, even on ticks that need the real work. Fixed by having
   the gate decide once and hand down the answer, instead of re-checking a
   flag whose lifetime doesn't reach that deep.

Every one of these was caught by tracing the actual code path before
trusting a shorthand description of it — including this plan's own.

## Not part of this plan: retiring the tick-state machine

`TickState` is this codebase's async/await: a tick that needs a remote
grant doesn't run start-to-finish in one call — it pauses, lets other
regions' requests get answered in the meantime (required, or two regions
waiting on each other would deadlock), then resumes when the grant arrives.
This plan makes that pause-and-resume dance *rare* (only on a change tick)
but doesn't remove it — economy settlement genuinely needs to see the
day's final grants before it pays anyone, so *something* has to wait for
them on a dirty day.

A future plan could go further: stop waiting at all, settle each tick with
whatever grants are already held, and let fresh requests/grants apply
starting *next* tick instead of this one. That deletes the pause machinery
entirely, but it means a region's *own* changes become one tick stale too
(today they're not — only cross-region reads are). That's a real, felt
gameplay change (a grant now pays out tomorrow, not today) and deserves its
own plan and its own sign-off — not bundled into this one.
