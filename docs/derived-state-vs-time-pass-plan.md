# Derived State vs Time Pass Plan

This plan splits the simulation `tick` into two distinct concepts:

- a **derived pass** -- a pure function of the current world configuration, recomputed on
  change (not on time), and therefore visible **while the game is paused**, and
- a **time pass** -- the accumulators that genuinely advance with the passage of time, and
  only run on a tick.

It generalizes what R5 already did for power and jobs (cache the resolution, recompute on
mutation, not every tick) into an explicit model boundary, and extends it to happiness and
the UI. It is a **local** simulation-structure change, largely independent of the
cross-region model, but it composes with
[regional-snapshot-consistent-plan.md](regional-snapshot-consistent-plan.md) (see
"Composition" below).

> **This plan owns the canonical local step model** -- the `derived pass -> time pass`
> structure of one simulation step. Other plans that restructure the step define their
> changes *on top of* this boundary rather than redefining it: the snapshot plan's OB3
> only swaps the derived pass's cross-region input to the frozen snapshot and adds the
> publish at the step boundary; it does not re-collapse the tick. For that reason this
> split is recommended to land **first**, as the foundation (see "Composition").

## The model

> **Derived state** is the instantaneous truth: what is powered, who has a job, the
> happiness conditions, pollution, road connectivity, derived stats. It is a pure function
> of the buildings/roads/citizens present right now. Change the config -- even while paused
> -- and it updates immediately.
>
> **Time integration** is what an instant cannot tell you: money earned from salary and
> tax, citizens aging, population growing, businesses reinvesting, a position travelling
> from A to B, accumulated stress easing. These advance only when time advances (a tick),
> and stay frozen while paused.

The invariant tying them together: **derived-before-time, every step.** The time pass
reads the derived state as its input (economy reads the job assignment; population growth
reads happiness and jobs), so derived recompute always precedes the time pass within a
step. The current tick order already approximates this; the split makes it a rule.

```text
  config change (build/bulldoze/upgrade) ----> invalidate derived cache
                                                       |
        (paused OR running)                            v
                                              DERIVED PASS  (pure fn of config)
                                              power, jobs, roads, pollution,
                                              local effects, stats,
                                              happiness TARGET
                                                       |
        (running only -- skipped while paused)         v
                                              TIME PASS  (accumulators)
                                              money (salary/tax/income),
                                              population growth, aging,
                                              business reinvestment,
                                              happiness ACTUAL (eases to target),
                                              travel position
```

## Categorization of the current systems

| Current system | Category | Notes |
|---|---|---|
| `power::run` | Derived | already R5-cached |
| `road_network_analysis::run` / road connectivity | Derived | `RoadNetworkAnalysis` is already derived |
| `local_effects::run` | Derived | amenity/effect field |
| `pollution::run` | Derived | sources -> field, instantaneous |
| `stats::run` | Derived | aggregate counts (unemployment, totals) |
| job availability + assignment (`economy::assign_local_jobs`, R5 job resolution) | Derived | matching is a pure function of housing + slots |
| happiness **target** (the conditions part of `citizens::citizen_happiness`, `happiness::run`) | Derived | pure function of jobs/power/amenities/pollution |
| `economy::run` money (salary, tax, business income) | Time | the accumulator half of today's `economy::run` |
| `population::run` | Time | growth/births over time; reads derived happiness/jobs |
| `business_growth::run` | Time | reinvestment/auto-upgrade accumulates over time |
| citizen aging, `citizens::apply_daily_happiness_decay`, `rent_stress` | Time | per-tick accumulators |
| happiness **actual** (`citizens::update_happiness` applying decay/stress) | Time | eases toward the derived target |
| travel position A->B | Time | *does not exist yet* -- forward-looking; citizens currently hold no grid position |

Two of today's systems are **hybrid** and must be split: `economy::run` (derived job
assignment + time-driven money) and happiness (derived target + time-driven actual).

## Happiness: H2 (target derived, actual time-relaxed)

Decision: **H2** -- keep the inertia, expose the target.

Today `citizen_happiness()` already computes `f(instantaneous conditions) - happiness_decay
- ...`, and `happiness_decay` / `rent_stress` are per-tick accumulators on `Citizen`. H2
splits this cleanly along the line that already exists:

- **`happiness_target` (derived):** the `f(conditions)` part -- employment, powered home,
  nearby amenities, pollution. Recomputed on config change; **visible while paused**.
- **accumulators (time):** `happiness_decay`, `rent_stress` advance only on a tick.
- **`happiness` (actual, time):** eases toward `happiness_target` net of the accumulators,
  exactly as today.

This is **behavior-preserving for the running sim** (same per-tick happiness values, because
the formula is unchanged) and **additive for the paused view** (you can now see the target
move when you build a park or restore power without advancing time). No balance change.

## Pause semantics

- **Paused + a command (build/bulldoze/upgrade):** invalidates the derived cache; the next
  view read recomputes the derived pass, so power, jobs, happiness target, pollution, and
  stats all update immediately. The time pass does **not** run, so money, population, age,
  and actual happiness do not move.
- **Running (a tick):** derived recompute (if dirty) then the time pass; same as today plus
  the explicit boundary.

This is exactly the requested behavior: "in pause mode we can see power/job/happiness change
without a tick; economy does not change while paused."

## Composition with the snapshot-consistent plan

The two plans are consistent and reinforce each other:

- **Local** derived state recomputes immediately from current config (this plan).
- **Cross-region** derived state (imported power/jobs) is also derived, but its input is the
  *previous step's frozen snapshot* (the snapshot plan), so it carries the one-tick lag and
  does not change while paused (no step -> no new snapshot).
- A **step** is then: derived recompute (local config + frozen cross-region snapshot) ->
  time pass -> publish the new snapshot. The snapshot plan's OB3 ("straight-through tick")
  and this plan's "time pass" describe the same collapsed step from two angles: OB3 says the
  tick no longer pauses for grants; this plan says *what is left in the tick* (only the time
  accumulators).

Recommended sequencing: do this derived/time split **first** as a local, mostly
behavior-preserving foundation -- it clarifies what the tick contains and makes OB3 a thin
change (swap the cross-region input source into the already-isolated derived pass). The two
plans both touch `simulation.rs`, so whichever lands second must respect the other. They are
**separate missions** (one at a time).

## Staged patches

Each patch is independently shippable, small, and gated. Most are behavior-preserving; the
only observable change is *additive* paused visibility.

### Patch DT1: Formalize the derived pass for the already-derived systems

Make the derived/time boundary explicit for the systems that are already pure functions of
config (power, roads, local effects, pollution, stats, job matching). Ensure the time path
reads them through the cache rather than recomputing, and ensure `RegionState::view()`
recomputes-on-read when the derived cache is dirty so a **paused** command updates the view.

- **No behavior change** (running): the cache yields identical values.
- New behavior (additive): a paused build/bulldoze updates power/jobs/stats in the view.

Tests: parity -- per-tick outputs identical to today; new -- paused command updates the
powered/jobs view with no tick.

### Patch DT2: Happiness H2 split

Extract `happiness_target` (derived, from conditions) into the derived layer; keep
`happiness_decay` / `rent_stress` accumulation and the actual-happiness relaxation in the
time pass; expose `happiness_target` in the view.

- **No behavior change** (running): actual happiness per tick is unchanged.
- New behavior (additive): the target updates in a paused view.

Tests: parity on actual happiness over a scripted run; new -- paused amenity/power change
moves `happiness_target` without a tick.

### Patch DT3: Split economy into derived assignment vs time-driven money

Separate `economy::run`: the job-assignment/matching becomes part of the derived pass
(visible while paused -- "job apply" updates immediately), while salary, tax, and business
income stay in the time pass (frozen while paused).

- **No behavior change** (running): money and assignments per tick unchanged.
- New behavior (additive): a paused workplace build updates job assignments in the view.

Tests: parity on money and assignments; new -- paused workplace build updates the job
assignment view without a tick.

### Patch DT4: Purify the time pass and document the dependency DAG

Audit the remaining systems (`population`, `business_growth`, aging) to confirm they only
**read** derived state and only **advance** accumulators, and write down the derived->time
dependency graph. Resolve any case where a time output feeds a derived input within the same
step (it becomes a deliberate one-step lag, consistent with the snapshot philosophy, or is
reordered).

- **No behavior change**: this is an audit + reorder/cleanup, gated by the existing parity.

Tests: full parity over a long scripted run; a test asserting the time pass is a no-op while
paused (money/population/age unchanged across a paused command).

### Patch DT5 (future): Travel as a time-driven position system

When positional travel is added (citizens/vehicles moving A->B over time), it lands as a
pure time-pass system advancing a position each tick. Placeholder only -- no positional
travel exists today.

## Determinism and gates

- The derived pass is a **pure function** of config (+ the frozen cross-region snapshot
  under the snapshot plan); deterministic by construction.
- The time pass is deterministic and reads the derived pass; the **derived-before-time**
  order is fixed.
- Every patch is gated by a **parity guard** against today's behavior for the running sim
  (the split must not change tick-by-tick results); the paused-visibility behavior is the
  only intended new observable, tested explicitly.

## Open questions / audit items

- **Dependency DAG (DT4):** confirm derived->time is acyclic. Any time->derived edge within
  a step is a deliberate one-step lag; enumerate them.
- **Where `update_happiness` / `apply_daily_happiness_decay` run today** must be traced so
  DT2 moves only the accumulator half into the time pass.
- **Stats happiness vs per-citizen happiness:** `happiness::run` derives city-level
  `stats.happiness` from per-citizen values; confirm the derived/time split is applied at
  the per-citizen layer and the city stat stays derived.

## Non-goals

- No balance change. H2 preserves happiness inertia; all parity guards hold for the running
  sim.
- No new cross-region mechanism -- that is the snapshot plan's job; this plan only fixes
  what is derived vs time-integrated locally.
- No travel implementation (DT5 is a placeholder).
