# Readable inspect view: fixed slots + visual encodings

Status: P1 and P2 implemented.

The inspect panel is hard to scan. This plan redesigns it as a fixed-slot card
that **graphs** values (bars, gauges, glyphs) instead of spelling them in a
wrapping pipe-line, with common fields locked to the same screen position for
every building type.

Key fact that keeps this cheap: everything we want to graph is **already typed**
in `InspectDetailsView` (`powered: bool`, `goods_stored/goods_capacity: i32`,
`population/max_population`, `business_cash` + `upgrade_threshold`, …). The
current renderer just stringifies it. So P1 is a UI-only re-render — **no change
to `core/` or the view models.**

---

## 1. Why the current view is hard to scan

- **One wrapping pipe-line** (`src/ui/ascii.rs::format_inspect`):
  `(12,4) Commercial | Powered: Yes | Demand: 2 | Road: Yes | Level: 1 |
  Maintenance: 3 | Sales Tax: 1 | Goods: 0/12 | Business: 0/50 recent 0 ready No
  | Jobs: 2`. It wraps differently per terminal width and per building type, so a
  field's screen position is never stable.
- **Per-type field order differs**, so "Powered" / "Road" / "Level" sit in
  different spots depending on what was clicked.
- **Prose notes** (`explanations`, e.g. "…unreachable by road", "growth is
  blocked because no jobs are available") force reading a sentence to extract a
  yes/no.

---

## 2. Design rules

1. **Fixed slots.** Every building renders the same top zones in the same rows.
   A field a type lacks shows `—` in place — the slot never moves.
2. **Encode, don't spell.** Ratios → fill bars; booleans → glyphs; bounded values
   → gauges. Read from the existing typed fields, never by parsing prose.
3. **Three fixed zones:** Header → Status strip (common) → Type detail (bars) →
   Local-effects footer (common).

---

## 3. Mockups (same skeleton, different middle)

```
Commercial                                Residential
┌ (12,4) COMMERCIAL          Lvl ██░ 2/3 ┐ ┌ (3,9) RESIDENTIAL        Lvl █░░ 1/3 ┐
│ ⚡ on    🛣 ✓    🔧 3    👷 2 jobs       │ │ ⚡ on    🛣 ✓    🔧 2                  │
│ Goods   ▕████░░░░░░░░▏  4/12            │ │ People  ▕██████░░░░▏  6/10            │
│ Cash    ▕██████░░░░░░▏ 30/50  ▲ soon    │ │ Happy   ▕███████▏╎    72  (tgt 75)    │
│ Sales   1/shopper                       │ │ Money   §  120/cit                   │
│ Supply  ◀ neighbor region               │ │ Work    ✓ 4 local · 2 ◀ region 1     │
├─────────────────────────────────────────┤ ├──────────────────────────────────────┤
│ Land ▆  Poll ▁  Access ▅  Desir ▅       │ │ Land ▃  Poll ▂  Access ▆  Desir ▄    │
└─────────────────────────────────────────┘ └──────────────────────────────────────┘
```

```
PowerPlant (no power-consumer slot → "—" holds the position)
┌ (7,7) POWER PLANT          Lvl ██░ 2/3 ┐
│ ⚡ —     🛣 ✓    🔧 4                    │   ← Power slot shows "—" but stays put
│ Output  ▕██████████▏  120 cap           │
├─────────────────────────────────────────┤
│ Land ▄  Poll ▇  Access ▆  Desir ▂       │
└─────────────────────────────────────────┘
```

Header row (coords · type · level gauge) and the status strip (Power · Road ·
Maint) are **the same layout for every type** — that is the "common fields stay
put" rule. The eye learns: power is always top-left, road always next, level
always top-right.

---

## 4. Field → visual encoding

| Field | Encoding |
|---|---|
| `powered` | `⚡ on` / `⚡ off` / `⚡ —` (n/a, e.g. PowerPlant) |
| `road_connected` | `🛣 ✓` / `🛣 ✗` |
| `upgrade_level` / max | `Lvl ██░ 2/3` gauge |
| `maintenance_cost` | `🔧 3` |
| `goods_stored/capacity`, `population/max`, power capacity | fill bar `▕████░░░░▏ n/max` |
| `business_cash` vs `upgrade_threshold` | fill bar + `▲ soon` when `upgrade_ready` |
| `average_happiness` vs `average_happiness_target` | gauge with a `╎` target tick |
| goods-route reachability (Part B) | `◀ neighbor region` / `local ✓` / `✗ none` glyph |
| local effects (land / poll / access / desir) | four mini block-bars `▁▂▃▄▅▆▇` |

Booleans become glyphs; bounded numbers become bars; the few genuinely free-form
notes collapse to a single `⚠`-chip line at the bottom instead of full prose.

---

## 5. Implementation plan (UI-only)

- **P1 — layout + bars from existing typed fields.** Fixed-slot inspect cards in
  both terminal frontends. The TUI renders the mockup-style Unicode glyphs and
  bars (`⚡`, `🛣`, `🔧`, `▕████░░▏`, `▁▂▃...`); the ASCII fallback renders the
  same slots with plain `[####....]`. **No change to `InspectView` /
  `InspectDetailsView` or anything in `core/`** — it reads the structured fields
  that already exist. This alone delivers the stable-slots + graphs win.
- **P2 — typed diagnostic chips (optional follow-up).** A few statuses still live
  only in `explanations: Vec<String>` (goods-route reachability, "growth blocked:
  no jobs"). To render those as glyphs without fragile string-matching, promote
  them from `String` to a small `enum InspectFlag` on the view model (adapter +
  view change). Implemented for no-jobs growth blockers and commercial
  goods-route reachability; chip-covered diagnostics are omitted from prose, and
  prose explanations remain for detailed notes that do not have typed flags.

---

## 6. Scope / guardrails

- **UI-only and deterministic.** No ECS access — renders from view models, per the
  architecture rule. P1 touches only `tui.rs` + `ascii.rs`.
- **Graceful degrade.** The ratatui TUI uses Unicode glyphs and block bars; the
  ASCII frontend keeps plain `[####....]` / `Y` / `N` output for bare terminals.
- **Alignment guard.** Keep a snapshot/golden test of the ASCII `format_inspect`
  output so the fixed-slot alignment cannot silently drift.
- **No prose parsing.** Bars and glyphs come from typed fields; never regex the
  `explanations` strings to drive layout.

---

## 7. P3 — commercial goods source (city-made vs outside-imported)

Status: planned (not yet implemented).

A commercial building's storage is filled by city industry (this region's or a
road-connected neighbor's — both count as "city"); when storage is empty, sales
are served from the abstract outside-city edge market. The inspect card shows
total goods stored but never the **source split of what the shop actually sold**.
This adds it.

### Locked display

A new fixed-slot `Source` row in the commercial card, between `Sales` and `Local`:

```
Source  ▕████▓▓░░░▏  🏭 6 · 🌍 2
```

- Split bar (`▕…▏`, width 10): `█` = city-made share, `▓` = outside-imported
  share, of the day's goods sold. Fill proportionally across the full width when
  there are sales; the all-`░` form is the empty state only:
  `Source  ▕░░░░░░░░░░▏  no sales today`. (Do not leave a partial `░` tail when
  sales exist — the tail in the picker mockup was just the empty look.)
- Counts: `🏭 N` city-made · `🌍 M` from outside.
- Per **day** (reset each economy settlement, exactly like `last_period_profit`).
- TUI color: `█` green, `▓` yellow.

### Data path

1. `src/core/systems/economy.rs` — in the sales loop of `run_with_goods_exports`
   (the site already doing `business_profit_from_sale = Some((shopping.commercial,
   …))`), tally per-commercial counts: city if `shopping.local_goods`, else
   outside. Reset to 0 for every productive commercial at the start of the
   settlement so a shop with no sales reads 0·0 (mirror `last_period_profit`).
2. `src/core/components.rs` — add to `BusinessFinance`:
   `last_period_goods_from_city: i32`, `last_period_goods_from_outside: i32`
   (both `#[serde(default)]`).
3. `src/interface/view.rs` — add to `InspectDetailsView::Commercial`:
   `goods_sold_from_city: i32`, `goods_sold_from_outside: i32`.
4. `src/interface/adapter.rs` — fill those from `BusinessFinance` in the
   commercial branch of `inspect_details` (same place it reads `recent_profit`).

### Rendering

5. `src/ui/ascii.rs` — add a `split_bar(city, outside, width)` helper
   (`█`/`▓`/`░`, `▕…▏` brackets) and the `Source` line to the Commercial arm of
   `inspect_card_lines`. This shared formatter feeds both the ratatui TUI and the
   bare-ASCII frontend, so emoji + block chars appear in both. Update the golden
   `inspect_card_layout_is_fixed_slot_aligned` test.
6. `src/ui/tui.rs` — color the `Source` bar segments (`█` green, `▓` yellow);
   leave the shared string plain.

### Tests

- Economy: a 2-region city where region A's industry stocks region B's shop, plus
  a shop forced to edge-import → assert that shop's `goods_sold_from_city > 0` /
  `goods_sold_from_outside > 0` respectively, and that per-shop `outside` counts
  sum to the city-wide `goods_imported_from_outside`.
- `split_bar` unit check: all-city → all `█`; all-outside → all `▓`; 0/0 → all
  `░`; a mixed case → expected fill.
- Golden card line updated.

### Constraints / split

- Display-only, deterministic, no balance change (it only counts sales already
  happening). UI reads view models only — no ECS access outside the adapter.
- ~8 files, over the 5-file guideline, so split:
  - **S1**: data + view model + adapter + `ascii.rs` text/bar + tests (emoji included).
  - **S2**: `tui.rs` color layer.
- Run `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test -q` after each patch.

### Width note

Emoji are double-width but sit at the line's end (nothing column-aligns to their
right), so they don't break the left label grid; a bare terminal shows tofu but
the counts stay readable. To guarantee bare-terminal cleanliness, swap the emoji
for ASCII markers in `ascii.rs` and keep emoji TUI-only — default is emoji in both.
