# 20260628 — Building the L1 route map: cost, locking, and the chosen approach

Status: **decision record** (no code). Scopes how the cross-region Layer-1 route map
(`region_routes`, see `docs/20260627-multi-region-return-home.md`) is built and published
in the directory without stalling routing. Pairs with that plan.

## 1. Problem

The L1 route map (`RegionRoutes`) is **not a simple data merge** like the directory's
other published state — it is a **Dijkstra-per-destination** computation. Today the
directory builds the whole `CrossRegionDiscovery` snapshot inside `rebuild_discovery`
*while holding the write lock* (`publish_state`):

```text
directory.rs:
  publish_region(region, …)                      // any worker thread, on its region change
    lock publish_state ───────────────────────┐  // WRITE lock
      idempotent skip if unchanged (l.150)     │
      update this region's input entry         │
      rebuild_discovery(&state):               │  ← runs UNDER publish_state
        build CrossRegionDiscovery {           │
          components = build_component_graph()  │  // today: a union pass
          (+ region_routes = Dijkstra×T)        │  // NEW: heavier
        }                                      │
        lock active_snapshot: swap Arc ──┐     │  // SWAP only (cheap)
        unlock active_snapshot ──────────┘     │
    unlock publish_state ──────────────────────┘
```

Adding a Dijkstra to a section that is held under the write lock raises a fair worry:
a heavy rebuild holds `publish_state` and **serializes other publishers**. (Readers are
unaffected — see §2.)

## 2. What's actually true (grounding `directory.rs`)

```text
  RegionDirectory
    publish_state:   Mutex<DirectoryPublishState>   // WRITE — inputs + the rebuild
    active_snapshot: Mutex<Arc<CrossRegionDiscovery>>// SWAP — held only to store the new Arc

  WRITE path (publish_region → rebuild_discovery)      READ path (discovery_snapshot)
  ──────────────────────────────────────────────      ───────────────────────────────
  worker publishes → build new snapshot UNDER          routing reads Arc::clone(active)
  publish_state → swap active_snapshot (brief)         under active_snapshot (brief), then
                                                       releases — NEVER blocks on a build
```

Three facts that bound the worry:

1. **Readers never block.** `discovery_snapshot()` only `Arc::clone`s `active_snapshot`;
   the build happens off that lock. Token routing (every sub-tick) is never stalled by a
   rebuild. ✅ This is the property we most need, and it already holds.
2. **The heavy Dijkstra is NOT in the directory.** The expensive *road-cell* Dijkstra is
   the per-region `road_report()` — computed in each region's own `World` (share-nothing,
   **off the directory lock entirely**) and published as a `RegionRoadReport`. The
   directory runs only the small **region-level** Dijkstra (nodes = `(region, border-link)`,
   a handful per region) → ~O(R²·log R) for R regions. Bounded by region count, not city
   size; sub-millisecond for realistic R.
3. **It's rare, not per-tick.** `publish_region` is idempotent (`directory.rs:150`): if a
   region's normalized inputs are unchanged, no rebuild. The road graph changes only when
   a road/region is built/bulldozed — so the rebuild is the `resource_registry`
   "cache, invalidate at a chokepoint" pattern, amortized; sub-ticks read the cached
   snapshot for free.

So the only residual cost is: **a rebuild holds `publish_state`, serializing concurrent
publishers** (not readers).

## 3. Why the write lock spans the build (the lost-update race)

Holding `publish_state` across update→build→swap is **protecting correctness**, not just
convenience. Naively shrinking the lock to "just the swap" reintroduces a lost update
between concurrent publishers:

```text
  state = {A:a0, B:b0}
  Worker-A: lock→ state={A:a1,B:b0}, clone₁ →unlock.  building snap₁ from clone₁ … (slow)
  Worker-B: lock→ state={A:a1,B:b1}, clone₂ →unlock.  building snap₂ … (fast)
  Worker-B: swap → active = snap₂          ✅ has a1, b1
  Worker-A: swap → active = snap₁          ❌ has a1, b0 — B's update is GONE
```

Under today's design this can't happen: the whole update→build→swap is serialized, so the
last swap always reflects every prior update.

## 4. Options

```text
  V1 (CHOSEN): build under publish_state                  correct · simplest · small+rare
    update + build + swap all under the write lock.
    Cost: a rebuild blocks other PUBLISHERS (not readers) for the region-level Dijkstra.

  (a) version / CAS swap                                   off-lock build, no lost update
    lock publish_state (SHORT): update; gen += 1; clone {inputs, gen}      → unlock
    build region_routes from the clone                    (UNLOCKED — the Dijkstra)
    lock active_snapshot (SHORT): if gen > installed { swap; installed = gen }  → unlock
    A stale (older-gen) build is discarded; a newer publish already superseded it.

  (b) single rebuild owner                                cleanest for a heavy/dirty build
    publish_region: update input + set dirty, under a SHORT lock. No build here.
    ONE owner (the worker scheduler's bounded pass) rebuilds when dirty, off-lock, swaps.
    One builder ⇒ no concurrent-build race, no version guard, publishers never block.

  REJECTED: naive "lock only the swap"                    → lost update (§3)
```

## 5. Decision

- **v1: keep the build under `publish_state`** (today's design, extended with
  `region_routes`). It is correct, simplest, and the costly part (road-cell Dijkstra) is
  already per-region and off-lock; the directory's region-level Dijkstra is small and only
  runs on a road-graph change.
- **Do NOT** shrink to a naive lock-only-swap — it loses updates.
- **Escape hatch, if profiling shows publisher contention** (region count grows large, or
  the rebuild measurably stalls publishes): adopt **(b) single rebuild owner** — it takes
  the rebuild off the publish hot path entirely and needs no version guard. **(a)** is the
  lighter-touch alternative if a dedicated owner is awkward.

### Triggers to revisit
- A publish-path profile shows `publish_state` held long enough to stall other regions'
  publishes.
- Region count (and thus the region-level graph) grows beyond a few dozen.
- The rebuild starts running more than once per road change (regression in the idempotent
  skip).

## 6. Non-goals
- No change to the reader path — `discovery_snapshot()`/`Arc` swap stays as-is.
- No new worker command for v1 (the rebuild rides the existing publish path); option (b)
  would route it through the existing scheduler pass, still no new command.
- This doc decides *how to build/publish* `region_routes`; *what it contains* and *how
  tokens route on it* live in `docs/20260627-multi-region-return-home.md`.
