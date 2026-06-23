# City-Wide Citizen References Plan

## Goal

Make citizen/travel references city-wide so cross-region travel can pass a
traveler between regions without moving the whole `Citizen` component or leaking
region-local `Entity` ids into another `World`.

Current local-only shape:

```text
Citizen.home:              Entity
WorkplaceSource::Local:    Entity
TravelState.current_cell:  Entity
TravelState.destination:   Entity
```

Those `Entity` values only mean something inside one region. A region receiving
one of them cannot resolve it safely.

Target shape:

```text
CitizenId      = stable city-wide person id
CityEntityRef  = { region, entity }       // internal city-wide ECS reference
CityCellRef    = { region, x, y }         // portable crossing/save/display reference

Citizen stays owned by home region.
Travel state may move between regions, keyed by CitizenId.
```

## Non-Goals

- Do not make `World` city-global.
- Do not let one region read another region's ECS.
- Do not move the durable `Citizen` component between regions.
- Do not change economy/job behavior while migrating ids.

## Architecture

```text
                 durable owner                         temporary host
REGION A                                      REGION B
──────────────────────────────               ──────────────────────────────
citizens[local_entity] = Citizen              visiting_travel[CitizenId]
  id: CitizenId                                 current_cell: B-local road
  home: CityEntityRef(A, home)                  destination: B-local target
  workplace: CityCellRef(B, x, y)

CitizenId crosses regions.
Region-local Entity refs do not.
```

Each region still stores and mutates only its own ECS. City-wide refs are handles
that include `RegionId`; a region may only dereference refs whose `region == self.id`.
Cross-region messages use `CitizenId` plus cell/link data, and the receiver resolves
that into its own local road/building entities.

## Types

Add small plain data types near the existing `Entity`/regional types:

```rust
pub struct CitizenId {
    pub home_region: RegionId,
    pub local: Entity,
}

pub struct CityEntityRef {
    pub region: RegionId,
    pub entity: Entity,
}

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

Change `Citizen.home` from `Entity` to `CityEntityRef`.

Local systems must resolve with:

```text
citizen.home.as_local(local_region)
```

If the home is not local, treat the citizen as invalid for that local system and
skip/fall back to home. In normal data, a citizen's home is always local to its
owning region.

Save compatibility:

- Legacy saves with `home: Entity` load as `CityEntityRef { region: local_region, entity: home }`.
- New saves write the city-wide form.

Tests:

- Legacy save loads and residents still count at their home.
- Population cache refresh still counts local homes.
- Non-local home ref is ignored by local-only systems instead of panicking.

## Patch CW3 — City-Wide Workplace Reference

Change local workplace identity from region-local only:

```text
WorkplaceSource::Local { entity: Entity }
```

to city-wide:

```text
WorkplaceSource::Local { workplace: CityEntityRef }
```

Keep remote job data as cell/slot based:

```text
WorkplaceAssignment {
    region,
    position,
    salary,
    source,
}
```

For local jobs, `region/position` and `source.workplace` describe the same local
building. For remote jobs, `region/position` stays the portable target and
`Remote { slot_id }` stays producer-owned.

Tests:

- Local job assignment still pays salary and shows workplace location.
- Remote job assignment remains unchanged.
- UI roster still resolves local workers' homes through the adapter only.

## Patch CW4 — Split Travel Identity From Local Route State

Replace "cross the same raw `TravelState`" with a city-wide traveler record.

Keep local `TravelState` using local entities for route stepping:

```text
TravelState {
    current_cell: Option<Entity>,
    destination: Option<Entity>,
    status: TravelStatus,
}
```

Add a wrapper for ownership/identity:

```text
TravelerState {
    citizen: CitizenId,
    travel: TravelState,
}
```

For local residents, `world.travel` can stay keyed by local citizen `Entity` until
P5 needs the crossing path. For visiting travelers, use:

```text
visiting_travel: HashMap<CitizenId, TravelState>
```

Tests:

- Local movement remains unchanged.
- Adapter can render local travel and visiting travel through the same
  `CitizenTravelView`.

## Patch CW5 — Cross-Region Travel Uses Portable Handoff

Update P5 in `traffic-pathfinding-plan.md` to use a portable handoff instead of
raw `TravelState`:

```text
TravelerHandoff {
    citizen: CitizenId,
    from_region: RegionId,
    to_region: RegionId,
    entry_link: BorderLinkId,
    destination: CityCellRef,
    return_path: Vec<ReturnHop>,
    purpose: Outbound | Return,
}
```

Receiver behavior:

```text
Receive Outbound:
  map entry_link -> local entry road
  map destination CityCellRef -> local workplace/building if destination.region == self
  create local TravelState in visiting_travel[citizen]

Receive Return:
  if return_path has a hop, route one hop back
  if empty and self is citizen.home_region, clear away mark
```

Owner-side stale guard:

```text
accept Return only if citizen is currently away for that same CitizenId/generation
otherwise ignore
```

Tests:

- A handoff never contains raw region-local `Entity` refs from another region.
- A→B commute creates a B-local visiting `TravelState`.
- Stale return is ignored.
- A→B→C return unwinds through B.

## Patch CW6 — Optional Cleanup

Only after CW1-CW5 are working:

- Consider keying `world.travel` by `CitizenId` instead of local `Entity`.
- Consider storing `Citizen.id: CitizenId` if deriving it at call sites becomes noisy.
- Consider replacing more view-facing `(region, x, y)` tuples with `CityCellRef`.

Skip these until the code asks for them.

## Invariants

- UI still sees only view models.
- `World` remains private to its owning region.
- Regions only dereference `CityEntityRef` when `region == self.id`.
- Cross-region messages carry `CitizenId` and `CityCellRef`, not foreign `Entity`.
- Local simulation remains deterministic: sorted entity/citizen order, integer logic,
  no live reads from neighbor regions.

