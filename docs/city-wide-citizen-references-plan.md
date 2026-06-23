# City-Wide Citizen References Plan

## Goal

Make durable citizen references (home and workplace) city-wide so cross-region
features can identify a citizen or a building without moving the whole `Citizen`
component or leaking region-local `Entity` ids into another `World`.

Current local-only shape:

```text
Citizen.home:           Entity
WorkplaceSource::Local: Entity
```

Those `Entity` values only mean something inside one region. A region receiving
one of them cannot resolve it safely.

Target shape:

```text
CitizenId      = stable city-wide person id
CityEntityRef  = { region, entity }       // internal city-wide ECS reference
CityCellRef    = { region, x, y }         // portable crossing/save/display reference

Citizen stays owned by its home region; only city-wide refs cross regions.
```

## Non-Goals

- Do not make `World` city-global.
- Do not let one region read another region's ECS.
- Do not move the durable `Citizen` component between regions.
- Do not change economy/job behavior while migrating ids.

## Architecture

```text
REGION A (owns its citizens)                  REGION B (owns its citizens)
──────────────────────────────               ──────────────────────────────
citizens[local_entity] = Citizen              citizens[local_entity] = Citizen
  id: CitizenId                                 id: CitizenId
  home: CityEntityRef(A, home)                  home: CityEntityRef(B, home)
  workplace: CityEntityRef(B, wkpl)             workplace: CityEntityRef(A, wkpl)

CitizenId / city-wide refs cross regions (in messages).
Region-local Entity refs do not.
```

Each region still stores and mutates only its own ECS. City-wide refs are handles
that include `RegionId`; a region may only dereference refs whose `region == self.id`.
Cross-region messages use `CitizenId`, `CityEntityRef`, and cell/link data
(`CityCellRef`); the receiver resolves a ref into a local building entity only when its
`region == self.id`, and otherwise just carries/echoes it.

## Types

Add small plain data types near the existing `Entity`/regional types:

```rust
// All three are plain Copy data. Ord/Hash give deterministic ordering for the CW1
// test and for use as map keys; Serialize/Deserialize for save records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CitizenId {
    pub home_region: RegionId,
    pub local: Entity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CityEntityRef {
    pub region: RegionId,
    pub entity: Entity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CityCellRef {
    pub region: RegionId,
    pub x: usize,
    pub y: usize,
}
```

`CitizenId` can initially be derived from `(RegionId, citizen_entity)` instead of
stored on `Citizen`. Store it later only if a call site needs it often enough to
justify the field.

## Patch CW1 — Add City-Wide Reference Types

Add `CitizenId`, `CityEntityRef`, and `CityCellRef`.

**Prerequisite:** the `Ord`/`PartialOrd` derives above require `Entity` to be
orderable, but on master `Entity` derives only `PartialEq, Eq, Hash`. Add
`PartialOrd, Ord` to `Entity` (it's a `u32` newtype, so the derive is trivial and
deterministic). Lazy and correct; avoids hand-written `(region.0, entity.0)` sorts at
every call site.

Add helpers:

```text
CityEntityRef::local(region, entity)
CityEntityRef::as_local(region) -> Option<Entity>
CityCellRef::local(region, x, y)
```

Tests:

- `as_local` returns the entity for matching region.
- `as_local` returns `None` for a different region.
- `CitizenId` ordering/equality is deterministic.

No behavior change.

## Patch CW2 — City-Wide Citizen Home

Change `Citizen.home` from `Entity` to `CityEntityRef`, then resolve in local systems
with `citizen.home.as_local(self_region)`.

### Decision (required first): how a bare `World` knows its own region

Most systems that read `home` only receive `&World`, never a `RegionId` (job
registry, economy, citizens, the adapter). `as_local` needs the region, so we must
pick a source. **Decision: store `region_id: RegionId` on `World`.**

- Set it in `RegionState::new` and `RegionState::from_world(id, ..)` (both already
  know the id). Bare `World::new(w, h)` test construction defaults it to `RegionId(0)`.
- Every `&World` system reads `world.region_id`; no signature churn across the system
  layer.
- This **revisits the "the bare `World` is region-agnostic" note** (today in
  `view.rs` on `CitizenRelation::LivesAt`). That stance was only viable while no
  durable field needed the region; `CityEntityRef` makes the region part of the data
  model, so `World` owning its id is the consistent move. Update that note.

> Rejected alternative: thread `RegionId` into every affected system signature. More
> churn, and the id is constant per `World` anyway — a field is the lazy-correct fit.

If a resolved home is somehow not local (should never happen — a citizen is owned by
its home region), local systems skip it rather than panic.

### Save compatibility (concrete load boundary)

Field-level serde **cannot** do this: when `Citizen` deserializes, the region id is
not in scope. So:

- **On disk, `home` stays a bare entity** (custom `serialize_with`/`deserialize_with`,
  or `#[serde(with)]`) — no save-format change, legacy and new saves read identically.
- Deserialization can't know the region, so it constructs a **placeholder**:
  `home = CityEntityRef { region: RegionId(0), entity }`.
- The region is then **stamped at the region load boundary**, where the id is known:
  `RegionState::from_world(id, world)` / `from_save_record` replaces the placeholder
  region `0` with `id` for every citizen (homes are always local, so `region = self.id`
  is unconditionally correct). This is the same pass that already calls
  `rebuild_entity_records` / `refresh_derived_state_for_world`.

Tests:

- Legacy save loads and residents still count at their home (region stamped to `id`).
- Population cache refresh still counts local homes via `as_local(world.region_id)`.
- A home ref with `region != world.region_id` is skipped by local-only systems, not
  panicked on.

## Patch CW3 — City-Wide Workplace Reference (keep the enum)

Make the workplace identity city-wide **without** removing `WorkplaceSource` yet. The
enum's two variants stay; only the data they carry becomes typed/portable. Dropping
the enum is a separate, larger patch (CW4).

Local variant — region-local `Entity` → `CityEntityRef`:

```text
WorkplaceSource::Local { workplace: CityEntityRef }     // was { entity: Entity }
WorkplaceSource::Remote { workplace: CityEntityRef }    // was { slot_id: u32 }  (see wire change)
```

### Wire change: the grant must carry a typed producer entity, not `slot_id`

Today `JobExportGrant.slot_id: Option<u32>` is **documented as opaque**, even though
the producer happens to set it to `workplace.0`. The discovery layer also carries
opaque ids (`RegionalAvailabilityHint.spare_job_slot_ids` in `worker.rs` /
`directory.rs`). Do **not** rely on "slot_id is literally the entity." Instead:

- Change the authoritative grant to carry the producer's workplace explicitly:
  swap `slot_id: Option<u32>` for `workplace: Option<CityEntityRef>`. The producer
  already has the `Entity` when it grants — it just packages it region-tagged. Keep
  `source_region` / `position` / `salary` for now (they stay equal to
  `workplace.region` / the workplace cell; the redundancy is removed in CW4).
- `apply_job_export_grant` stores `Remote { workplace }` directly; no `u32`
  reinterpretation anywhere.
- Update the old `slot_id` consumer `imported_job_slots` (test summary) to read
  `workplace.entity.0`.
- **Leave the discovery hints alone.** `spare_job_slot_ids` stays an opaque
  stale-tolerant availability counter; it is a *hint*, not bound identity, and the
  authoritative binding now lives in the grant's `CityEntityRef`.

### `WorkplaceAssignment` shape (unchanged in CW3)

CW3 keeps the existing shape and only retypes what the `source` variants carry — this
is the minimal, behavior-neutral retype:

```text
WorkplaceAssignment {
    region:   RegionId,          // == source.workplace.region (collapsed away in CW4)
    position: Position,          // workplace cell (becomes a CityCellRef in CW4)
    salary:   i32,
    source:   WorkplaceSource,   // Local { workplace: CityEntityRef } | Remote { workplace: CityEntityRef }
}
```

- **Local job:** `Local { workplace: { region: self, entity } }`; salary/tax read
  `workplace.entity` (a local assignment's workplace is local) — behaves exactly as
  today.
- **Remote job:** `Remote { workplace: { region: producer, entity } }`. The producer
  still settles workplace tax from its own export ledger (keyed by its own entity), so
  this changes no economy behavior.

The `region`/`position` → `CityCellRef location` collapse and the `as_local`-based
local/remote discriminator land in **CW4** with the enum removal.

Tests:

- Local job: salary paid; assignment still exposes the workplace cell.
- Remote job: grant round-trips a typed `CityEntityRef`; `imported_job_slots` reports
  the producer entity id.
- No `slot_id`→entity reinterpretation remains; discovery hints unchanged.

## Patch CW4 — Drop `WorkplaceSource`

Only after CW3 proves the `CityEntityRef` path. Two collapses land together here:

- The enum's `Local`/`Remote` tag is now redundant with the region tag, so collapse
  `source: WorkplaceSource` to a single `workplace: CityEntityRef`; local-vs-remote
  becomes `workplace.region == self.id`.
- The now-redundant `region`/`position` fields collapse into a self-describing
  `location: CityCellRef` (`location.region == workplace.region`), and the grant drops
  its `source_region`/`position` in favor of `workplace` + `location`.

This is **deliberately its own patch** because it is broad — it touches the adapter
roster, economy salary logic, `imported_job_count`, the `remote_workers_for` reverse
lookup, the export-release assumptions, and their tests. Splitting it keeps CW3 small
and lets the typed-ref path land and bake before the enum disappears.

```text
WorkplaceAssignment {
    workplace: CityEntityRef,    // local iff workplace.region == self.id
    location:  CityCellRef,
    salary:    i32,
}
```

Tests:

- Every former `Local`/`Remote` match site now branches on `workplace.as_local(self)`.
- Adapter derives `is_remote` from `workplace.region != self.id` (no view-model change).
- Imported-job count and remote-worker lookup give the same results as before CW4.

## Patch CW5 — Optional Cleanup

Only after CW1-CW4 are working:

- Consider storing `Citizen.id: CitizenId` if deriving it at call sites becomes noisy.
- Consider replacing more view-facing `(region, x, y)` tuples with `CityCellRef`.
- **Make `Entity` city-wide unique — prerequisite for Model B (citizen relocation).**
  Chosen direction: a citizen's `Citizen` component can be *moved* into another region's
  `World` (permanent relocation, where it then ticks in the new region), so a migrated
  entity must never collide with the destination's own local ids. (Model A — only a
  `CityEntityRef`/`CitizenId` crosses while the entity stays home — does **not** need
  this; Model B does.)
  - **Design: birth-tagged region-partitioned id.** `World::spawn` mints
    `Entity { region: RegionId /* birth */, local: u32 }` (or packed
    `Entity(u64) = (region.0 << 32) | local`) from `world.region_id`. The region is the
    **birth** region, fixed for life: a citizen born in A keeps `Entity(A, n)` after
    moving to B, and never collides with B's own `Entity(B, _)`. Per-region deterministic
    counter, no shared allocator (share-nothing + determinism preserved). This converges
    `Entity`, `CitizenId`, and `CityEntityRef` — a birth-tagged `Entity` already is a
    stable city-wide id; decide whether to collapse the aliases.
  - **A global/shared atomic counter is rejected**: non-deterministic across worker
    threads (breaks saves/replays/parity) and reintroduces forbidden cross-region
    coordination.
  - **Save migration — birth region is now persisted per entity.** Unlike `Citizen.home`
    (always local, so CW2 could placeholder-then-stamp at load), a migrated entity's
    birth region is **not** derivable from the `World` it currently lives in, so the id
    must be serialized *with* its region. No placeholder trick.
  - **Ownership semantics shift:** after migration the region bits mean "born in," not
    "owned by." Owner = whichever `World` holds the entity; `Entity::region()` is no
    longer an owner lookup.

  > Model B is bigger than this id change: relocating a `Citizen` also needs a
  > cross-region **migration message** that transfers the serialized component across the
  > share-nothing boundary, re-homes it to a destination building, and removes it from the
  > origin's maps — and it **reopens the traffic plan §5 assumption** that "the entity
  > never migrates." Those belong in a dedicated relocation mission; the city-wide-unique
  > `Entity` here is its foundation.

Skip the other items until the code asks for them; the `Entity` change lands with the
relocation mission, not before.

> Cross-region travel itself (passing a traveler between regions) is **out of scope
> here** — it belongs to the future pathfinding/traffic work and will build on these
> city-wide refs. This plan only makes the references portable.

## Invariants

- UI still sees only view models.
- `World` remains private to its owning region.
- Cross-region messages may carry city-wide refs (`CitizenId`, `CityEntityRef`,
  `CityCellRef`) — but **never a bare region-local `Entity`** stripped of its region tag.
- A region dereferences a `CityEntityRef` (or `CitizenId.local`) **only** when
  `region == self.id`; a foreign ref is carried/echoed, never resolved to a local entity.
- Local simulation remains deterministic: sorted entity/citizen order, integer logic,
  no live reads from neighbor regions.

---

## Implemented (CW1–CW4) — as built

Shipped: `15238f2` (CW1), `337aac3` (CW2), `a3c2672` (CW3), `d4efab4` (CW4). **CW5 is
optional and intentionally not built** (no call site has asked for it yet).

### The model that landed

```text
core::city_refs (CW1)            one rule, everywhere
────────────────────             ───────────────────────────────────────────────
CitizenId     { home_region, local }     a ref is "local" to a region R iff
CityEntityRef { region, entity }           ref.region == R
CityCellRef   { region, x, y }           CityEntityRef::as_local(R) -> Option<Entity>
  (Entity gained Ord so these               Some(entity)  when region == R  (use it)
   derive Ord deterministically)            None          otherwise        (carry, never deref)
```

`World` now records the region it is simulated as in `region_id` (`#[serde(skip)]`):
`RegionState::new`/`from_world` stamp it, and `begin_tick_power_phase` /
`refresh_derived_state_for_world` re-stamp it (guarded, so production — where it already
matches — pays nothing). That single field is what every local/remote decision reads.

```text
   CW2  Citizen.home : CityEntityRef          CW3→CW4  WorkplaceAssignment
   ─────────────────────────────────          ──────────────────────────────────────
   on disk: bare entity  (home_serde)          { workplace: CityEntityRef,
   load: placeholder RegionId(0)                  location:  CityCellRef,   // self-describing cell
   from_world(id): set_region_id(id)              salary }
     stamps region_id + every home              local job  : workplace.region == world.region_id
   home read: home.entity                       remote job : workplace.region != world.region_id
     (home is always local, so the                (no enum tag — CW3 retyped the old
      region tag is informational)                 WorkplaceSource, CW4 deleted it)
```

### Cross-region job export, now typed end to end (CW3/CW4)

```text
PRODUCER region B                         consumer region A
─────────────────                         ─────────────────
process_job_export_request:               apply_job_export_grant:
  workplace = local Entity                  store WorkplaceAssignment {
  grant.workplace  = CityEntityRef(B, e) ─►   workplace,  // B-tagged: as_local(A) = None
  grant.location   = CityCellRef(B, x, y) ─►  location,   // shown on A's roster
  grant.salary                                salary }
        │                                          │
        └ producer ledger still keyed by its       └ A never dereferences a B-entity;
          own Entity; tax accrues to B               imported_job_count/remote_workers_for
          (unchanged)                                discriminate by workplace.region != A
```

The old opaque `slot_id: u32` is gone; the discovery hints (`spare_job_slot_ids`) stay
opaque on purpose — they are stale-tolerant availability counters, not bound identity.

### Why this is behavior-neutral

- Every read that used to test the `WorkplaceSource` tag now tests
  `workplace.region == region_id`; because `region_id` is always stamped to the region
  jobs are assigned under (production via `RegionState`, tests via the guarded
  phase-entry stamp), the two are equivalent for all valid data.
- `Citizen.home`'s on-disk form is unchanged (bare entity), so saves load identically;
  the region is reconstructed at the load boundary.
- Salary/tax accounting, determinism (sorted iteration + integer math), and the view
  models (`JobAssignmentView`, `CitizenRelation`) are untouched — only how the adapter
  *derives* their fields changed, to the same values.

### Deviations from the plan (all intentional)

- CW3 kept `WorkplaceSource` (retype only) and CW4 removed it, exactly as the split was
  rewritten after review — keeping each diff's risk bounded.
- Home reads use `home.entity` directly rather than `as_local(region_id)`: a home is a
  structural always-local ref, so the guard would only ever return `Some`; `as_local`
  is reserved for genuinely-foreign refs (workplaces).
- `region_id` is stamped at the phase entry points (guarded), not only at construction,
  so a bare `World` ticked directly in tests classifies jobs with the same region it is
  assigned under — no production behavior change.

