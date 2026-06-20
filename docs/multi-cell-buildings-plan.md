# Multi-cell buildings: grow-on-upgrade, merge same-type, block when boxed in

Status: planned (not implemented). Core/simulation mission (+ a UI rendering
patch). Implements the §5 multi-cell feature reserved in
[tui-city-redesign-plan.md](tui-city-redesign-plan.md), using its reserved
`Building.footprint` and `CellView` hooks.

## The mechanic (locked decisions)

Upgrading a building costs **space**, not just money:

1. **Grow on upgrade (gradual).** Footprint area per level: **L1 = 1, L2 = 2,
   L3 = 4** cells (nominal shapes 1×1 → 2×1 → 2×2). These per-level areas are
   **configurable** (see [Configurable footprint ruleset](#configurable-footprint-ruleset)).
   Capacity still scales with level as today; the footprint is the new *spatial*
   requirement.
2. **Claim cells — same-type first, then empty.** To reach the next level's area,
   the building claims adjacent cells. Scanning its perimeter in a fixed order
   (N, E, S, W), it prefers an adjacent **same-type building → merge it**, else an
   **empty cell → claim it**. Roads, different-type buildings, and out-of-bounds
   (incl. region borders) are not claimable.
3. **Merge = absorb + transfer.** A merged same-type neighbor is removed; its
   cells join this building's footprint and its **population / goods transfer into
   the merged building up to the new capacity** (excess is lost).
4. **Block when boxed in.** If the area target can't be reached (not enough
   claimable cells, even with merges), the upgrade **fails** and the player is
   told *"no space to level up"* (message + inspect flag). Money is not spent.
5. **Scope: Residential, Commercial, Industrial only.** Road, Power Plant and Park
   stay 1×1.

```
 L1            upgrade            L2 (claimed the same-type R to the east → merge)
 ┌──┐                             ┌──┬──┐
 │R │  + [R east, same type]  →   │R │R │   one building, level 2,
 └──┘                             └──┴──┘   B's residents moved in (≤ capacity)

 boxed in:  ══ R ══     press U →  "no space to level up"  (R surrounded by roads)
            ══════
```

## Data model

- **`Building.footprint`** (already reserved): store the footprint as a **strict
  rectangle** — the anchor (top-left, min y then min x) plus `width × height`.
  **Every level is a rectangle** (1×1, then 2×1 or 1×2, then 2×2); growth extends
  one full side, and a merge is only allowed when it keeps the footprint
  rectangular (see the algorithm). `#[serde(default)]` to 1×1 so old saves load
  unchanged.
- **`Grid` stays `Vec<Option<Entity>>`** — write the same owner `Entity` into every
  footprint cell. Add `set_footprint` / `clear_footprint` / `footprint_cells`
  helpers; storage type unchanged.
- **Capacity stays level-driven** (existing upgrade scaling). Area is decoupled
  from capacity: if prior merges already gave a building ≥ the next level's area,
  the upgrade claims no new cells and just bumps level/capacity.

```
world.buildings:  E7 → Building{ kind:Residential, level:2, footprint:{(3,4),(4,4)} }
world.positions:  E7 → (3,4)            ← anchor (top-left), carries the label
grid cells:       (3,4)=Some(E7) (4,4)=Some(E7)
get(4,4) → E7 → inspect/bulldoze/upgrade all resolve to the one building
```

## Configurable footprint ruleset

The per-level footprint areas are **data, not constants** — tunable without
recompiling.

- **Format: JSON via `serde_json`** (already a project dependency — no new crate;
  TOML/RON are avoided since they would add one).
- **Delivery: embedded default + optional external override.** A baseline ruleset
  is baked into the binary with `include_str!` so the game always runs; an
  external `config/buildings.json` overrides it when present. Malformed overrides
  fail loudly with a clear error; the embedded default is guarded by a parse test.
- **Lives in `core`** as a `BuildingRules` resource; the UI never reads it.

```json
{
  "buildings": {
    "Residential": { "footprint_area_per_level": [1, 2, 4] },
    "Commercial":  { "footprint_area_per_level": [1, 2, 4] },
    "Industrial":  { "footprint_area_per_level": [1, 2, 4] }
  }
}
```

```rust
struct BuildingRules { footprint_area_per_level: BTreeMap<BuildingKind, Vec<u32>> }
impl BuildingRules {
    fn footprint_area(&self, kind, level) -> u32  // clamped to the table length
}
```

- **Validation on load:** each list is non-empty, strictly positive, and
  non-decreasing (an upgrade must never *shrink* a footprint); length covers all
  levels. R/C/I are required; Road/Power/Park are fixed 1×1 and not configurable.

### Determinism: the ruleset is stamped into saves

Footprint areas are **game rules**, and replays assume fixed rules (CLAUDE.md §3).
So the active `BuildingRules` is **written into the save file** and **loading uses
the saved rules**, not the current external JSON:

- `RegionalGameSave` gains `building_rules: BuildingRules`, `#[serde(default)]` to
  the embedded baseline so **legacy saves load unchanged** (they get the [1,2,4]
  default).
- A **new** game reads the embedded default / external override; that ruleset then
  travels with the city. Editing `config/buildings.json` afterwards affects only
  *new* games — an existing save replays identically regardless. Replay parity is
  guaranteed.
- The rules are stored once at the `RegionalGameSave` level and injected into each
  region's `World` on construction (single source of truth, no per-region drift).

## The upgrade algorithm (deterministic, strict rectangle)

The footprint is always a rectangle, so an upgrade **extends the rectangle by one
full row or column** to a rectangle of the next level's target area.

```
fn upgrade(entity):
    next = level + 1; if next > MAX: fail "already max level"
    target = rules.footprint_area(kind, next)
    if current_rect.area >= target:                 # earlier merges already grew it
        level = next; capacity = capacity_for(next); return ok

    # Try extending one full side; only sides that yield a rectangle of `target` area count.
    candidates = []
    for side in [N, E, S, W]:                        # fixed order → deterministic
        new_rect = current_rect.extend(side)         # adds one row/column on that side
        if new_rect.area != target: continue
        added = new_rect.cells - current_rect.cells  # the new row/column
        if added all satisfy claimable_in_rect(new_rect):   # see below
            candidates.push(side)

    # Prefer a side that MERGES a same-type neighbor, else a side that is all-empty.
    side = candidates.find(|s| added_cells(s) contain a same-type building)
                 .or(candidates.first())
    if side is None: fail "no space to level up"     # nothing changed (atomic)

    # Commit:
    merge same-type buildings fully inside the added cells (absorb + transfer)
    claim the empty added cells
    footprint = new_rect; level = next; capacity = capacity_for(next)
```

- **`claimable_in_rect(new_rect)`** for each added cell: it is **empty**, or it
  belongs to a **same-type building whose entire footprint lies inside `new_rect`**
  (no overhang). A road, a different-type building, a same-type building that would
  overhang the rectangle, or an out-of-bounds cell (incl. region border) makes that
  side fail — this is what keeps the footprint a rectangle.
- **Side preference (determinism, CLAUDE.md §3):** sides are tested N, E, S, W;
  **pass 1** picks the first side whose added cells include a mergeable same-type
  building; only if none, **pass 2** picks the first all-empty side. First match
  wins; identical inputs → identical result.
- **Merge absorbs whole neighbors** that sit fully inside the new rectangle (their
  contents transfer, capped at capacity); because they must be fully contained,
  the result stays a clean rectangle.
- **Atomic:** if no side qualifies, nothing is claimed/merged and no money is
  spent — the failed upgrade changes nothing.

## Contents transfer on merge

- **Residential:** reassign the absorbed building's citizens' `home` to the merged
  entity, up to the new `max_population`; despawn citizens beyond capacity
  (deterministic order). The residential population cache is rederived as today.
- **Commercial / Industrial:** add the absorbed building's stored goods to this
  building's stock, capped at the new capacity (excess lost); business cash sums.
  Jobs are level-derived, so nothing to transfer there.
- All transfers are integer and order-deterministic.

## Touch points (core)

- `components.rs` — `Building.footprint` (+ helpers for area / anchor).
- `grid.rs` — `set_footprint` / `clear_footprint` / `footprint_cells`.
- `systems/build.rs`, `placement.rs` — place new buildings 1×1 (unchanged effect).
- `systems/upgrade.rs` — the algorithm above (the bulk of the work).
- `systems/bulldoze.rs`, `entity_cleanup.rs` — clear **all** footprint cells.
- `systems/citizens.rs` — citizen reassignment on residential merge.
- `systems/local_effects.rs` — measure building distance from the **nearest**
  footprint cell. *First cut:* keep anchor-distance (cheap); refine only if a
  2×2's effects feel off.
- `road_connectivity` — a building is road-connected / road-adjacent if **any**
  footprint cell touches a road.

## Touch points (UI — the §5 rendering the redesign plan already shaped for)

- `interface/view.rs` — fill the reserved `CellView` fields: `is_building_anchor`,
  `footprint`, `footprint_offset`.
- `interface/adapter.rs` — resolve **any** footprint cell → owner entity for
  inspect; expose an `UpgradeBlockedNoSpace` flag.
- `ui/tui.rs` — render the multi-cell **sprite** (anchor = letter, body cells =
  zone-colored fill sharing the bg), and make the cursor/preview highlight span
  the whole footprint (the renderer is already shaped via a cell predicate).
  Inspect shows footprint size and the "no space to level up" flag.

## Suggested split (each its own tested patch)

- **M0 — configurable footprint ruleset.** `core/building_rules.rs`:
  `BuildingRules` (per-kind `footprint_area_per_level`), embedded default via
  `include_str!` + optional `config/buildings.json` override, validation, and
  `footprint_area(kind, level)`. Stamp it into `RegionalGameSave`
  (`#[serde(default)]`) and inject into each region's `World`. No gameplay change
  yet — nothing reads the area until M2. Tests: default parses and validates; a
  good override loads; a bad/shrinking override is rejected; save round-trips the
  rules; a legacy save (no field) gets the default.
- **M1 — footprint plumbing, 1×1 parity.** `Building.footprint` (default 1×1),
  grid helpers, write/clear on build/bulldoze, cell→owner inspect resolution.
  **No behaviour change** — everything is still 1×1. Tests: 1×1 round-trips;
  grid helpers; inspect any cell of a (manually 2-cell) building → one card.
- **M2 — grow + claim + merge + block.** The upgrade algorithm: footprint grows,
  same-type neighbors merge (cells only), upgrade blocks with the message/flag
  when boxed in; atomic rollback. Tests: upgrade grows footprint; merges an
  adjacent same-type building; blocks when surrounded; rollback leaves state and
  money untouched; determinism (same layout → same claimed cells).
- **M3 — contents transfer.** Residential citizen reassignment and commercial/
  industrial goods transfer on merge, capped at capacity. Tests: residents move
  in up to cap, excess lost; goods transfer capped.
- **M4 — UI rendering.** Multi-cell sprites, footprint-spanning cursor, inspect
  footprint + blocked flag; fill the reserved `CellView` fields. Tests: anchor vs
  body cells render; 2-col width held; blocked flag shows.

Run `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test -q` after each.

## Risks / guardrails

- **Determinism** is the top risk: the fixed N,E,S,W scan, pass-1-same-type /
  pass-2-empty, and ordered citizen reassignment are non-negotiable. Add a
  save/replay parity check.
- **Atomicity**: a failed upgrade must change *nothing* (rollback claimed cells,
  no spent money, no partial merge).
- **Balance**: merging transfers population/goods — capacity caps prevent spikes,
  but watch for pop loss surprising the player; surface "excess lost" in the
  upgrade message. Keep economy formulas otherwise untouched.
- **Save migration**: `footprint` defaults to 1×1 and `building_rules` defaults to
  the embedded baseline, so existing saves load unchanged.
- **Config vs. determinism**: the ruleset is stamped into saves and load uses the
  saved rules, so editing `config/buildings.json` never breaks an existing city's
  replay parity — it only affects new games.
- **Region borders**: footprints never cross a region edge (out-of-grid cells are
  not claimable → may block an upgrade at the border, which is correct).
- **Architecture**: all simulation logic in `core`; UI renders view models only
  and resolves cell→owner in the adapter, never the ECS.

## Levels & capacity (today vs this plan)

The current code only supports **one** upgrade step (`MAX_UPGRADE_LEVEL = 2`).
Capacity-on-upgrade today is a mix:

- **Formula** — commercial/industrial jobs: `BuildingKind::jobs_at_level =
  base + (level − 1)` (C: 2→3, I: 3→4). Extends to any level for free.
- **Hardcoded L2 values** in `apply_upgrade_effect` — residential `population.max`
  5→8, power capacity 10→15, park happiness 3→5, industrial pollution 2→3.

To reach this plan's **L3 (2×2)** you must raise `MAX_UPGRADE_LEVEL` to **3**, and:

- **Residential — area formula (chosen):** `max_population = base × area × 3/2`
  (integer; core is float-free), with `area == 1` → `base`. E.g. base 8 →
  1×1: 8, 1×2: 24, 2×2: 48. Capacity falls out of the footprint, so residential
  needs **no per-level table** and L3 is automatic. `base` is the 1×1 value
  (today 5 — set to 8 to match the example).
- **Commercial / Industrial:** keep the `jobs_at_level` formula (extends to L3
  for free).
- **Power / Park:** still hardcoded single-step; need an explicit L3 value (or
  give them the same area treatment later). ponytail: hardcode L3 now, table only
  if you actually tune them.

## Open design notes

- **Footprint shape: strict rectangle** (chosen). Every footprint is a `w × h`
  rectangle; growth extends one full side and merges only same-type neighbors that
  sit fully inside the new rectangle. Cleaner sprites and inspect, at the cost of
  more "no space to level up" blocks than a free-form connected set would hit —
  accepted.
- **Down-level / un-merge** is out of scope: bulldoze removes the whole building.
