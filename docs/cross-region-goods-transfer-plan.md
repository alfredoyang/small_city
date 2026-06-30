# Cross-region goods transfer plan

Status: implemented — G1 + G2. Follow-ups: exact route distance on
`GoodsExportGrant`; goods-specific multi-worker parity test.

This mission lets a road-connected industrial in one region supply goods to a
shop in a neighboring region, instead of both falling back to the abstract edge
market. It mirrors the cross-region **jobs** patch; read
[regional-terminology.md](architecture/regional-terminology.md) §4/§4b first — the vocabulary
and the registry → discovery → producer-owned export-grant model are reused
verbatim.

> **Scope warning.** This is meaningfully bigger than the jobs patch. Goods carry
> money (manufacturing tax, sales tax, business profit), so an over-counted good
> is a real balance/determinism bug — not the cosmetic population overshoot the
> jobs hint can tolerate. That forces the full authoritative-grant path, which
> touches the `TickState` machine. Split into two patches (G1, G2) to stay under
> the 5-file / ~400-line rule.

---

## 1. Goal & current gap

Goods today are entirely intra-region:

- `economy::distribute_local_goods` and `nearest_commercials_for_goods`
  (`src/core/systems/economy.rs`) operate on a single `World`.
- Industrial surplus that finds no local shop becomes `exported_goods`
  (`economy.rs:412`) and is sold to the **abstract edge market** for export tax.
- An empty shop "imports" from that same abstract market at a higher citizen
  price (`imported_goods_sold`), never from a neighbor.

So an industrial in region A and a shop in region B connected by road never
trade. Goods are the last large item in the `regional_game.rs:8-10`
"not consumed yet" list (power ✅, jobs ✅, **goods ✗**, happiness ✗).

```
TODAY                                   TARGET
─────                                   ──────
Region A industrial                     Region A industrial
  surplus ──► [edge market] (tax)         surplus ──road──► Region B shop
                                                    (in-city: no border tax,
Region B empty shop                                  sold as a local good)
  fill   ◄── [edge market] (premium)     Region B shop
                                            fills from A's surplus first;
                                            edge market only as fallback
```

---

## 2. The mapping (what "demand" and "spare" mean for goods)

- **Producer side (spare)** — an industrial's `remaining_goods` after filling
  local commercial storage; the quantity that today becomes `exported_goods`.
- **Consumer side (demand)** — a productive commercial's free storage that would
  otherwise trigger `imported_goods_sold` from the edge.
- **Network-scoped** exactly like jobs/power: only shops and industrials on the
  same cross-region road component can trade.

---

## 3. Recommended approach: authoritative `GoodsExport`, mirroring jobs

Reuse the generic `ExportResource` trait (`src/core/regions/worker.rs:804`) — add
a third impl alongside `PowerExport` / `JobExport`.

- **New types in `regions/mod.rs`** (one-to-one with the jobs vocabulary):
  `PendingGoodsDemand`, `GoodsExportRequest`, `GoodsExportAllocationRequest`,
  `GoodsExportGrant`, `GoodsExportAllocation` (+ `GoodsExportAllocationKey`,
  `GoodsExportAllocationRelease`).
- **New `RegionEvent` variants:** `ProcessGoodsExportRequest`,
  `ApplyGoodsExportGrant`, `ReleaseGoodsExportAllocations`.
- **New availability-hint field** (like `spare_job_slot_ids`): publish per-network
  exportable goods as `spare_goods_units: u32`. A count is fine here — goods are
  fungible, so there is **no dedup-by-id** the way job slots needed.
- **Extend the `TickState` machine to a third sequential phase**
  (`src/core/regions/runtime/mod.rs`):

  ```
  Tick
   -- power demand --> WaitingForPowerExports
   -- (none) jobs   --> WaitingForJobExports
   -- (none) goods  --> WaitingForGoodsExports
   -- (none)        --> finish, Idle

  WaitingForPowerExports --(last grant)--> WaitingForJobExports
  WaitingForJobExports   --(last grant)--> WaitingForGoodsExports
  WaitingForGoodsExports --(last grant)--> finish tick, Idle
  ```

  Goods must run **after** jobs/economy basics, because the producer's local goods
  distribution decides `remaining_goods` — goods export operates on the surplus
  left over.

---

## 4. Two things that differ from jobs (not a blind rename)

1. **Divisible resource.** A citizen fills one whole slot; a shop can take *part*
   of an industrial's surplus. Model it like jobs models N citizens: one request
   per unit (or per shop-fill batch), so the existing "no partial grants" trait
   shape still holds — the producer reserves whole units against its surplus
   ledger. Do **not** extend the trait for partial grants.
2. **Inter-region goods are internal, not border trade — no import/export tax.**
   All regions together *are one city*. Goods crossing a region boundary never
   leave the city, so they incur **no `export_tax` and no edge-import premium**.
   They are treated exactly like **local goods** for the consuming shop: citizens
   pay the local price and get the full local-goods happiness bonus (`local_goods
   = true`), not the degraded `imported_goods_sold` path. Only goods that truly
   leave the city — surplus with no in-city shop, sold to the abstract edge market
   — pay `export_tax`; only a shop that no in-city industrial can fill pays the
   edge-import premium.

   The taxes that *do* apply to an inter-region transfer are the ordinary
   **domestic** ones, split by who did the activity:
   - manufacturing tax → **producer** region (it made the goods). Current G2 uses
     the producer's existing map-edge `import_export_distance` as the margin
     proxy because `GoodsExportGrant` does not yet carry the exact
     producer-to-consumer route distance. Add that route distance to the grant
     before claiming longer cross-region road runs are priced exactly.
   - sales tax → **consumer** region (it sold to its citizens).

   So an inter-region good is economically a *local good moved inside the city* —
   not a re-import. This is the goods analog of the jobs "economic ownership flows
   to producer" twist (§4b point 2), minus any border tax.

---

## 5. Determinism

One-tick-stale across regions (read last tick's published surplus/demand),
authoritative grant reserves units — same contract as jobs/power. Within-region
goods stay synchronous. The producer's `GoodsExportAllocation` ledger prevents two
consumer regions double-spending the same surplus. **The authoritative path is
mandatory here, not optional:** the jobs-style "guess and let the grant arbitrate"
is required because money is at stake — a stale-only hint that two importers both
act on would mint tax out of nothing.

---

## 6. Suggested split

- **Patch G1 — plumbing & visibility.** Add the types, the `spare_goods_units`
  hint, the `GoodsExport` `ExportResource` impl, and the `TickState` third phase —
  but the consumer step still hits the edge market. Tests assert requests / grants
  / releases route correctly and surplus is published. **No balance change yet.**
- **Patch G2 — economic hookup.** Make the consumer's empty-shop fill prefer a
  granted neighbor surplus over the edge, apply the split tax/price, and record
  profit on both sides.

---

## 7. Tests

- **2-region trade:** A has surplus industrial, B has an empty shop, road-connected
  across the border → B's shop fills from A, A's `exported_goods` (and its
  `export_tax`) to the edge drop, B's sale counts as a **local good** (no import
  premium), and B never records `imported_goods_sold` for those units.
- **Control (no link):** same layout with no road link → no transfer, both fall
  back to the edge market (numbers unchanged from today).
- **Contention:** two consumer regions competing for one producer's surplus →
  total grants never exceed the reserved surplus (the determinism guard).
- **Save/load + parity guard** unchanged (the surplus hint is transient like
  `importable_remote_jobs`). Goods currently rides the same generic worker
  routing path as power/jobs; a goods-specific multi-worker parity test remains
  useful follow-up coverage rather than a separate G2 behavior rule.

---

## 8. Risks

- **Scope:** realistically 6+ files and likely over 400 lines in one shot — hence
  the G1/G2 split.
- **Balance:** real inter-region trade changes city economics. Because in-city
  transfer pays no border tax and sells at the local price, it is strictly better
  than the edge market for both sides — so once roads connect regions, almost all
  surplus will route in-city and edge `export_tax` / import premium revenue
  drops. That is the intended behavior (one city), but re-check that total city
  income stays sane and that a pure-industrial region still profits from
  manufacturing tax alone.
- **Tick latency:** a third wait phase lengthens the worst-case tick continuation.
  The machine already handles two phases, so it is mechanical, but the
  FIFO / deferred-second-`Tick` logic needs the same care as the jobs phase.
