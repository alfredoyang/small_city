# Collapse `CityEntityRef` and `CitizenId` into `Entity`

## Goal

Remove `CityEntityRef` and `CitizenId`; use `Entity` everywhere instead. After CW5c a
bare `Entity` is a packed, birth-tagged, city-wide-unique id that already carries its
region (`Entity::region()`), so both ref types are now pure redundancy:

```text
CityEntityRef { region, entity }   ── region == entity.region(), so the field duplicates the id
CitizenId     { home_region, local: Entity }   ── home_region == local.region(); local IS the id
```

`CityCellRef { region, x, y }` **stays** — coordinates are not packed into an `Entity`.

## Why now (context)

- CW1 introduced the three city-wide ref types because `Entity` was a region-local
  `u32` that couldn't be safely passed between regions.
- CW5c made `Entity` a packed `u64` = `(birth_region << 32) | local`. The region is now
  *intrinsic to the id*, which is exactly the convergence CW5c's plan note flagged:
  "a birth-tagged `Entity` already is a city-wide id — decide whether to collapse the
  aliases." This plan collapses them.

## Non-Goals

- Do not touch `CityCellRef` (region + coords; not derivable from an `Entity`).
- Do not change any economy/job/UI behavior — this is a pure type collapse.
- Do not add relocation (Model B) — this only removes redundant ref types; the
  birth-vs-owner caveat below is for whoever builds relocation later.

## The guard moves onto `Entity`

`CityEntityRef::as_local` is the one piece of real logic to preserve — the "only
dereference a ref that belongs to me" guard. Move it onto `Entity`:

```rust
impl Entity {
    /// `Some(self)` iff this id was born in `region` (its owning region today, since
    /// nothing relocates yet); `None` for a foreign id. The single guard that keeps one
    /// region from dereferencing another region's entity as a local key.
    pub fn as_local(self, region: RegionId) -> Option<Entity> {
        (self.region() == region).then_some(self)
    }
}
```

> **Birth-vs-owner caveat (for the future relocation mission).** `region()` is the
> *birth* region. For buildings (homes, workplaces) birth == owner forever, so
> `as_local` is exactly right. A *relocated* citizen (Model B) would have birth ≠ owner,
> so its ownership must be tracked by which `World` holds it, not by `entity.region()`.
> Nothing relocates today, so every current `as_local` call is correct.

---

## Patch EC1 — Replace `CityEntityRef` with `Entity`

Delete `CityEntityRef`; move `as_local` onto `Entity` (above). Then at every site the
`{ region, entity }` pair becomes just the `Entity`:

| was | becomes |
|---|---|
| `CityEntityRef::local(region, e)` | `e` (the entity already carries `region`) |
| `ref.entity` | the entity itself |
| `ref.region` | `entity.region()` |
| `ref.as_local(r)` | `entity.as_local(r)` |

### Fields

```text
Citizen.home:               CityEntityRef → Entity
WorkplaceAssignment.workplace: CityEntityRef → Entity
JobExportGrant.workplace:    Option<CityEntityRef> → Option<Entity>
```

### Free simplifications (the point of the patch)

- **Delete `home_serde`.** It existed to serialize `CityEntityRef` as a bare entity and
  re-stamp the region at load. `home` is now an `Entity`; the home building's id already
  packs its region, so it serializes directly (the bytes are *identical* to what
  `home_serde` wrote — the building entity's `u64`) and needs no placeholder/stamp.
- **`World::set_region_id` stops stamping home regions.** They are intrinsic to the
  home `Entity`. It still sets `world.region_id` (and, until EC2, rebuilds `Citizen.id`).
- Every `CityEntityRef::local(local_region, workplace)` wrapper (economy local-job
  assignment, the producer grant, `apply_job_export_grant`) collapses to just the entity.

### Sites (≈10 files)

`city_refs.rs` (delete the type), `entity.rs` (add `as_local`), `components.rs`
(`Citizen.home`, `WorkplaceAssignment.workplace`, drop `home_serde`), `world.rs`
(`set_region_id`), `citizens.rs`, `economy.rs`, `upgrade.rs`, `population.rs`,
`regions/mod.rs` (grant struct + `apply_job_export_grant` + `imported_job_*`),
`runtime/mod.rs` (grant construction), plus test literals (`region_worker_test.rs`,
etc.).

### Behavior neutrality

- `as_local(region)` returns exactly what `CityEntityRef::as_local` did (`region()` ==
  the `region` field that was always set to the entity's region).
- Save bytes for `home` are unchanged (it was already serialized as the building
  entity's id).
- `WorkplaceAssignment` and `JobExportGrant` are `#[serde(skip)]`/transient, so no save
  impact there at all.

### Tests

- Existing local/remote job, roster, and save/load tests must pass unchanged (only
  construction literals updated: `CityEntityRef::local(r, e)` → `e`).
- Keep one assertion that a foreign workplace's `as_local(self_region)` is `None` and a
  local one resolves — the guard's contract, now on `Entity`.

---

## Patch EC2 — Replace `CitizenId` with `Entity`

Delete `CitizenId`; `Citizen.id` becomes the citizen's own birth-tagged `Entity` (which
*is* the stable city-wide person id, and equals the citizen's map key).

```text
Citizen.id:  CitizenId → Entity       (#[serde(skip)], still rebuilt at the load boundary)
spawn:        id = <the spawned entity>
set_region_id rebuild: id = *entity    (the map key)
```

`CitizenId.home_region` is dropped (it equalled `local.region()`); `local` was already
an `Entity`, so the type just *is* `Entity` now.

### Sites (4 files)

`city_refs.rs` (delete the type), `components.rs` (`Citizen.id: Entity`, drop the
`placeholder_citizen_id` helper — `Entity` has an obvious default or use
`Entity(u64::MAX)`/`#[serde(default)]`), `citizens.rs` (spawn), `world.rs`
(`set_region_id` rebuild).

### Behavior neutrality

`Citizen.id` is `#[serde(skip)]` and still has **no production reader** (CW5a was
groundwork). Pure type substitution; nothing observable changes.

### Tests

The CW5a stamping assertion updates from `CitizenId { home_region, local }` to the bare
`Entity` (the map key); it should still assert `id == <the citizen's entity>` after
`set_region_id`.

---

## End state

```text
core::city_refs   →   contains only CityCellRef
core::entity      →   Entity (packed, city-wide-unique) + as_local guard
Citizen           →   { id: Entity, home: Entity, workplace_assignment, morale, money }
WorkplaceAssignment → { workplace: Entity, location: CityCellRef, salary }
JobExportGrant    →   { .., workplace: Option<Entity>, location: Option<CityCellRef>, .. }
```

## Invariants (unchanged)

- A region dereferences an `Entity` as a local key only when `entity.region() ==
  self.id` (`as_local`); a foreign id is carried/echoed, never resolved.
- Cross-region messages carry `Entity` and `CityCellRef`, both region-tagged.
- Local simulation stays deterministic: sorted iteration, integer logic, no neighbour
  reads.
