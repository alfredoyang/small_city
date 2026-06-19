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

- **P1 — layout + bars from existing typed fields.** Shared fixed-slot inspect
  card lines in `src/ui/ascii.rs`, rendered by both the ASCII fallback and TUI.
  Bars use plain `[####....]` cells in both frontends for one code path and
  predictable degradation. **No change to `InspectView` / `InspectDetailsView`
  or anything in `core/`** — it reads the structured fields that already exist.
  This alone delivers the stable-slots + graphs win.
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
- **Graceful degrade.** P1 uses plain ASCII bars in both frontends. Unicode block
  gauges can be added later as a TUI-only polish pass if the shared formatter is
  too plain.
- **Alignment guard.** Keep a snapshot/golden test of the ASCII `format_inspect`
  output so the fixed-slot alignment cannot silently drift.
- **No prose parsing.** Bars and glyphs come from typed fields; never regex the
  `explanations` strings to drive layout.
