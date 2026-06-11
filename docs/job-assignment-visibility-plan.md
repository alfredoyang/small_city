# Job Assignment Visibility Plan

This proposal exposes where citizens work without exposing ECS internals to UI.
It replaces the old CR4 imported-resource visibility idea with a narrower,
job-focused model that covers both local and remote jobs.

## Goal

Show each employed citizen's workplace region and tile location through
`GameView`/inspect data.

This is not a power visibility patch. Power imports may keep recording only the
producer region needed by simulation; UI does not need to show power source
details.

## Design

Use one owned assignment model for local and remote jobs:

```rust
pub struct WorkplaceAssignment {
    pub region: RegionId,
    pub position: Position,
    pub salary: i32,
    pub source: WorkplaceSource,
}

pub enum WorkplaceSource {
    Local { entity: Entity },
    Remote { slot_id: u32 },
}
```

Local assignments still keep the local workplace entity for core simulation.
Remote assignments keep the producer-owned opaque slot id. UI does not receive
either identifier.

The interface layer converts the core assignment into a view-safe shape:

```rust
pub struct JobAssignmentView {
    pub region: RegionId,
    pub x: i32,
    pub y: i32,
    pub salary: i32,
    pub is_remote: bool,
}
```

## Flow

```text
Local assignment
  economy/job system finds workplace entity
      |
      v
  Citizen.workplace_assignment =
      region: current region
      position: local workplace tile
      salary: local salary
      source: Local { entity }
      |
      v
  adapter emits JobAssignmentView without Entity

Remote assignment
  producer grants job export
      |
      v
  JobExportGrant carries producer region + workplace tile + slot id + salary
      |
      v
  consumer stores Citizen.workplace_assignment =
      region: producer region
      position: producer workplace tile
      salary: grant salary
      source: Remote { slot_id }
      |
      v
  adapter emits JobAssignmentView without slot id
```

## Implementation

- Add `WorkplaceAssignment` and `WorkplaceSource`.
- Replace split citizen job state reads with `Citizen.workplace_assignment`.
- For local jobs, store the current region, workplace tile position, salary, and
  `WorkplaceSource::Local { entity }`.
- Extend `JobExportGrant` to carry the producer workplace tile position.
- For remote jobs, store producer region, producer workplace tile position,
  salary, and `WorkplaceSource::Remote { slot_id }`.
- Add a view-safe `JobAssignmentView` or equivalent field to inspect/view models.
- Update inspect output so a citizen can show its workplace region and tile.
- Do not expose `Entity`, `World`, or remote slot ids to UI.
- Do not add power-source visibility.

## Tests

- Local employed citizen inspect/view shows workplace region and tile.
- Remote employed citizen inspect/view shows producer region and workplace tile.
- UI/view models do not expose ECS `Entity`, `World`, or remote slot id.
- Save/load behavior remains rebuildable: remote assignment data should not
  become authoritative cross-region truth beyond the existing derived job state
  policy.

## Review Focus

- One assignment model covers local and remote jobs.
- UI receives only owned, view-safe data.
- Local simulation still has enough identity to run deterministic job/economy
  logic.
- Remote simulation still has enough opaque identity for producer-owned export
  allocation.
