# Citizen Roster Popup — Plan

## Goal

Let the player see the individual citizens tied to a building. With the cursor
on a building, opening the roster shows a scrollable popup:

- **Residential** → the citizens who **live** there (residents), each with their
  job, happiness, money, age.
- **Commercial / Industrial** → the citizens who **work** there (workers), each
  with where they live, salary, happiness, money, age.

Anything else (road / power plant / park / empty) → no roster (a short "no
citizens" note, or the key is a no-op there).

```text
            cursor on a building
                   │  open roster
                   ▼
   ┌─ Residents of R(3,2) ─ 3/3 ───────────┐
   │ #  age  happy  $    works at          │
   │ 1   27   72   14    C (5,0) local      │
   │ 2   34   41   3     I (7,1) local      │
   │ 3   19   88   21    — unemployed       │
   └ ↑/↓ scroll · Esc close ───────────────┘
```

## What already exists (reuse, don't rebuild)

- `Citizen` (`src/core/components.rs`) already carries everything we display:
  `age`, `home: Entity`, `workplace_assignment: Option<WorkplaceAssignment>`
  (`region`, `position`, `salary`, `source: Local{entity} | Remote{slot_id}`),
  `morale.actual` (happiness), `money`.
- Residents of a building = `world.citizens.filter(|c| c.home == entity)`.
  The adapter already does exactly this in `job_assignment_views_for_home`
  (`src/interface/adapter.rs:650`).
- Local workers of a workplace = `world.citizens.filter(|c|
  c.workplace_assignment.source == WorkplaceSource::Local { entity == W })`.
  Same identity the economy uses (`economy.rs:160,375`).
- The TUI already fetches an `InspectView` for the selected cell every frame and
  passes it to `render_selected_cell` (`tui.rs:1083`). The roster can ride that
  existing inspect read — **no new cross-layer request/reply plumbing**.
- A modal template already exists: `render_quit_confirm` + `centered_rect` +
  `Clear` (`tui.rs:1103`). The popup copies this pattern.

## Do we modify core? No.

The citizen data already exists in core — this feature adds **no simulation
change**. Every field the roster shows is already on the `Citizen` component
(`age`, `money`, `morale.actual`, `home`, `workplace_assignment`). The reverse
lookups (residents of a home, workers of a workplace) are plain filters over
`world.citizens`, the same ones the adapter/economy already use. No new
components, no new systems, no new fields in core.

What is missing is purely the **projection**: the current API exposes only
*aggregates and anonymized slices* (`average_happiness`, `average_money`,
`population`, `citizens` count, and `job_assignments` — residents' jobs only,
no per-citizen attributes, nothing for workers-of-a-workplace). There is no
view model carrying one citizen's full detail. That gap is closed entirely in
the interface layer.

## Layered architecture & mission boundary

UI never touches ECS. Citizen data becomes a **UI-safe view model** in the
adapter (the sole ECS→view boundary), then renders in the TUI. No `Entity` ids,
no remote slot ids leak out — same rule the existing views follow.

`(+)` = added by this feature. **M1** owns the core→view projection; **M2** owns
the TUI popup. The simulation core and the regional facade/threading are
untouched.

```text
┌─ src/core  (SIMULATION — UNCHANGED) ───────────────────────────────────────┐
│  World.citizens : HashMap<Entity, Citizen>                                 │
│    Citizen { age, money, morale.actual, home: Entity,                      │
│              workplace_assignment: Option<{region,position,salary,src}> }  │
│  residents(building) = citizens.filter(home == building)                   │
│  workers(workplace)  = citizens.filter(workplace.src == Local{workplace})  │
└───────────────────────────────────────────────┬───────────────────────────┘
                                                  │  ECS read (adapter only)
┌─ src/interface  (ECS→VIEW BOUNDARY) ── M1 ──────▼───────────────────────────┐
│  view.rs                                                                   │
│    (+) CitizenDetailView { age, happiness, money, relation }               │
│    (+) CitizenRelation   { WorksAt | Unemployed | LivesAt }                │
│    (+) InspectView.roster : Vec<CitizenDetailView>  (local citizens only)  │
│  adapter.rs::inspect_world(world, x, y)                                     │
│    (+) fills roster: residential→residents, C/I→local workers, else empty  │
│        sorted by Entity.0 (deterministic); resolves worker home→Position   │
└───────────────────────────────────────────────┬───────────────────────────┘
                                                  │  existing inspect path —
                                                  │  NO new request/reply enum
┌─ src/core/regional_game.rs  (UI FACADE — UNCHANGED) ─▼───────────────────────┐
│  RegionalGame::inspect_region(region, x, y) -> InspectView                  │
│    already plumbed runner→worker→runtime→RegionState; roster rides along    │
└───────────────────────────────────────────────┬───────────────────────────┘
                                                  │
┌─ src/ui/tui.rs  (FRONTEND) ── M2 ───────────────▼───────────────────────────┐
│  render loop ALREADY holds an InspectView for the cursor cell               │
│  (+) TuiState { citizen_panel: bool, citizen_scroll: usize }                │
│  (+) Enter on occupied R/C/I → open panel (empty land still builds)         │
│  (+) handle_citizen_panel_key (modal: ↑/↓ scroll, Esc close)                │
│  (+) render_citizen_panel(inspect) — reuses centered_rect + Clear           │
│        renders rows straight from inspect.roster                            │
└────────────────────────────────────────────────────────────────────────────┘
```

One-line flow:

```text
Citizen (core, exists) → inspect_world projects → InspectView.roster (view model)
   → RegionalGame.inspect_region (existing path) → TUI modal renders rows
```

Why no new transport: the roster is a field **inside the InspectView the TUI
already requests**, so nothing new crosses the region-threading boundary — no
new `UiRequest`/`UiReply` variant, no worker/runtime change. The data already
makes the trip; we put more in the existing envelope. The only read that
reaches past a normal inspect is resolving a worker's `home: Entity` → its
`Position` for `LivesAt`, still a pure read inside `inspect_world` on the core
side of the boundary.

## Cross-region limitation (call out, don't solve now)

A workplace can employ **remote** workers imported from another region. Those
citizens live in their home region's `World`; this region only holds an opaque
slot reservation, not the worker's identity. So a workplace roster can fully
enumerate **local** workers only. A remote-worker *count* is **not derivable at
the adapter boundary either**: the producer's export reservations live in the
runtime ledger, not in the `World` the adapter reads — so M1 exposes local
workers only and does not add a `remote_worker_count` field. (M2 may show a
static "local workers only" footnote on workplace rosters.) Residents who hold
a `Remote` job are still fully listed (`WorksAt { is_remote: true }`), since the
resident lives in this region and carries the assignment. This matches the
existing one-way cross-region data model and keeps the feature
single-region-local.

> **DONE — limitation lifted:** remote workers are now listed on a workplace
> roster by reverse-looking-up the consumer regions where they live (keyed on
> `(producer_region, position)`, which the consumer citizen records). Shipped as a
> read-only, all-five-layers, 3-mission feature (no simulation/tick/export-protocol
> change): commits `e87646d` (M1), `6a90205` (M2), `2e43f13` (M3). The "local
> workers only" footnote below is therefore superseded by a `N local · M remote`
> breakdown. Design + implemented architecture:
> [remote-workers-roster-plan.md](remote-workers-roster-plan.md).

## Missions (one patch each, per the dev loop)

### M1 — view model + adapter roster (core/interface)

- Add to `src/interface/view.rs`:
  ```rust
  pub struct CitizenDetailView {
      pub age: u32,
      pub happiness: i32,          // morale.actual
      pub money: i32,
      pub relation: CitizenRelation,
  }
  pub enum CitizenRelation {
      // For a residential roster: where this resident works.
      WorksAt { region: RegionId, x: usize, y: usize, salary: i32, is_remote: bool },
      Unemployed,
      // For a workplace roster: where this worker lives.
      LivesAt { x: usize, y: usize },
  }
  ```
- Add `pub roster: Vec<CitizenDetailView>` to `InspectView` (default empty).
- In `adapter.rs::inspect_world`, populate `roster`:
  - Residential → residents (`home == entity`), each mapped to `WorksAt`/`Unemployed`.
  - Commercial/Industrial → local workers (`source == Local { entity }`), each
    mapped to `LivesAt` (resolve the worker's `home` Entity → its `Position`).
  - Everything else → empty.
  - **Deterministic order**: sort by citizen `Entity.0` (the adapter already
    uses this ordering for `job_assignment_views_for_home`).
- No `remote_worker_count`: not derivable from the `World` (see the cross-region
  note above). Workplace rosters are local-workers-only.
- Tests (`tests/inspect_view_test.rs` + adapter unit tests):
  - Residential roster lists each resident once, in entity order, with correct
    job/unemployed mapping.
  - Commercial/Industrial roster lists each local worker with the right
    `LivesAt` position.
  - Road/power/park/empty roster is empty.
  - Determinism: two inspects of the same state produce identical rosters.

Size: ~2 files (`view.rs`, `adapter.rs`) + tests. Well under the 5-file/400-line cap.

### M2 — TUI roster popup (ui)

- `TuiState`: add `citizen_panel: bool` (open/closed) and `citizen_scroll: usize`.
- Key binding decision (**see "Open key" below**). Default plan: make **Enter
  context-sensitive** — on an occupied R/C/I cell Enter opens the roster; on
  empty land Enter still builds. Keep `b`/`B` as the unambiguous build key
  (already mapped). Add a fallback explicit key too if preferred.
- While the panel is open it is modal (like the quit/prompt modals): `↑/↓`
  scroll, `Esc`/`Enter`/`q` close, other gameplay keys ignored. Add a
  `handle_citizen_panel_key` mirroring `handle_quit_confirm_key`.
- `render_citizen_panel(frame, area, inspect)`: `centered_rect` + `Clear` +
  bordered `Paragraph`/rows from `inspect.roster`, a title naming the building
  and citizen count, and (on workplaces) a static "local workers only" footnote.
  Render after the base layout so it overlays (like `render_quit_confirm`).
- Empty roster (e.g. a building with no citizens yet) → show "No citizens yet".
- Tests (`tui.rs` unit tests, no real terminal):
  - Toggling `citizen_panel` open/closed via the key handler.
  - Panel only opens on R/C/I, not on road/empty.
  - Rendered rows match `inspect.roster` length; scroll clamps at bounds.
  - 2-column tile alignment invariant is untouched (popup is separate widget).

Size: 1–2 files (`tui.rs`, maybe `tui_input.rs`) + tests.

### M3 — aligned columns + in-list cursor via ratatui `Table` + `TableState` (ui)

M2 shipped the popup as a `Paragraph` whose rows are pre-formatted strings
(`citizen_row`), with a bare scroll offset (`citizen_scroll`) and **no visible
cursor on the list** — you can't tell which row is "current." Swap to ratatui's
native **`Table`** (aligned columns) driven by a **`TableState`** (the selected
row *is* the cursor). Both are already in deps (0.29).

- In `render_citizen_panel`, replace `Paragraph::new(lines)` with
  `Table::new(rows, constraints).header(header)`:
  - Header `Row`: `# · Age · Happy · $ · Relation` (bold/yellow).
  - One `Row` per `inspect.roster` entry (no manual slicing — `TableState`
    handles the viewport, see below).
  - Column `Constraint`s, e.g. `Length(4) / Length(5) / Length(6) / Length(6) /
    Min(12)` — the last column flexes, the rest stay fixed so values line up.
  - `.row_highlight_style(reverse/bold)` + `.highlight_symbol("> ")` so the
    selected row reads as a cursor.
  - Render with `frame.render_stateful_widget(table, popup, &mut state)`.
- **Rename `citizen_scroll: usize` → `citizen_selected: usize`** in `TuiState`
  (the selected row *is* the cursor). *(ponytail: keep a plain `usize`, not a
  persisted `TableState` — `render_citizen_panel` builds a local
  `TableState::default().with_selected(Some(i))` (offset 0) each frame and
  ratatui's `get_row_bounds` scrolls the viewport so the selection stays visible.
  Persist a real `TableState` only if you later need sticky mid-screen scrolling.)*
  `↑/↓` in `handle_citizen_panel_key` move the selection (clamped to
  `0..roster.len()`); opening the panel selects row 0. Render also clamps
  `selected` to the live roster length so a roster that shrank while open keeps a
  visible cursor.
- Collapse `citizen_row(number, citizen) -> String` into
  `relation_text(&CitizenDetailView) -> String` (just the relation `match` arm);
  the numeric fields become their own cells instead of being baked into a string.
- Keep the title, the "No citizens yet" empty state, and the workplace
  "(local workers only)" footnote as lines/caption around the table.
- `Table` has no `.wrap()`, so the relation column **truncates** instead of
  wrapping — acceptable (arguably better) in a fixed-width popup.
- Determinism/layer rules unchanged: `TableState` is UI-only state, like the
  scroll offset it replaces; the roster view model is untouched.
- Tests: rename/repoint the `citizen_row` formatting test to `relation_text`;
  assert the cursor moves and clamps via the key handler
  (`state.citizen_selected`); render a non-empty roster through
  `render_citizen_panel` and assert the popup shows the column header, the
  per-row values, and the `> ` cursor symbol on the selected row.

Size: 1 file (`tui.rs`) + test tweak. UI-only; no view-model or core change.

## Open key (decision needed before M2)

Enter is currently `TuiAction::Build` (`tui_input.rs:70`). Options:

1. **Context-sensitive Enter** (recommended, matches the user's ask): occupied
   R/C/I → open roster; empty land → build. `b`/`B` remains a pure build key.
2. **Dedicated key** (e.g. `i`/`Enter`-free): unambiguous, but the user asked
   for Enter specifically.

Recommend option 1. Confirm before implementing M2.

## Risks / notes

- Roster is recomputed on every inspect (each cursor move) even when the popup
  is closed. Citizen counts are tiny (≈ base×area, single digits), so the cost
  is negligible; revisit only if profiling ever says otherwise. *(ponytail:
  reuse the existing inspect read instead of adding a request channel; add
  dedicated plumbing only if roster size grows.)*
- No new dependencies. No balance/sim changes — this is read-only presentation.
- Determinism holds: rosters are a pure function of world state in a fixed
  (entity-id) order.

## Architecture diagram (post-implementation, to append per dev-flow step 8)

```text
 press Enter on a building
        │
        ▼
 TuiState.citizen_panel = true ──► modal loop (↑/↓ scroll, Esc close)
        │
        └─ render_citizen_panel(inspect)
                 ▲
                 │ reads
        InspectView.roster: Vec<CitizenDetailView>   ◄─ filled by adapter::inspect_world
                                                          residents:  home == entity
                                                          workers:    workplace == entity (Local)
                                                          remote:     not listed (other region)
```

---

## Implemented architecture (M1 + M2)

Status: **done**. M1 `efc3a4a`, M2 `8bf2a74`. No simulation/core change — the
feature is a read-only projection (M1) plus a TUI modal (M2).

### Data flow as built

```text
 ECS (unchanged)                interface (M1)                 ui/tui (M2)
 ───────────────                ──────────────                 ───────────
 World.citizens                 adapter::inspect_world         render loop fetches
   Citizen{age,money,    ─────►   └ citizen_roster(x,y)  ─────►  inspect each frame
     morale, home,                   residents | local workers     │
     workplace_assignment}           → Vec<CitizenDetailView>      ▼
                                    (sorted by Entity.0)        render_citizen_panel(inspect)
                                  InspectView.roster ───────────┘  (Clear + centered popup)
```

### M1 — projection (src/interface)

- `view.rs`: `CitizenDetailView { age, happiness, money, relation }` +
  `CitizenRelation { WorksAt{region,x,y,salary,is_remote} | Unemployed |
  LivesAt{x,y} }`; `InspectView.roster: Vec<CitizenDetailView>`.
- `adapter.rs`: `citizen_roster(world, x, y)` + `citizen_relation(...)`.
  Residential → residents (`home == entity`), each `WorksAt`/`Unemployed`;
  Commercial/Industrial → local workers (`workplace_assignment.source ==
  Local{entity}`), each `LivesAt` (worker `home` Entity → `world.positions`);
  every other cell → empty. Deterministic: `sort_by_key(Entity.0)`.
- Boundary: a workplace's remote workers live in another region's world and a
  remote-worker *count* is not in this world either (it's in the runtime export
  ledger), so workplace rosters are local-only. Residents holding a remote job
  are still listed (`is_remote: true`).

### M2 — TUI popup (src/ui)

```text
 key event
   │
   ├─ handle_prompt_key ───────────► (save/load filename) ─ consumed
   ├─ handle_quit_confirm_key ─────► (quit dialog)        ─ consumed
   ├─ handle_citizen_panel_key ────► OPEN: ↑/↓ scroll (clamp to live roster),
   │                                       Esc/Enter/q close              ─ consumed
   └─ map_key_event → apply_action
         Enter → EnterCell:  cell_has_roster(inspect)? open panel : Build
         b/B   → Build       (always builds)
```

- `tui_input.rs`: `Enter → TuiAction::EnterCell`; `b/B → Build`.
- `tui.rs`: `TuiState{ citizen_panel, citizen_scroll }` (M3 renames
  `citizen_scroll` → `citizen_selected`); `apply_action(EnterCell)` opens on
  R/C/I else builds;
  `handle_citizen_panel_key` is fully modal (dispatched after the quit modal,
  before `map_key_event`); `render_citizen_panel` draws the popup from the
  `InspectView` the render loop already holds — title `Residents of`/`Workers
  at`, `No citizens yet` when empty, `(local workers only)` footnote on
  workplaces; `citizen_row` formats one line per citizen. *(Superseded by M3:
  the `Paragraph`/`citizen_row` rows become a ratatui `Table` + `TableState`
  with a header row, aligned columns, and a highlighted selected row as the
  in-list cursor — replacing the cursor-less `citizen_scroll` offset.)*
- The cursor is clamped twice: in the key handler (against the live roster
  length) and again at render (so a roster that shrank while open keeps a
  visible, in-range highlight).

### Invariants preserved

- UI reads only view models (`InspectView.roster` / `CitizenDetailView`); no ECS
  access from `src/ui`.
- Determinism unaffected — roster is a pure function of world state in a fixed
  (entity-id) order; the panel and scroll are UI-only state.
- No new cross-region transport: the roster rides inside the existing
  per-frame `InspectView`.

### Tests

- M1 (`tests/inspect_view_test.rs`):
  `roster_lists_residents_with_their_workplace_and_workers_with_their_home`
  (WorksAt/LivesAt, empty road/empty cells, count match, determinism).
- M2 (`src/ui/tui.rs`, `src/ui/tui_input.rs`): Enter→EnterCell mapping;
  Enter opens on a zone / builds on empty land; Enter no-ops the panel on a road;
  modal scroll-clamp + close; `citizen_row` formatting for all four relations;
  popup render headers (`Residents of` / `Workers at` + footnote).

---

## M3 — Table + in-list cursor (done, `d39d4d3`)

Status: **done**. UI-only; no view-model/adapter/core change, no new deps. M2's
`Paragraph` of hand-padded strings (`citizen_row`) with a cursor-less scroll
offset becomes a ratatui `Table` whose **selected row is the cursor**.

### Layout as built

```text
 popup = centered_rect(60,60)            ← Clear + bordered Block, title carries
   │                                       "Residents of / Workers at (x,y) — N …"
   ├─ block.inner ─┬─ body  (Table)        header: # · Age · Happy · $ · Relation
   │               │         rows: one per inspect.roster entry (relation_text)
   │               │         selected row: REVERSED + "> " highlight_symbol
   │               └─ footer (1 line, workplaces only): "(local workers only)"
   └─ empty roster → "No citizens yet." in body (footer still shown on workplaces)
```

### Cursor / scroll model (ponytail)

```text
 TuiState.citizen_selected : usize   (the cursor; renamed from citizen_scroll)
        │  ↑/↓ in handle_citizen_panel_key: ±1, clamped to 0..roster.len()
        ▼
 render: TableState::default().with_selected(Some(selected.min(len-1)))   ← rebuilt
        │                                                                    per frame,
        ▼                                                                    offset 0
 ratatui get_row_bounds scrolls the viewport so the selected row is visible
```

- No persisted `TableState`: a fresh one each frame (offset 0 + selected) is
  enough because ratatui recomputes the viewport to reveal the selection. Persist
  a real `TableState` only if sticky mid-screen scrolling is ever wanted.
- `selected` is clamped at render against the live roster length — if the roster
  shrinks while the panel is open, the cursor clamps to the last row instead of
  vanishing (ratatui scrolls an out-of-range selection into view but won't draw
  its highlight).
- `citizen_row(number, &c) -> String` → `relation_text(&c) -> String` (relation
  column only); the numeric fields are now their own `Table` cells.

### M3 tests (`src/ui/tui.rs`)

- `relation_text_formats_each_relation` — the four relation arms.
- `citizen_panel_table_renders_header_and_rows` — renders a non-empty roster
  through `render_citizen_panel` (bypassing the sim, which doesn't spawn citizens
  deterministically in a unit test): asserts the column header, per-row aligned
  values, the `> ` cursor on the selected row, and that an out-of-range selection
  clamps to the last row.
- `citizen_panel_key_moves_cursor_clamped_and_closes` — ↑/↓ clamp against the
  live roster, Esc closes and stops consuming keys.
