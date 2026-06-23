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

Skip these until the code asks for them.

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

