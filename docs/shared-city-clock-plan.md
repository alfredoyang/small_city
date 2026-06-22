# Same City Clock Across Regions — Plan

## Problem

Regions belong to one city, yet each shows its **own** time. Switch the selected
region and the clock jumps to a different hour.

```text
 Region A (viewed, ticked 50x)        Region B (never selected)
   ┌ Time: Day 3, 02:00 ┐               ┌ Time: Day 1, 00:00 ┐
   └────────────────────┘               └────────────────────┘
            ▲ same city, two different clocks — wrong
```

## Diagnosis

Two compounding facts; the second is the real culprit.

1. **`GameTime` is a per-region resource that self-increments.** Each region's
   `World` owns `resources.time`, and the tick bumps it:
   ```text
   src/core/simulation.rs:97   world.resources.time.advance_hours(1);  // inside EACH region's tick
   src/core/resources.rs:15    pub time: GameTime,                     // stored per-World (per region)
   ```

2. **The UI ticks only the selected region — nothing else advances.**
   ```text
   TUI/ascii auto-tick & manual tick
     └ CityDriver::tick()                       (city_driver.rs:267)
         └ RegionalGame::tick_selected_region() (regional_game.rs:520)
             └ tick_region(selected)            → one Tick event to that region only
   ```
   `tick_all_regions` exists but **no UI path calls it** (only tests). Nothing ticks
   regions in the background — a region advances only when it receives a `Tick`,
   and the runner sends one only to the region you explicitly tick
   (`regional_game_runner.rs:232`).

So while you watch and tick region A, region B is **frozen** — its time, power,
citizens, economy, and growth all stand still until you select B and tick it.

> Nuance: when A ticks, cross-region **export** events (power/jobs/goods
> request→grant→release) are still routed into B's mailbox and processed in the same
> pass, so B can supply spare capacity to A. But those are coordination events, not
> `Tick`s — they never advance B's clock or run B's daily/weekly economy.

The clock was a single-city/single-`World` concept carried verbatim into every
region's `World` when the engine split into regions; the regions layer added
cross-region sharing but never made ticking a city-wide action.

## Primary fix (minimal): tick the whole city

Point the UI tick at **all** regions instead of the selected one.

```text
CityDriver::tick()
  - tick_selected_region()   // ticks only the region you're viewing
  + tick_all_regions()       // ticks every region, one hour each
```

**Why this fixes it:** every region's clock starts at 0 (`GameTime::default`) and
advances `+1h` per tick. If the only tick path always advances *all* regions, the N
independent counters stay numerically identical forever — *and* every district
actually simulates, so nothing is frozen.

```text
UI tick ─► RegionalGame.tick_all_regions()
              ├─ Tick ─► region A: A.time += 1h ; run A
              ├─ Tick ─► region B: B.time += 1h ; run B
              └─ ...                              (all advance one hour together)
display(any region).time == every other region's time
```

### Details to handle (not blockers)

- **Return value / status line.** `tick_all_regions()` returns `Result<(), _>`, but
  `CityDriver::tick()` returns a `CommandResult` for the status message. Tick all,
  then surface one message — simplest: return the **selected** region's result for
  the line after ticking everyone.
- **9-region cost.** `CityDriver` always builds the 3×3 (9-region) default, so today
  the player ticks 1 of 9; after this all 9 tick each step. Empty regions are cheap
  and have no economy effect, but a few tests assert exact money after N ticks — run
  the suite to confirm none regress.

### The one honest caveat

With tick-all the N clocks are still N separate counters that *stay equal because
they're always advanced together*. They could only drift if some code ticks a single
region alone (today: only tests) or a tick partially fails. That's acceptable: the
observed bug is fixed and the invariant ("always ticked together") is easy to keep.

## Optional hardening (deferred — YAGNI): one shared city clock

Only worth doing if we later (a) want to tick **only active regions** for
performance, or (b) want it to be *structurally impossible* for the clocks to drift.
Then make time a single city-owned value instead of N counters:

- `RegionalGameRunner` owns `city_clock: GameTime`, advanced once per city step.
- `RegionEvent::Tick` carries the target `hour`; the runtime sets
  `world.resources.time = hour` and `begin_tick_power_phase` stops self-incrementing
  (day/week detection uses `before = world.resources.time` → `hour`).
- Save envelope (`RegionalGameSave`) gains `city_clock` (`#[serde(default)]`); load
  sets every region from it; old saves migrate from `max(total_hours)`.

This is the more correct design (single source of truth), but it is **not needed**
to fix the reported problem and is intentionally left for later.

## Mission (one small patch through the dev loop)

- `RegionalGame` / `CityDriver`: make `tick()` advance every region (tick-all),
  returning a status message (selected region's result).
- Tests: one `tick()` advances **every** region's clock by exactly one hour;
  switching the selected region afterward shows identical time; a built region's
  economy still behaves (no money regression from neighbors ticking).

## Risks / notes

- **Determinism:** preserved — every region still runs the same deterministic tick;
  we only change *how many* run per UI step.
- **Cross-region staleness:** untouched — ticking all regions per step is the normal
  per-region tick repeated; the one-tick-stale export model is unchanged.
- **Balance:** the whole city now grows each tick (every district), not just the
  viewed one — watch economy/pollution/demand pacing after the change.
