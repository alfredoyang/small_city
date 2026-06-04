# Richer In-Region Map UI Plan

This plan improves how the terminal UI draws a single region's map so the city
reads more like a city. It does not change the simulation, the regional
threading model, or the UI boundary: the TUI still renders only from interface
view models (`GameView`, `CellView`, `InspectView`) and never touches ECS
`World`, workers, or runtimes.

The headline feature is **connected-road line-art**: replacing the flat `==`
road tile with box-drawing glyphs that visually connect, so a road grid looks
like streets instead of a field of equals signs.

## Goals

- Make roads render as connected line-art in the existing TUI map panel.
- Keep the map grid perfectly aligned in any standard terminal.
- Add no new external dependencies.
- Add view-model fields before adding rendering, per the project architecture
  rule.
- Keep ASCII and non-UTF-8 terminals fully working and unchanged.

## Non-Goals

- Do not change core simulation, economy, power, population, or road analysis.
- Do not expose `World` or ECS storage to the UI.
- Do not add a graphical (non-terminal) UI.
- Do not change the default tile theme. Line-art is opt-in through the existing
  `Unicode` theme.

## Current State (what already exists)

Confirmed by reading `src/ui/tui.rs` and `src/interface/view.rs`:

- The map panel (`render_map`) draws each cell as a fixed **2-character tile**
  via `TileTheme::tile_for_cell(cell, overlay, is_cursor, preview)` returning a
  `TileGlyph { tile: String, style: Style }`.
- There are three themes: `AsciiCompact`, `AsciiDetailed` (default, "ASCII-2"),
  and `Unicode`, cycled with the theme toggle.
- **Zone colors already exist.** `cell_base_style` -> `building_style(kind)`
  already colors every building type, and dead-end/unpowered cells get
  `problem_style()`. So "color-coded zones" is done; it is not part of this plan.
- `CellView` exposes `building`, `road_connected`, `powered`, `power_demand`,
  `population`, `local_effects`, etc., but **not** which neighbors are roads.
- `tile_for_cell` only sees one `CellView`; it has no neighbor context, which is
  why roads currently render as a context-free `==`.

The missing piece for line-art is per-cell road adjacency, which belongs in the
view model.

## Patch M1: Road Adjacency In The View Model

Goal: give each map cell the directions in which it connects to neighboring
roads, computed in core and exposed as UI-safe data.

Likely files:

- `src/interface/view.rs` (add the field to `CellView`)
- `src/interface/adapter.rs` (populate it in `view_world` / `view_world_with_overlay`)
- `tests/inspect_view_test.rs` or a focused adapter test

Implementation:

- Add a road-link mask to `CellView`, for example:

  ```text
  pub struct RoadLinks { pub north: bool, pub east: bool, pub south: bool, pub west: bool }
  // field on CellView:
  pub road_links: RoadLinks,
  ```

- In the adapter, for each cell that is a road, set each direction true when the
  orthogonal neighbor cell is also a road. Use the same positional adjacency the
  road-connectivity system already relies on; do not invent new rules.
- For non-road cells, `road_links` is all false.
- Keep this purely derived data. It is rebuildable from authoritative state and
  carries no ECS references.

Tests:

- a straight horizontal road run produces `east`/`west` links and no `north`/`south`
- a corner produces exactly two perpendicular links
- a 4-way intersection produces all four links
- non-road cells produce an all-false mask
- map edges do not report links to out-of-bounds neighbors

Review focus:

- The field is owned, UI-safe, and derived from authoritative state.
- Adjacency matches the existing road model; no duplicated or divergent logic.
- No other view model behavior changes.

## Patch M2: Connected-Road Line-Art In The Unicode Theme

Goal: render roads as connected box-drawing glyphs in the `Unicode` theme,
keeping the grid aligned and the ASCII themes unchanged.

Likely files:

- `src/ui/tui.rs` (Unicode road tile, gap suppression, width-safety test)

Implementation:

- Only the `Unicode` theme changes. `AsciiDetailed` and `AsciiCompact` keep
  `==` so ASCII and non-UTF-8 terminals are unaffected.
- Use the **light** box-drawing set (`─ │ ┌ ┐ └ ┘ ├ ┤ ┬ ┴ ┼`). It is in the
  Box Drawing block (U+2500–U+257F), East Asian Width "Neutral" = **width 1**,
  so each glyph occupies exactly one column and the grid stays aligned. Avoid
  emoji, CJK, and East-Asian-Ambiguous symbols (arrows, `● ★ ◆ ■`), which can
  render width 2 and would desync rows.
- Map the 4-bit `road_links` mask `N E S W` to a 2-character tile using the
  node-in-left-column rule:
  - **Left char** = box glyph for the arm set `{up:N, down:S, left:W, right:E}`.
  - **Right char** = `─` if `E` else a space.
  - See the design table and preview in the appendix below.
- **Gap suppression:** when an east-connected road abuts a west-connected road,
  fill the inter-cell `map_cell_gap` with `─` instead of a space so horizontal
  streets do not show seams. Do not change the gap anywhere else.
- Preserve the existing tile invariant, restated from "two ASCII chars" to
  "two display columns": every tile is exactly two `char`s drawn from
  {ASCII, the light box-drawing set}. Because those are all width 1,
  `chars == columns == 2`, so cursor highlight and map width stay stable.

Tests:

- mask-to-tile mapping for straight, corner, T-junction, and cross cases
- a width-safety test asserting every road tile is exactly two chars and every
  char is in the allowed width-1 set (no `unicode-width` dependency needed)
- a small rendered road network matches an expected aligned snapshot
- `AsciiDetailed` road tiles are still `==` (no regression in the default theme)
- gap suppression joins two horizontally connected road cells

Review focus:

- Line-art is confined to the `Unicode` theme; default behavior is unchanged.
- Tile width invariant holds; no width-2 glyphs can slip in.
- No new dependencies.

## Patch M3 (Optional): Intensity-Graded Zones

Goal: in the Normal overlay, modulate a zone's existing color by activity so
thriving and struggling districts read at a glance.

Likely files:

- `src/ui/tui.rs` (theme/style layer only)

Implementation:

- Adjust `building_style` brightness or modifier based on existing `CellView`
  data such as `population`/`max_population` or `local_effects.land_value`.
- No view-model change; this is a pure styling tweak.

Tests:

- a high-population residential cell renders a different style than a low one
- non-building cells are unaffected

Review focus:

- Color changes stay subtle and deterministic.
- No overlay other than Normal is altered.

## Patch M4 (Deferred): Cursor-Following Scroll Viewport

Not needed at the current default region size (20x15 fits inside the map panel
in a 100x30+ terminal; ratatui clips today). Implement only when region maps can
exceed the visible panel. When implemented:

- Track a viewport offset in `TuiState`, follow the cursor, and clamp to map
  bounds.
- Render only the visible window of cells.
- Add tests for cursor-follow and clamping at all four edges.

## Design Appendix: Mask To Tile

Rule: left char is the box glyph for `{up:N, down:S, left:W, right:E}`; right
char is `─` if east-connected, else a space.

```text
N E S W   tile    meaning
-------   ----    -------------------------
0 0 0 0   --      isolated road (special-cased)
1 0 1 0   |.      vertical street      (. = space)
0 1 0 1   --      horizontal street
1 1 0 0   |_->    corner N->E   (left-up-right elbow, then east arm)
1 0 0 1   corner  N->W
0 1 1 0   corner  S->E
0 0 1 1   corner  S->W
1 1 0 1   T (no south)
0 1 1 1   T (no north)
1 1 1 0   T (no west)
1 0 1 1   T (no east)
1 1 1 1   4-way intersection
1 0 0 0   dead-end up      (stub or vertical)
0 0 1 0   dead-end down
0 1 0 0   dead-end right
0 0 0 1   dead-end left
```

Concrete glyphs (light box-drawing), left char then right char:

```text
NESW=0101 horizontal : ──
NESW=1010 vertical   : │ (space)
NESW=1100 N+E corner : └─
NESW=1001 N+W corner : ┘ (space)
NESW=0110 S+E corner : ┌─
NESW=0011 S+W corner : ┐ (space)
NESW=1101 T no S     : ┴─
NESW=0111 T no N     : ┬─
NESW=1110 T no W     : ├─
NESW=1011 T no E     : ┤ (space)
NESW=1111 crossing   : ┼─
```

Dead-end stubs `╵ ╷ ╴ ╶` are also width-1 box-drawing but rarer in fonts; they
may be collapsed to `│`/`──` if preferred.

Eyeball preview — a ring road `R R R / R . R / R R R` renders (with gap
suppression, right-column space shown as a trailing blank) to a clean rectangle:

```text
┌───┐
│   │
└───┘
```

A 4-way crossing renders coherently as well:

```text
 │
─┼─
 │
```

Note the two harmless quirks of node-in-left-column: vertical streets hug the
left column of their cells, and the right edge of a shape sits in the cell's
left column, leaving a trailing blank column on the far right.

Alternative considered: the "doubled glyph" scheme (one line-art glyph per mask,
repeated, e.g. `──`, `││`, `└└`, `┼┼`) is perfectly symmetric and centered but
makes corners and junctions look doubled and horizontal continuity slightly
worse. The node-in-left-column scheme above is preferred for cleaner straights,
corners, and intersections.

## Guardrails

- Box-drawing glyphs are width-1; do not introduce emoji, CJK, or ambiguous-width
  symbols into any tile.
- Line-art lives only in the `Unicode` theme; the default `AsciiDetailed` theme
  and the ASCII fallback are unchanged.
- No new external dependencies. Width safety is enforced by restricting the glyph
  set and asserting two-char tiles, not by pulling in `unicode-width`.
- Each patch is one mission, stays within the project limit of roughly five files
  and 400 changed lines, and includes tests.

## Review Checklist

- Does the change render only from view models, with no ECS access?
- Is road adjacency derived from authoritative state and matched to the existing
  road model?
- Do all tiles remain exactly two display columns in every theme?
- Is the default theme behavior unchanged?
- Did `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test` pass?
