# TUI city-look redesign (SimCity skin), built for multi-cell buildings & traffic

Status: planned (not implemented).

Target aesthetic: the original **SimCity (1989, DOS/Maxis)** — tan ground, green
forest blobs, gray roads, colored `R`/`C`/`I` zones, a `Funds · City · Date`
header, a left tool strip, a bottom tool/cost line. We evoke that in a character
grid (no pixel sprites) with strict map alignment.

**Scope now: UI-only skin. Zero ECS changes, zero view-model changes.** Two
known-future features — multi-cell buildings (footprints > 1×1) and traffic
simulation — are **reserved, not built here**. We only *shape the render code* so
adding them later is a localized change, and we write down the contract they will
need (§2). Nothing in `core/`, and no new `CellView` fields, are added by this
plan.

Locked decisions:

1. **Replace `TileTheme::Unicode` in place** with this "City" look.
   `AsciiCompact` / `AsciiDetailed` stay as bare-terminal fallbacks.
2. **Quarantine all emoji in panels** (tool strip, HUD, legend). Never in the map.
3. **Zone tiles use letter markers `R`/`C`/`I`** (SimCity style) — already how
   `AsciiDetailed` renders, so low-risk.
4. **Left tool strip is a static icon+hotkey legend**, not interactive.
5. **Reserve for multi-cell/traffic in the render code's *shape* only** — add no
   `CellView` fields and touch no ECS now. §2 records the contract so the future
   missions are a localized change, not a rewrite.

UI-only (one file, `src/ui/tui.rs`). The §2 fields are **future** additive
view-model changes documented here for design intent; they are **not** part of
this plan's patches.

---

## 1. The alignment rule (everything bows to this)

Every map tile is **exactly 2 display columns** (`render_map`:
`cell_width = 2 + gap.len()`); the cursor highlight must not change tile width.
Emoji are double-width with terminal-dependent measurement, so:

| Zone | Emoji? | Why |
|---|---|---|
| **Map grid** (`render_map`, `tile_for_cell`) | ❌ never | 2-col lock; emoji shift the grid |
| **Panels** (tool strip / HUD / legend / inspect) | ✅ yes | nothing column-aligns to their right |

Multi-cell buildings and traffic both stay 2-col per cell (a footprint is N
2-col cells; a congested road is still a 2-col road tile), so the rule survives
both features.

---

## 2. Reserved rendering contract (design only — NOT built in this plan)

This section is a **forward-compat note**, not work. It records the `CellView`
fields multi-cell/traffic will eventually need, so we can *shape* the render
functions now to absorb them later without a rewrite. **We add none of these
fields now and touch no ECS.**

| Field | Status | Future meaning for the renderer |
|---|---|---|
| `building: Option<BuildingKind>` | **exists today** | which letter/color/zone bg |
| `road_links: RoadLinks` (NESW) | **exists today** | road box-drawing glyph |
| `population/level/...` | **exists today** | occupancy/level shade (char 2) |
| `local_effects` | **exists today** | overlay intensities |
| `is_building_anchor: bool` | *reserved (future)* | draw the letter here; body cells draw fill |
| `footprint: (u8,u8)` | *reserved (future)* | sprite size + cursor highlight span |
| `footprint_offset: (u8,u8)` | *reserved (future)* | cell's position within its building |
| `traffic: Option<i32>` | *reserved (future)* | congestion value for the Traffic overlay |

**What this plan actually does** to stay ready: structure the render code so the
reserved fields drop in as a localized change — namely

- keep tile rendering in **one `tile_for_cell` seam** (already true) so a future
  footprint/sprite branch is one added match arm, not a scatter of edits;
- keep cursor/preview highlighting computed from **a cell predicate** (today:
  "is this the cursor cell") so it can later become "is this cell part of the
  selected building" without restructuring;
- keep the overlay list in **one `MapOverlayInput` seam** so a `Traffic` variant
  is one arm in the cycle/legend/color match.

That shaping is free — it's just where we put the code, using fields that already
exist. The reserved fields and their `core` logic come with their own missions.

```
TODAY (this plan)                 LATER (separate missions, no rewrite)
 tile_for_cell(cell) ─ letters    + footprint arm reads is_anchor/offset
 cursor = (x==cur && y==cur)      → cursor = same-building predicate
 overlays: Normal..Desirability   + Traffic arm reads cell.traffic
 (UI still never touches ECS)
```

---

## 3. Layout: top header bar + left tool strip

Current bands (`render`): `[map | inspect]` / `[status | build]` / `[messages]`.
New skeleton:

```
╔ Funds: $20,000          Smallville          Day 12 · 09:00 ╗   header (Length 1)
╭Tools╮╭ City Map ── Overlay: Normal ─────────╮╭ Inspect ─────╮
│🚜 b ││  R▓ C░ I▒  ϟ█  ♣  (earth ground)      ││ (inspect card)│
│═ r  ││  └──┴──┐                              ││               │
│🏠 R │╰─────────────────────────────────────╯╰───────────────╯
│🏪 C │╭ City HUD ───────────────╮╭ Build ───────────────────╮
│🏭 I ││ 💰 👥 💼 ⚡ 😊 🏭 📦 📈   ││ (build preview)           │
│⚡ P │╰─────────────────────────╯╰───────────────────────────╯
│🌳 K │ Residential: $100   (cursor tile + cost)               bottom line
╰─────╯
```

- **Header bar**: new `Constraint::Length(1)` at top. `Funds: ${money}` left ·
  city/region name centered · `time.label` right; blue/gray SimCity styling.
- **Left tool strip**: split the map band into `[Tools(Length ~8) | map | inspect]`.
  Static list, one tool/line, emoji + existing hotkey. A legend, not a selector.
- **Bottom line**: reuse the messages band, SimCity-styled, showing the message or
  the cost of the building under the cursor (from the existing build preview).

Tool strip: 🚜 Bulldoze · ═ Road · 🏠 R · 🏪 C · 🏭 I · ⚡ Power · 🌳 Park
(emoji-safe; each leads its line; keys unchanged).

---

## 4. The map grid: SimCity ground + zoning colors + letter tiles

Strictly 2-col single-width. The city feel is **background color** + letters.

### 4a. Background zoning colors

**Decision: the map gets an explicit "muted earth" ground** so empty land no
longer inherits the terminal's black default (the "too dark" problem). Muted
earth is dark-but-not-black, so bright zone glyphs stay high-contrast and it
behaves well on dark terminals.

| Cell | fg | new bg |
|---|---|---|
| Empty land (ground) | DarkGray | **muted earth** `Rgb(60,48,34)` (dark warm brown; 16-color → none) |
| Residential | Green | dark green |
| Commercial | Yellow | dark blue |
| Industrial | Magenta | dark yellow/olive |
| Park | LightGreen | **green** (forest blob) |
| PowerPlant | Cyan | dark cyan |
| Road | Gray | none (infrastructure) |
| Problem (no road / unpowered) | Red bold | none — red overrides the tint |

The zone bg tints sit a notch brighter than the ground so districts read as
raised lots on the earth. Keep zone glyph fg bright (bold) for contrast against
both the ground and the tint.

Applied in `cell_base_style`; `problem_style` overrides. Truecolor muted-earth
ground with a 16-color fallback (omit bg).

### 4b. Letter zone tiles (char1 = letter, char2 = occupancy/level shade)

`R░ R▓ R█` · `C░ C▒ C▓` · `I░ I▒ I▓` · `ϟ█` · `♣ ` · roads keep box-drawing ·
empty `..` on muted earth. All single-width. Problem-marker path (`R!`, `I-`)
unchanged.

### 4c. Selection (single-cell now; reserved to span a footprint later)

Cursor/preview highlight uses a **bright background**. Now it highlights the
single cursor cell, but compute it through **a cell predicate** (`is this the
cursor cell`) rather than inline coordinate checks — so a future footprint can
swap the predicate to "is this cell part of the selected building" with no
restructuring. No footprint logic is added now.

---

## 5. Multi-cell building sprites (RESERVED — future mission, not built now)

Design intent only; depends on the §2 reserved fields. A building occupying N
cells is N zone-colored 2-col tiles — *this is the SimCity
"sprite across a zoned lot" look*. Rendering rule, generalizing the existing
road renderer (which already picks a glyph from `road_links`):

- **Anchor cell** (`is_building_anchor`): zone letter + shade, e.g. `R▓`.
- **Body cells**: zone-color fill from `footprint_offset` — interior `▓▓`, or
  (optional, later) box-drawing edges/corners for the SimCity lot border, chosen
  from the offset exactly like roads choose from links.
- All footprint cells share the zone **bg color**, so the lot reads as one block.

```
2×2 Commercial            3×3 Park
 C▓ ▓▓                     ♣  ♣  ♣
 ▓▓ ▓▓                     ♣  ♣  ♣     (anchor top-left carries the label;
 (anchor = C, body = ▓)    ♣  ♣  ♣      body cells fill in zone color)
```

Inspect/cursor on any body cell resolves to the one building (adapter maps
cell→owner entity), so the card shows the building once — matching the `core`
rule that a footprint is one entity with one anchor `Position`.

**Core-side model (for the future mission, kept here so the contract is grounded):**
`Grid` stays `Vec<Option<Entity>>`; the same `Entity` is written into all
footprint cells. `Building` gains `footprint {w,h}` (rectangle only); `Position`
is the anchor (top-left). Build validates all cells empty+in-bounds; bulldoze
clears all; road-adjacency = *any* footprint cell touches a road; distance
effects measure from the nearest footprint cell (anchor-distance is an acceptable
first cut). A footprint never crosses a region border.

---

## 6. Traffic overlay (RESERVED — future mission, not built now)

Design intent only; depends on the §2 reserved `traffic` field. The overlay
system (`MapOverlayInput`, intensity coloring, cycle, legend) already exists for
Power/Pollution/Population/LandValue/Desirability. Traffic will be **one more
variant**:

- Add `MapOverlayInput::Traffic`; cycle + legend gain an arm.
- Road cells tint green→yellow→red from `cell.traffic` via the existing
  `intensity_tile` / `intensity_style` machinery (same as pollution). Non-road
  cells render dim. Zero new rendering architecture.
- `traffic: None` today → overlay shows empty until `core` fills it.

**Core-side model (future mission):** traffic is a *derived per-road-cell scalar*,
computed deterministically each tick by routing trips (home→work, →shop) over the
existing road network and accumulating load per cell — it rides `LocalEffects`
like `pollution_pressure`, **no `Grid` storage change**. Determinism (CLAUDE.md
§3) requires fixed origin order + fixed tie-break on equal-length paths.
Per-direction/one-way flow is a later refinement, not in scope.

---

## 7. City HUD + colored legend

- **City HUD**: repurpose the Status panel; each stat emoji leads its line
  (`💰 👥 💼 ⚡ 😊 🏭 📦`), demand as colored `block_meter` bars. Fields already on
  `view.status` — no view-model change.
- **Legend**: `TileTheme::legend` renders each entry in its real zone color, with
  the letter↔zone map and (new) a Traffic legend arm.

---

## 7b. Quit confirmation + save (IMPLEMENTED)

Pressing **Esc / Q** no longer exits instantly — it opens a modal that
double-confirms and offers to save first, so an accidental keypress can't drop
unsaved progress. **Ctrl-C** stays an immediate, unconfirmed quit (emergency
hatch).

```
╭ Confirm Quit ───────────────────╮
│        Quit Small City?         │
│   Unsaved progress will be lost.│
│  S Save & Quit   Q Quit   Esc … │
╰─────────────────────────────────╯
   S → opens the Save filename prompt, flagged to quit once the save succeeds
   Q / Enter → quit now (no save)
   Esc / N / C → cancel, back to the game
```

Mechanics (already in `tui.rs` / `tui_input.rs`): a new `TuiAction::RequestQuit`
(Esc/q/Q) opens `TuiState.quit_confirm`; `Ctrl-C` keeps the immediate
`TuiAction::Quit`. A fully-modal `handle_quit_confirm_key` consumes keys while the
dialog is open; "Save & Quit" reuses the existing Save prompt with a `then_quit`
flag, and a successful save sets `pending_quit`, which the event loop honors.
UI-only, no `core/` change; covered by unit tests.

---

## 7c. Feel & fun: feedback, juice, tactile building

Make the city feel **alive and responsive** so playing is enjoyable, not just
informative. Two of these are UI-only (fold into T2/chrome); two are their own
missions because they touch input or the driver.

### F1. Trend arrows on HUD stats — UI-only (T2)

Show `▲ / ▼ / →` beside Pop, Money, and Happiness in the City HUD so the player
reads a *story over time* ("my city is growing"), not just a number.

- Keep the **previous tick's `view.status`** in `TuiState` (UI-side snapshot,
  refreshed each tick). Compare current vs previous; pick the glyph.
- Color the arrow (green up / red down) per stat. No view-model or `core/` change.

```
 👥 312 ▲   💰 $20,400 ▼   😊 72 →
```

### F2. Build juice — UI-only (T2)

Every action should *react*. On a successful build/bulldoze/upgrade:

- a brief **confirm flash** on the affected cell (reuse the preview green/red for
  a few frames), and
- a **transient floating delta** near the cursor: `+$100` / `-$100`, fading out.

- Implement as a small UI-side **effect with a frame-counter lifetime** in
  `TuiState` (e.g. `{ cell, text, frames_left }`); decrement per render, drop at
  zero. **Off a UI frame counter, never sim state, never fed back into `core/`**
  (keeps determinism/saves intact, CLAUDE.md §3).

### F3. Drag / paint to build — input mission (separate)

The biggest *tactile* win: hold-drag to lay a **road line** or repeat-place a zone
along the cursor path, instead of one cell per keypress.

- A UI-side **paint mode**: on drag-start record the anchor; as the cursor moves,
  apply the selected tool to each newly-entered cell by calling the existing
  facade `build()` per cell (no `core/` change — just multiple normal builds).
- Needs event-loop work (track press/drag/release or a toggled paint mode) and a
  preview of the pending line. Deterministic: each cell is an ordinary build.
- **Its own mission**, after the skin — not part of T1/T2.

### F4. Undo last action — driver mission (separate, touches `core/`)

Forgiveness lowers anxiety and invites experimentation.

- Snapshot the region state before each mutating command (build/replace/upgrade/
  bulldoze), keep a small bounded undo stack in the driver/facade, and add an
  `undo()` that restores the last snapshot. Bind to a key (e.g. `Z`).
- **Not UI-only** — it adds a command/snapshot surface to the driver (and must
  respect the regional threading model). Scope it as its own mission; weigh cost
  vs. a simpler "snapshot only the last command" first cut.

> F1 + F2 ride along with the chrome work; F3 and F4 are deliberately *not*
> bundled — they each deserve their own small, tested patch.

---

## 8. Emoji robustness

Gate header/strip/HUD/legend emoji behind `locale_supports_unicode()`; ASCII
fallback on bare terminals. The **map grid is emoji-free regardless**, so only
panels degrade.

---

## 9. Scope / guardrails

- **This plan = one file: `src/ui/tui.rs`. No `core/` change, no `CellView`/
  view-model change.** The §2 reserved fields are documented for a future mission,
  not added here.
- **Alignment guard:** keep the 2-col invariant; test that every City-theme tile
  — all zones and problem markers — renders exactly 2 columns, so a wide glyph
  can't break the grid. Assert the tool strip + header never feed the map width
  math. (When footprints land later, extend this test to body/offset cells.)
- **Deterministic / UI-only:** renders from view models only; no ECS access. The
  cell→owner-entity resolution for multi-cell inspect lives in the adapter.
- Run `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test -q` after each patch.

### Suggested split

- **T1 — map skin:** zoning bg + muted-earth ground + letter tiles + single-cell
  selection via a cell predicate + 2-col test. Reads as SimCity now; shaped per §2
  so footprints/traffic are localized later, but builds none of them.
- **T2 — chrome:** header bar + left tool strip + bottom cost line + City HUD +
  colored legend + emoji fallback + **F1 trend arrows** + **F2 build juice** (§7c).
- **Later missions (separate):** `core` footprints (§5), traffic routing (§6),
  **F3 drag/paint build** (input, §7c), **F4 undo** (driver, §7c). Footprints/
  traffic only flip the §2 defaults the renderer already honors; F3/F4 are their
  own small, tested patches.

Each is independently shippable; T1 is the core "looks like SimCity" win and the
piece that bakes in multi-cell/traffic readiness.

---

## Appendix: preview mockups (current functions → new skin)

ASCII can't show color, so colors are annotated in captions. Every current TUI
panel, key, and overlay is preserved — only the *look* changes. Target ≥100×30.

### A. Full screen — Normal overlay (the everyday view)

```
 💰 Funds: $20,000              S M A L L V I L L E              ☀ Day 12 · 09:00 ◐
╭Tools─╮╭ City Map ─ Overlay: Normal · Theme: City ──────────╮╭ Inspect: (12,4) ──────╮
│🚜 X  ││     8   9  10  11  12  13  14                       ││ COMMERCIAL    Lvl ██░ 2│
│═  1 ◀││  3  ⌂▓  ⌂█  │   ▦▓  ▦░  ╬▒  ╬░                      ││ ⚡on 🛣✓ 🔧3  👷2 jobs  │
│🏠 2  ││  4  ⌂▒  └───┴───┐   ╬▓  ϟ█  ..                      ││ Goods ▕████░░░░▏ 4/12   │
│🏪 3  ││  5  ♣   ..  ┌───┘   ╬░  ╬▒  ..                      ││ Cash  ▕██████░░▏30/50 ▲ │
│🏭 4  ││  6  ..  ..  │   ▦█  ▦▓  ..  ..                      ││ Sales 1/shopper rec -1  │
│⚡ 5  ││  7  ==  ==  ==  ==  ==  ==  ==                      ││ Source▕████▓▏ 🏭6·🌍2   │
│🌳 6  ││ (earth ground · R green · C blue · I magenta · ϟcyan)││ Land▆ Poll▂ Acc▅ Desr▅ │
╰──────╯╰──────────────────────────────────────────────────╯╰────────────────────────╯
╭ City HUD ─────────────────── ▶ RUNNING · 1× ─╮╭ Build / Actions ───────────────────╮
│ 💰 $20,000   👥 312 pop   💼 84 jobs (4 idle) ││ Tool: 🏪 Commercial                 │
│ ⚡ 120/160   😊 72 happy  🏭 14 pollution      ││ Cost: $100 | Upkeep: $3            │
│ 📦 +18 made · 4 imported · 6 exported         ││ Can Build: Yes                     │
│ 📈 R ▮▮▮·· · C ▮▮··· · I ▮▮▮▮·                 ││ B Build  R Replace  U Upgrade  X Bz│
╰───────────────────────────────────────────────╯╰────────────────────────────────────╯
╭ Messages / Tick Summary ────────────────────────────────────────────────────────────╮
│ Simulation: RUNNING | 1× | auto tick every 1 second                                  │
│ Built Commercial at (12,4) for $100                                                   │
│ Space pause · +/- speed · WASD move · 1-6 tools · N next · O overlay · T theme · H help│
╰──────────────────────────────────────────────────────────────────────────────────────╯
```

Function mapping (nothing removed, only restyled):

| Current panel / line | New location | Note |
|---|---|---|
| `Status`: Turn/Money/Pop/Citizens | header bar + City HUD | Funds in header; pop/citizens in HUD |
| `Status`: Jobs/Unemployed/Happiness/Pollution | City HUD | `💼 / 😊 / 🏭` rows |
| `Status`: Power supplied/demand/shortage | City HUD | `⚡ 120/160` |
| `Status`: Goods produced/imported/exported | City HUD | `📦 +18 made · 4 imp · 6 exp` |
| `Status`: Demand + Demand Notes | City HUD | `📈` colored `block_meter` bars |
| `Status`: right-aligned `Time:` | header bar | `☀ Day 12 · 09:00 ◐` |
| `Simulation: RUNNING/PAUSED` line | HUD title (`▶/⏸`) + Messages | kept in both |
| Map overlay header | Map title row | `Overlay: … · Theme: City` |
| Map column/row axis labels | unchanged | still shown |
| `Build / Actions` panel | unchanged layout, icon on Tool | keys B/R/U/X intact |
| Tool select keys `1-6` | left Tools strip (icons) | strip is a legend; `1-6` still select |
| `Messages / Tick Summary` | unchanged | bottom band, same keybind line |
| Inspect card | unchanged structure | gains zone-colored header |

### B. Overlay cycle (key `O`) — same map, recolored cells

```
Pollution overlay                          Traffic overlay (RESERVED — future)
╭ City Map ─ Overlay: Pollution ─╮          ╭ City Map ─ Overlay: Traffic ───╮
│  ⌂· ⌂· ▦· ╬▓ ╬█  (green→red)   │          │  ==·· ==▓▓ ==██  (green→red)    │
│  ··  ··  ╬▒ ╬▓ ϟ·              │          │  ··    no data yet (traffic     │
│  (dim non-source cells)        │          │   None until core fills it)     │
╰────────────────────────────────╯          ╰─────────────────────────────────╯
```

Overlay order (key `O`, unchanged): Normal → Power → Pollution → Population →
Land Value → Desirability → *(Traffic, reserved)* → Normal.

### C. Inspect variants (same fixed-slot card, zone-colored header)

```
Residential (3,9)                          Power Plant (7,7)
╭ Inspect: (3,9) ────────────────╮          ╭ Inspect: (7,7) ────────────────╮
│ RESIDENTIAL      Lvl █░░ 1     │          │ POWER PLANT      Lvl ██░ 2     │
│ ⚡on 🛣✓ 🔧2                    │          │ ⚡—  🛣✓ 🔧4                    │
│ People ▕██████░░░░▏ 6/10        │          │ Output ▕██████████▏ 120 cap    │
│ Happy  ▕███████▏╎ 72 (tgt 75)  │          │ (— holds the power slot in     │
│ Work   ✓4 local · 2 ◀ region 1 │          │  place; common slots stay put) │
│ Land▃ Poll▂ Acc▆ Desr▄         │          │ Land▄ Poll▇ Acc▆ Desr▂         │
╰────────────────────────────────╯          ╰────────────────────────────────╯
```

### D. Modals (keys `H`, `S`/`L`, `Esc`/`Q`) — restyled, same content

```
 Help (H)                                   Save / Load (S / L)
╭ Help ───────────────────────────╮         ╭ Save city ──────────────╮
│ Movement  WASD / Arrows         │         │ Save filename: city1    │
│ Build  1 ═Road 2 🏠R 3 🏪C       │         │ Enter confirms · Esc    │
│        4 🏭I  5 ⚡Pwr 6 🌳Park    │         ╰─────────────────────────╯
│ Actions B/Enter U R X · N next  │
│ Overlays O · Theme T · [ ] reg  │         Quit confirm (Esc / Q) — IMPLEMENTED
│ Files S save · L load           │         ╭ Confirm Quit ───────────────╮
│ Esc/Q quit (confirm) · ^C now   │         │     Quit Small City?        │
╰─────────────────────────────────╯         │ Unsaved progress will be lost│
                                             │ S Save&Quit  Q Quit  Esc … │
(modals keep behavior; emoji added          ╰─────────────────────────────╯
 to the build-tool list)                     (double-confirm; ^C = quit now)
```

These previews are **mockups, not output** — the build is what makes them real.
Every key in the current help screen still works; the redesign moves nothing
functional, it reskins.
