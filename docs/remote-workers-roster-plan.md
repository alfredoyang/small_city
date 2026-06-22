# Remote Workers in the Workplace Roster — Plan

## Goal

A workplace roster (Commercial/Industrial) currently lists **local workers only**
— citizens who live in the same region. But a workplace can be staffed by
**remote** workers who commute in from another region (cross-region job export).
Those workers fill real jobs yet appear nowhere on the workplace's own roster, so
a busy industrial can show *"No local workers"* while every slot is taken.

This plan adds remote workers to the workplace roster.

```text
 Industrial @ region 4, cell (2,6) — 12 jobs, all filled by region-1 commuters
   ┌ Workers at (2,6) ─ 12 worker(s) ──────────────┐
   │ #   Age  Happy  $    Lives at                  │
   │ #1  31   70    $9    region 1 (4,11)           │  ← remote (NEW)
   │ #2  27   64    $7    region 1 (4,12)           │  ← remote (NEW)
   │ …                                              │
   └ local + remote ───────────────────────────────┘
```

## Why this is possible (and why it isn't a producer-side read)

The cross-region job link is recorded on the **consumer** citizen, not on the
producer building. When a region-1 resident takes a remote job at region-4's
industrial, the daily job-export phase writes onto that **region-1 citizen**:

```rust
Citizen.workplace_assignment = Some(WorkplaceAssignment {
    region:   RegionId(4),          // ← the producer region
    position: Position { x: 2, y: 6 },  // ← the producer cell
    salary,
    source:   WorkplaceSource::Remote { slot_id },
});
```

So the worker's identity **and full attributes** (`age`, `morale.actual`,
`money`, `home`) live in region 1's `World`, tagged with exactly which
`(region, position)` they commute to. The producer (region 4) holds only an
opaque slot reservation in the runtime export ledger — a *count*, not a *who*.

Therefore the only way to enumerate remote workers is a **reverse lookup into
the regions where the workers live**, keyed on `(producer_region, position)`:

```text
local workers (today):
   workplace @ region R, entity E
   = citizens in R where source == Local{ E }

remote workers (this plan):
   workplace @ region R, position P
   = citizens in ANY OTHER region where
       assignment.region == R && assignment.position == P && source == Remote{..}
```

The match key is `(region, position)` — both stored on the consumer citizen — so
no shared entity id is needed across regions.

## Architecture: a lighter cross-region read

`inspect_region` is a **synchronous direct read** under the runner's
`operation_lock` (`regional_game_runner.rs` → `worker.region_mut(id)`), **not** a
mailbox event. The new query rides that same synchronous path, so it needs **no
new tick event, no `TickState`/`RegionEvent` change, and no `UiRequest`/`UiReply`
variant** (those are only the async snapshot path).

The "lighter query" decision (vs a full `inspect_region` fan-out per frame):

- **Frequency:** fetch only when the workplace panel is **opened** (and re-fetch
  on tick while it stays open), never per cursor-move / per frame.
- **Payload:** a dedicated `remote_workers_at(region, pos) -> Vec<CitizenDetailView>`
  that computes only the roster slice — not the whole `InspectView`
  (cell/details/effects/flags/explanations).
- **Per-call work:** a plain citizen scan in M1/M2; add an
  `(target_region, target_pos) -> [worker]` reverse index only if profiling
  demands it (ponytail: skip the index until then).

```text
UI open workplace panel
  └─ RegionalGame.remote_workers_at(producer_region, pos)            [facade]
       └─ RegionalGameRunner.remote_workers_at(...)                  [lock, fan out]
            └─ each ThreadedRegionWorker: "remote_workers_at" command [thread boundary]
                 └─ RegionWorker iterates its regions (skip producer) [worker]
                      └─ RegionRuntime.remote_workers_for(prod, pos)  [ensure derived, read]
                           └─ RegionState: citizens where
                                assignment.region==prod && position==pos && Remote
                                → project to CitizenDetailView         [core + reuse adapter]
  └─ merge into the workplace roster, render (region-tagged)         [UI]
```

## Layered scope

`(+)` = added/changed. The change is read-only and spans **all five layers**, but
touches **no** simulation, balance, tick, or export-protocol code.

```text
┌─ src/core  (SIMULATION) ───────────────────────────────────────────────────┐
│  Citizen.workplace_assignment already carries { region, position, Remote }  │
│  (+) RegionState::remote_workers_for(region, pos) -> Vec<CitizenDetailView> │
│      filter citizens (assignment.region==R && position==P && Remote), project│
│  (+) RegionRuntime accessor: ensure derived state, then read (mirrors inspect)│
└───────────────────────────────────────────────┬───────────────────────────┘
                                                  │
┌─ src/interface  (ECS→VIEW BOUNDARY) ────────────▼───────────────────────────┐
│  (+) CitizenRelation::LivesAt gains `region: RegionId` (a remote worker's    │
│      (x,y) is in another region — ambiguous without it)                     │
│  (+) adapter projection reused for a remote worker (home in its own region)  │
└───────────────────────────────────────────────┬───────────────────────────┘
                                                  │  synchronous read path
┌─ src/core/regions  (THREADING) ─────────────────▼───────────────────────────┐
│  (+) RegionWorker::remote_workers_at(prod, pos): iterate owned regions       │
│  (+) ThreadedRegionWorker command (mirrors inspect_region)                   │
└───────────────────────────────────────────────┬───────────────────────────┘
                                                  │
┌─ src/core/regional_game{,_runner}.rs  (FACADE) ─▼───────────────────────────┐
│  (+) Runner.remote_workers_at: lock, fan out to ALL workers, merge          │
│  (+) RegionalGame.remote_workers_at(region, pos) -> Vec<CitizenDetailView>  │
└───────────────────────────────────────────────┬───────────────────────────┘
                                                  │
┌─ src/ui/tui.rs  (FRONTEND) ─────────────────────▼───────────────────────────┐
│  (+) on panel open: fetch + cache remote workers in TuiState (not per frame) │
│  (+) render them under the local workers, region-tagged                      │
└────────────────────────────────────────────────────────────────────────────┘
```

## Mission boundary

~9 files total — past the 5-file / ~400-line single-patch cap — so split into
three one-patch missions, each green through the dev loop before the next.

### M1 — core read + view model (core/interface)

- `view.rs`: `CitizenRelation::LivesAt { x, y }` → `LivesAt { region, x, y }`.
  Update every existing roster test/literal (same churn as the original roster
  feature). For a *local* worker `region` is the inspected region; for a remote
  worker it is the worker's home region.
- `regions/mod.rs` (`RegionState`): `remote_workers_for(producer_region, pos)`
  — filter `citizens` by `assignment.region == producer_region && position == pos
  && matches!(source, Remote{..})`, project each to `CitizenDetailView` (reuse the
  adapter's citizen mapping), deterministic order by `Entity.0`.
- Tests: a hand-built two-region `World`/`RegionState` (no threading) — a citizen
  with a `Remote` assignment to `(R, P)` is returned for that key, local workers
  and non-matching cells are excluded, order is stable.

Size: ~2–3 files + tests. UI and threading untouched.

### M2 — cross-region fan-out (regions/facade)

- `regions/runtime/mod.rs`: `RegionRuntime::remote_workers_for(prod, pos)` —
  ensure derived state (like inspect), then call the M1 `RegionState` read.
- `worker.rs`: `RegionWorker::remote_workers_at(prod, pos)` — iterate
  `self.regions`, skip the producer region, concatenate matches.
- `threaded.rs`: new worker command + response (mirror `inspect_region`).
- `regional_game_runner.rs`: `remote_workers_at` — take `operation_lock`, fan out
  to **all** workers, merge (deterministic region/entity order).
- `regional_game.rs`: public `remote_workers_at(region, pos)`.
- Tests: a real multi-region game with a `city1`-style remote-commuter setup —
  residents in region A working remotely at region B's workplace are returned by
  `remote_workers_at(B, pos)`; producer-region citizens are not double-counted.

Size: ~5 files + tests. No UI.

### M3 — TUI render (ui)

- `tui.rs`: when the panel opens on a workplace, call `remote_workers_at`, cache
  the result in `TuiState` (so it is not refetched per frame; refresh on tick
  while open), and render remote workers under the local ones — region-tagged via
  the new `LivesAt { region, .. }`, e.g. `region 1 (4,11)`.
- The empty-state and footnote adjust: a workplace with only remote workers shows
  them instead of "No citizens yet"; the "(local workers only)" footnote is
  dropped or reworded now that remote workers are listed.
- Tests: render a workplace roster mixing a local and a remote worker; assert both
  appear and the remote one carries its home-region tag.

Size: 1 file + tests.

## Risks / notes

- **Cross-worker locking (medium):** `inspect_region` hits one worker; this hits
  all of them on panel open. Follow the existing command-channel pattern; bounded
  by "only on open."
- **View-model ripple (low-med):** adding `region` to `LivesAt` touches every
  existing roster test/literal.
- **Cost (low):** O(citizens in other regions at that cell), only on open. Add an
  `outbound_remote` reverse index only if profiling says so.
- **Determinism:** intact — a pure function of each region's state in a fixed
  (region, entity) order. Cross-region reads are **one-tick-stale**, consistent
  with the documented model (within-tick synchronicity is the only thing relaxed
  across regions).
- **No simulation/balance/tick/export-protocol change.** The producer export
  ledger still holds only a count; identities come from the consumer regions where
  the citizens already live.

## Open question (decide before M3)

How to present a mixed roster: a single table tagging each row local/remote, or
two sections ("Local" / "Commuters from other regions")? A single table with the
`Lives at` column showing `region N (x,y)` for remote workers is the lightest and
matches the existing column model. Confirm before M3.
