# City-aware goods in the commercial inspect view

Status: Part A implemented; Part B implemented for commercial goods-supply inspect notes.

Follow-up to [cross-region-goods-transfer-plan.md](cross-region-goods-transfer-plan.md).
Now that goods cross regions inside one city (G1+G2), the commercial/industrial
**inspect notes still talk like each region is its own market**, which is wrong
and confusing. This plan fixes the wording, adds a city-wide produced-vs-imported
summary, and (in a second part) makes the goods-route note cross-region aware.

---

## 1. What's wrong (both seen in save `city1`)

1. **"Goods: nearest industrial route is unreachable by road"**
   (`src/interface/adapter.rs:410`) reads `access.goods_route_distance` from
   `road_network_analysis::access_for`, which runs on a **single `World`**. It
   cannot see a road-connected industrial in a neighbor region, so a shop whose
   only supplier is across a border wrongly reads "unreachable." This is the same
   local-only limitation already flagged for the residential commute note
   (`adapter.rs:269`, `TODO(cross-region display)`).
2. **"Goods: 0/12 local goods stored; imports are used when storage is empty"**
   (`adapter.rs:317`). Post-G2 that storage can be filled by a **neighbor
   region's** industrial (in-city, via the export grant). So "local goods" is
   misleading: it is really *city* goods, and "imports" should mean specifically
   **from outside the city** (the abstract edge market), not from a neighbor
   region.
3. **No visibility into city-produced vs outside-imported goods.** The data
   exists per region (`local_goods_produced`, `local_goods_sold` = city goods,
   `imported_goods_sold` = edge, `exported_goods` = edge) but nothing aggregates
   it city-wide or shows the split.

```
ONE CITY (regions)                       what the inspect note SHOULD say
──────────────────                       ────────────────────────────────
Region A industrial ─road─► Region B     "Goods: reachable via neighbor"
   produces city goods        shop           (not "unreachable by road")

Region B shop storage:                   "city goods stored"   (not "local")
  - filled by A (in-city)   ───────────►   sells as city goods
  - edge market (outside)   ───────────►   "from outside the city" only when empty
```

---

## 2. Recommended split

### Part A — wording + city aggregate (cheap; adapter / facade only)

No cross-region plumbing. Correct *because* G2 already routes neighbor supply
into storage and only `imported_goods_sold` comes from the edge.

- **Relabel** the commercial storage note (`adapter.rs:317`) and the industrial
  production note (`adapter.rs:335`): "local goods" → **"city goods"**, and make
  the import clause explicit, e.g.
  *"…stored; goods from outside the city are bought only when storage is empty."*
- **City-wide goods summary**: aggregate per-region tick economy in the
  `RegionalGame` facade —
  - `city_goods_produced       = Σ local_goods_produced`
  - `goods_imported_from_outside = Σ imported_goods_sold`
  - `goods_exported_outside     = Σ exported_goods`
  - (optional) `goods_traded_in_city = Σ exported_goods_units`

  Surface as a new view field and show it in a city / economy panel.
- **Tests:** 2-region city where region A's industrial supplies region B's shop —
  assert `city_goods_produced > 0`, `goods_imported_from_outside == 0` (nothing
  came from the edge), and the inspect note reads "city goods," not "local goods."

### Part B — cross-region reachability in inspect (harder; shared mission)

Make `goods_route_distance` (commercial→industrial) cross-region aware so the
"unreachable" note is correct when the supplier is across a border.

Implemented scope: commercial inspect now layers a display-only worker hint on
top of local `road_network_analysis`. If the shop has no local industrial route
but its local road network is in a cross-region component with published spare
city-goods supply, the note reads "reachable via neighbor region." Residential
commute/shop and industrial goods-demand notes still need their own published
summary data before they can be made cross-region aware without reading neighbor
worlds live.

- This needs the **cross-region road-component reachability** (the worker's
  discovery graph). The inspect path does not have it today:
  `RegionState::inspect` → `inspect_world(&self.world, …)` sees only one `World`.
- **This is the same plumbing the residential commute note (`adapter.rs:269`) is
  waiting on.** Do it once: thread a small, owned "border-reachable on my network
  component" hint into the inspect path — mirroring how `importable_remote_jobs`
  is computed in the worker from discovery and set on the region before inspect —
  then have `explain_road_access` consult it for Residential commute/shop *and*
  Commercial/Industrial goods routes.
- **Tests:** connected 2-region case → goods-route note shows a distance /
  "reachable via neighbor," not "unreachable"; disconnected control → still
  "unreachable."

---

## 3. Why split this way

Part A is a relabel plus a facade sum — small, no architectural change, fixes the
*misleading* part of issue 2 and delivers issue 3. Part B is the genuine
cross-region-display mission (touches the worker → inspect boundary) and is worth
bundling with the already-deferred commute note so the neighbor-reachability hint
is built once and consumed by all four route notes (commute, shopping, goods-in,
goods-out).

---

## 4. Risk / guardrail

- **Do not make `road_network_analysis::access_for` itself cross-region.** Keep it
  local and layer the neighbor-reachability hint on top in the adapter — the same
  way G2's growth gate layered `importable_remote_jobs` instead of rewriting local
  job resolution. That keeps determinism local and the cross-region read
  one-tick-stale (CLAUDE.md §3).
- Part A's facade aggregate is display-only; it must not feed back into any
  economy formula (no balance change).
