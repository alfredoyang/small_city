# Tiny City Builder Economy Implementation Plan

## 1. Objective

Implement a simplified **multi-market CGE-lite economy model** where economic values are calculated from the current state of the city.

The model should connect:

- Citizens

- Commercial buildings

- Industrial buildings

- Residential buildings

- Wages

- Goods production

- Local and imported goods

- Product prices

- Rent

- Taxes

- Business profit

- Building upgrades

The system should calculate target economic values from current city development and gradually adjust actual values using smoothing.

---

## 2. Core Design Principles

### 2.1 Building level increases capacity, not guaranteed profit

A higher-level building should have:

- More employees

- More storage

- More production or sales capacity

- Higher operating costs

- Higher wage costs

- Higher rent

- Higher maintenance

- More potential tax liability

Profit should depend on actual city conditions.

A high-level building may still lose money when:

- There are not enough workers

- There are not enough customers

- Goods are too expensive

- Demand is too low

- Rent is too high

- Taxes are too high

- Storage is full

- Production exceeds commercial demand

### 2.2 Separate economic markets

The economy should contain several connected markets:

1. Commercial labor market

2. Industrial labor market

3. Local goods market

4. Imported goods market

5. Retail goods market

6. Housing and rent market

7. Tax and government revenue system

### 2.3 Simulation clock

The economy resolves **once per in-game day** (the daily tick), not every hourly
tick. Every smoothing alpha and every `*_inventory_ticks` constant in this
document is therefore expressed **per economy tick = per day**. At alpha 0.20 a
value reaches its target in ~10–15 days; size the constants with that horizon in
mind.

### 2.4 Target values with gradual adjustment (fixed-point)

Economic values should not instantly jump to equilibrium, and they are stored as
fixed-point **centi-units** (`value * 100`, see §2.5). Smoothing is integer-only
and takes alpha as an integer percent:

```rust
/// `alpha_pct` is hundredths (20 == 0.20). Operates on centi-unit values.
fn smooth_fp(old: i32, target: i32, alpha_pct: i32) -> i32 {
    let step = (target - old) * alpha_pct / 100;
    // Force at least one centi-unit of motion so small gaps cannot stall.
    // (The old f32 `smooth_i32(10, 11, 0.2)` rounded back to 10 forever.)
    let step = match (step, target.cmp(&old)) {
        (0, std::cmp::Ordering::Greater) => 1,
        (0, std::cmp::Ordering::Less) => -1,
        _ => step,
    };
    old + step
}
```

Recommended smoothing values (per day):

| Value                 | alpha_pct |
| --------------------- | --------- |
| Commercial wage       | 20        |
| Industrial wage       | 20        |
| Local wholesale price | 20        |
| Retail goods price    | 20        |
| Rent                  | 15        |
| Land value            | 10        |

### 2.5 Integer / fixed-point determinism

The simulation is integer-deterministic and guarded by single-worker vs
multi-worker parity tests, so the economy must be reproducible bit-for-bit:

- **No `f32`/`f64` at runtime.** The `f32` shown elsewhere in this document is
  illustrative; the implementation uses integers throughout.
- **Money, prices, wages, rents are stored ×100 (centi-units).** This keeps
  smoothing from stalling on single-digit values and lets margins divide cleanly.
- **Rates are integer percents** (`import_tax_pct: i32 = 15`, not `0.15`).
- **Per-level scales are pre-baked into integer lookup tables** (§6), never
  computed with `powf`/`powi` at runtime — those are not guaranteed reproducible.

### 2.6 Money conservation

City money is a closed system. Per economy tick the only money that enters or
leaves the city is:

- **Source:** export revenue (goods sold outside the city).
- **Sinks:** import payments, maintenance, and building upgrade construction
  (all paid outside the city).

Everything else — wages, **rent**, retail spending, and **all taxes** — is an
*internal transfer* between citizens, businesses, and the treasury and must
neither create nor destroy money. Rent goes to the **residential building's own
cash** (the building is the landlord — §12.6), so it stays inside the
"businesses cash" ledger and cancels; it never reaches citizens or the treasury.

Whole-system invariant, checked every tick:

```text
Δ(citizens cash + businesses cash + treasury)
    == net_exports − net_imports − maintenance − upgrade_construction
```

Equivalently, split by ledger (their sum is the line above; internal transfers
cancel):

```text
Δ treasury        ==  taxes − city_maintenance
Δ private sector  ==  net_exports − net_imports − taxes
                       − building_maintenance − upgrade_construction
```

A test asserts the whole-system equality each tick — the money analog of "the
economy does not produce goods from nothing," and it would have caught the §7.4
over-sale bug automatically.

---

## 3. Architecture Requirements

The economy logic must remain inside the simulation core.

The UI must not directly access:

- ECS world

- Components

- Systems

- Internal resources

- Entity storage

The UI should receive economy information through:

- `EconomyView`

- `GameView`

- `InspectView`

The existing clean core/UI boundary should remain protected.

---

## 4. Economy Balance Configuration

Add a central configuration resource.

```rust
pub struct EconomyBalanceConfig {
    pub target_utilization: f32,
    pub target_after_tax_margin: f32,

    pub commercial_inventory_ticks: i32,
    pub industrial_inventory_ticks: i32,

    pub upgrade_payback_ticks: i32,
    pub level_growth: f32,

    pub commercial_customer_buffer: f32,

    pub base_goods_per_citizen: i32,

    pub base_commercial_wage: i32,
    pub base_industrial_wage: i32,

    pub base_local_wholesale_price: i32,
    pub base_import_price: i32,
    pub import_logistics_modifier: f32,

    pub income_tax_rate: f32,
    pub sales_tax_rate: f32,
    pub business_tax_rate: f32,
    pub import_tax_rate: f32,
}
```

Recommended starting values:

```rust
EconomyBalanceConfig {
    target_utilization: 0.75,
    target_after_tax_margin: 0.12,

    commercial_inventory_ticks: 3,
    industrial_inventory_ticks: 4,

    upgrade_payback_ticks: 60,
    level_growth: 2.0,

    commercial_customer_buffer: 1.5,

    base_goods_per_citizen: 2,

    base_commercial_wage: 10,
    base_industrial_wage: 12,

    base_local_wholesale_price: 2,
    base_import_price: 3,
    import_logistics_modifier: 1.10,

    income_tax_rate: 0.05,
    sales_tax_rate: 0.05,
    business_tax_rate: 0.10,
    import_tax_rate: 0.15,
}
```

---

## 5. Economy State

Add a global economy resource.

```rust
pub struct EconomyState {
    // Labor market
    pub commercial_wage: i32,
    pub industrial_wage: i32,

    pub commercial_jobs: i32,
    pub industrial_jobs: i32,

    pub employed_commercial_workers: i32,
    pub employed_industrial_workers: i32,

    pub labor_supply: i32,
    pub unemployment_rate: f32,

    // Goods market
    pub local_wholesale_price: i32,
    pub import_wholesale_price: i32,
    pub retail_goods_price: i32,

    pub local_goods_supply: i32,
    pub imported_goods: i32,
    pub goods_demand: i32,

    // Housing market
    pub housing_capacity: i32,
    pub housing_pressure: f32,
    pub average_rent: i32,

    // Tax revenue
    pub income_tax_collected: i32,
    pub sales_tax_collected: i32,
    pub business_tax_collected: i32,
    pub import_tax_collected: i32,

    // Debug and balancing
    pub total_wage_income: i32,
    pub total_rent_paid: i32,
    pub total_shopping_spending: i32,
}
```

---

## 6. Level Scaling Formula

Use a common level scale:

```rust
let level_scale =
    config.level_growth.powi((level - 1) as i32);
```

Example with `level_growth = 2.0`:

| Level | Scale |
| ----- | ----- |
| 1     | 1.0   |
| 2     | 2.0   |
| 3     | 4.0   |

Not every value should grow at the same rate. Because levels are few (1..=3),
**bake every per-level scale into an integer table** instead of computing `powf`
at runtime (§2.5). Multipliers are stored ×100 (centi-units):

| Level | capacity (×1) | maintenance ×100 (≈scale^0.85) | rent ×100 (≈scale^0.55) | wage ×100 (1 + 0.15·(L−1)) |
| ----- | ------------- | ------------------------------ | ----------------------- | -------------------------- |
| 1     | 1             | 100                            | 100                     | 100                        |
| 2     | 2             | 180                            | 147                     | 115                        |
| 3     | 4             | 324                            | 216                     | 130                        |

```rust
// Indexed by (level - 1); precomputed at build time, no runtime powf.
const CAPACITY_SCALE:   [i32; 3] = [1, 2, 4];
const MAINTENANCE_X100: [i32; 3] = [100, 180, 324];
const RENT_X100:        [i32; 3] = [100, 147, 216];
const WAGE_X100:        [i32; 3] = [100, 115, 130];

let i = (level - 1) as usize;
let employee_capacity = base_employee_capacity * CAPACITY_SCALE[i];
let maintenance       = base_maintenance * MAINTENANCE_X100[i] / 100;
```

---

## 7. Commercial Building Model

### 7.1 Commercial level specification

```rust
pub struct CommercialLevelSpec {
    pub level: u8,

    pub employee_capacity: i32,
    pub goods_storage_capacity: i32,
    pub customer_capacity: i32,
    pub sales_per_employee: i32,

    pub wage_multiplier: f32,
    pub rent_multiplier: f32,
    pub maintenance_multiplier: f32,

    pub upgrade_cost: i32,
}
```

### 7.2 Commercial capacity formulas

```rust
let level_scale =
    config.level_growth.powi((level - 1) as i32);

let employee_capacity =
    (base_commercial_employees as f32 * level_scale).round() as i32;

let sales_per_employee =
    (base_sales_per_employee as f32
        * 1.15_f32.powi((level - 1) as i32))
        .round() as i32;

let peak_sales_per_tick =
    employee_capacity * sales_per_employee;

let customer_capacity =
    (peak_sales_per_tick as f32
        * config.commercial_customer_buffer)
        .round() as i32;

let goods_storage_capacity =
    peak_sales_per_tick * config.commercial_inventory_ticks;
```

Recommended base values:

```rust
let base_commercial_employees = 2;
let base_sales_per_employee = 4;
```

### 7.3 Commercial runtime state

```rust
pub struct CommercialState {
    pub cash: i32,

    pub workers_employed: i32,
    pub inventory: i32,

    pub customers_served: i32,
    pub goods_sold: i32,

    pub local_goods_bought: i32,
    pub imported_goods_bought: i32,
    pub blended_goods_cost: i32,

    pub revenue: i32,
    pub wage_cost: i32,
    pub rent_cost: i32,
    pub goods_cost: i32,
    pub maintenance_cost: i32,
    pub tax_paid: i32,

    pub pre_tax_profit: i32,
    pub profit: i32,

    pub lifetime_profit: i32,
    pub profitable_days: i32,
}
```

### 7.4 Commercial sales limit

Sales should be limited by:

- Citizen demand

- Customer capacity

- Available inventory

- Employee sales capacity

```rust
let employee_sales_capacity =
    commercial.workers_employed * level_spec.sales_per_employee;

// `affordable_goods_demand` (§13.0) is a citywide pool shared across commercial
// buildings, drawn in a deterministic building order. Including it in the limit
// is what ties sales to what citizens can actually pay for — without it the
// model would book revenue for goods no citizen could afford.
let goods_sold = [
    remaining_affordable_demand,
    level_spec.customer_capacity,
    commercial.inventory,
    employee_sales_capacity,
]
.into_iter()
.min()
.unwrap_or(0);

remaining_affordable_demand -= goods_sold;

// Revenue is exactly the money citizens hand over for those units.
let revenue = goods_sold * economy.retail_goods_price;
```

`remaining_affordable_demand` starts each tick at `affordable_goods_demand` and
is decremented as buildings sell, so total realized sales across the city can
never exceed citizens' combined budget.

### 7.5 Commercial goods purchasing

Commercial buildings buy local goods first.

```rust
let storage_space =
    level_spec.goods_storage_capacity - commercial.inventory;

let goods_to_buy =
    expected_customer_demand.min(storage_space.max(0));

let local_goods_bought =
    goods_to_buy.min(available_local_goods);

let imported_goods_bought =
    goods_to_buy - local_goods_bought;
```

Commercial inventory must never exceed storage capacity.

---

## 8. Industrial Building Model

### 8.1 Industrial level specification

```rust
pub struct IndustrialLevelSpec {
    pub level: u8,

    pub employee_capacity: i32,
    pub production_per_employee: i32,
    pub output_storage_capacity: i32,

    pub wage_multiplier: f32,
    pub rent_multiplier: f32,
    pub maintenance_multiplier: f32,

    pub upgrade_cost: i32,
}
```

### 8.2 Industrial capacity formulas

```rust
let level_scale =
    config.level_growth.powi((level - 1) as i32);

let employee_capacity =
    (base_industrial_employees as f32 * level_scale).round() as i32;

let production_per_employee =
    (base_production_per_employee as f32
        * 1.20_f32.powi((level - 1) as i32))
        .round() as i32;

let peak_output_per_tick =
    employee_capacity * production_per_employee;

let output_storage_capacity =
    peak_output_per_tick * config.industrial_inventory_ticks;
```

Recommended base values:

```rust
let base_industrial_employees = 3;
let base_production_per_employee = 3;
```

### 8.3 Industrial runtime state

```rust
pub struct IndustrialState {
    pub cash: i32,

    pub workers_employed: i32,
    pub inventory: i32,

    pub goods_produced: i32,
    pub goods_sold: i32,

    pub revenue: i32,
    pub wage_cost: i32,
    pub rent_cost: i32,
    pub input_cost: i32,
    pub maintenance_cost: i32,
    pub tax_paid: i32,

    pub pre_tax_profit: i32,
    pub profit: i32,

    pub lifetime_profit: i32,
    pub profitable_days: i32,
}
```

### 8.4 Industrial production

Production should be limited by:

- Workers employed

- Production per employee

- Remaining output storage

```rust
let raw_production =
    industrial.workers_employed
        * level_spec.production_per_employee;

let remaining_storage =
    level_spec.output_storage_capacity
        - industrial.inventory;

let actual_production =
    raw_production.min(remaining_storage.max(0));

industrial.inventory += actual_production;
industrial.goods_produced = actual_production;
```

Production should stop when output storage is full.

---

## 9. Labor Market

### 9.1 Separate wages

Maintain separate wages for:

- Commercial jobs

- Industrial jobs

```rust
let commercial_labor_pressure =
    commercial_jobs as f32
        / available_workers.max(1) as f32;

let industrial_labor_pressure =
    industrial_jobs as f32
        / available_workers.max(1) as f32;
```

### 9.2 Target wages

```rust
let target_commercial_wage =
    config.base_commercial_wage as f32
        * commercial_labor_pressure.clamp(0.75, 1.60);

let target_industrial_wage =
    config.base_industrial_wage as f32
        * industrial_labor_pressure.clamp(0.75, 1.70);
```

Apply smoothing:

```rust
economy.commercial_wage = smooth_i32(
    economy.commercial_wage,
    target_commercial_wage.round() as i32,
    0.20,
);

economy.industrial_wage = smooth_i32(
    economy.industrial_wage,
    target_industrial_wage.round() as i32,
    0.20,
);
```

### 9.3 Worker allocation

Allocate workers using job demand and wage attractiveness.

```rust
let commercial_attraction =
    commercial_jobs as f32
        * economy.commercial_wage as f32;

let industrial_attraction =
    industrial_jobs as f32
        * economy.industrial_wage as f32;

let total_attraction =
    commercial_attraction + industrial_attraction;
```

```rust
let employed_commercial_workers =
    if total_attraction > 0.0 {
        (available_workers as f32
            * commercial_attraction
            / total_attraction)
            .round() as i32
    } else {
        0
    };

let employed_industrial_workers =
    available_workers - employed_commercial_workers;
```

Each employment value must also be clamped to its available job count.

---

## 10. Local and Imported Goods

### 10.1 Local goods

Industrial buildings produce local goods.

Commercial buildings should purchase local goods before importing goods.

### 10.2 Imported goods price

The price a commercial building pays for an imported good is **tax-inclusive**:
the pre-tax landed cost (base price + logistics) plus the import tax. The city
later collects *exactly that same tax component* (§14.3) — it is never a second
charge on top of the tax-inclusive price.

```rust
// Centi-units; rates are integer percents (§2.5).
let pre_tax_landed_x100 =
    config.base_import_price_x100 * config.import_logistics_pct / 100;

let import_tax_per_unit_x100 =
    pre_tax_landed_x100 * config.import_tax_pct / 100;

let target_import_price_x100 =
    pre_tax_landed_x100 + import_tax_per_unit_x100;
```

### 10.3 Blended wholesale cost

```rust
let total_goods =
    local_goods_bought + imported_goods_bought;

let blended_goods_cost =
    if total_goods > 0 {
        ((local_goods_bought
            * economy.local_wholesale_price)
            + (imported_goods_bought
                * economy.import_wholesale_price))
            / total_goods
    } else {
        economy.local_wholesale_price
    };
```

A city with insufficient industry should therefore have:

- More imports

- Higher wholesale costs

- Higher retail prices

- Lower commercial profit

- Lower citizen purchasing power

---

## 11. Product Price Model

Prices should combine:

1. Production or purchase cost

2. Target business margin

3. Supply-demand pressure

4. Gradual smoothing

### 11.1 Target pre-tax margin

```rust
let target_pre_tax_margin =
    config.target_after_tax_margin
        / (1.0 - config.business_tax_rate);
```

### 11.2 Local wholesale equilibrium price

```rust
let unit_fixed_cost =
    (wage_cost + rent_cost + maintenance_cost) as f32
        / expected_goods_produced.max(1) as f32;

let unit_total_cost =
    input_cost_per_good as f32 + unit_fixed_cost;

let equilibrium_local_wholesale_price =
    unit_total_cost
        / (1.0 - target_pre_tax_margin);
```

Apply local supply-demand pressure:

```rust
let local_goods_pressure =
    commercial_goods_demand as f32
        / local_goods_supply.max(1) as f32;

let target_local_wholesale_price =
    equilibrium_local_wholesale_price
        * local_goods_pressure.clamp(0.75, 1.75);
```

### 11.3 Retail equilibrium price

```rust
let retail_unit_fixed_cost =
    (commercial_wage_cost
        + commercial_rent_cost
        + commercial_maintenance_cost) as f32
        / expected_goods_sold.max(1) as f32;

let retail_unit_total_cost =
    blended_wholesale_cost as f32
        + retail_unit_fixed_cost;

let equilibrium_retail_price =
    retail_unit_total_cost
        / (1.0 - target_pre_tax_margin);
```

Apply retail demand pressure:

```rust
let retail_pressure =
    citizen_goods_demand as f32
        / commercial_goods_available.max(1) as f32;

let target_retail_price =
    equilibrium_retail_price
        * retail_pressure.clamp(0.80, 1.60);
```

Smooth both prices before storing them.

---

## 12. Residential and Rent Model

### 12.1 Residential types

```rust
pub enum ResidentialType {
    LowDensity,
    MediumDensity,
    HighDensity,
}
```

Suggested initial values:

| Type           | Capacity | Base Rent |
| -------------- | -------- | --------- |
| Low density    | 4        | 4         |
| Medium density | 10       | 7         |
| High density   | 25       | 10        |

### 12.2 Residential economy state

```rust
pub struct ResidentialEconomy {
    pub house_type: ResidentialType,
    pub current_rent: i32,
    pub land_value: i32,
    pub disposable_income: i32,

    // The building is its own landlord: collected rent accrues here, the
    // building pays its own maintenance from it, and the surplus funds density
    // upgrades. Rent never flows to citizens or the treasury (§2.6, §12.6).
    pub cash: i32,
    pub rent_collected: i32,
    pub maintenance_cost: i32,
}
```

### 12.3 Land-value modifier

```rust
fn land_value_modifier(land_value: i32) -> f32 {
    0.6 + (land_value as f32 / 100.0) * 1.4
}
```

Result:

| Land value | Rent modifier |
| ---------- | ------------- |
| 0          | 0.6           |
| 50         | 1.3           |
| 100        | 2.0           |

### 12.4 Housing pressure

```rust
let housing_pressure =
    total_population as f32
        / total_housing_capacity.max(1) as f32;

let housing_pressure_modifier =
    housing_pressure.clamp(0.75, 1.80);
```

### 12.5 Per-building rent

```rust
let target_rent =
    base_house_rent as f32
        * land_value_modifier(land_value)
        * housing_pressure_modifier
        * service_modifier;
```

Smooth each building's rent (centi-units, §2.4):

```rust
residential.current_rent = smooth_fp(
    residential.current_rent,
    target_rent_x100,
    15,
);
```

### 12.6 Rent collection — the building is the landlord

Rent is paid by residents but received by the **residential building itself**,
mirroring how commercial and industrial buildings hold `cash`. It never flows to
citizens or the city treasury, so money stays conserved (§2.6) without a separate
landlord agent.

```rust
// Each occupied unit pays the smoothed per-unit rent, capped by the residents'
// after-tax income so rent is never paid on credit (consistent with §13.3,
// where disposable income = after-tax income − rent).
let rent_collected =
    (occupied_units * residential.current_rent).min(resident_after_tax_income);

residential.cash += rent_collected;                // citizen cash -> building cash
residential.cash -= residential.maintenance_cost;  // building cash -> outside (sink)
```

The surplus accumulates and funds **density upgrades** (low → medium → high)
using the same payback/eligibility rules as §16. Building upgrade construction is
money leaving the city, so treat `cash -= upgrade_cost` as a sink identically for
residential, commercial, and industrial buildings.

A residential building that cannot cover maintenance from rent runs its cash
down and stops qualifying for upgrades — the housing analog of an unprofitable
business.

---

## 13. Citizen Income and Tax

### 13.0 Citizen goods demand

`citizen_goods_demand` is the citywide *want* — a need quantity that exists
independent of price or income. It is the demand anchor the rest of the model
was missing:

```rust
let citizen_goods_demand =
    population * config.base_goods_per_citizen;
```

This is a demand **ceiling**, not realized sales. What citizens can actually buy
is capped by what they can afford, computed once disposable income is known
(tick-order Step 8):

```rust
let affordable_goods_demand =
    if economy.retail_goods_price > 0 {
        (disposable_income.max(0) / economy.retail_goods_price)
            .min(citizen_goods_demand)
    } else {
        citizen_goods_demand
    };
```

`affordable_goods_demand` is the shared pool commercial sales (§7.4) draw from,
so the model can never sell — or bill citizens for — goods they cannot pay for.
Integer division floors, so a citizen short of a full unit's price simply does
not buy that unit (deterministic, no fractional goods).

### 13.1 Wage income

```rust
let commercial_payroll =
    employed_commercial_workers
        * economy.commercial_wage;

let industrial_payroll =
    employed_industrial_workers
        * economy.industrial_wage;

let total_wage_income =
    commercial_payroll + industrial_payroll;
```

### 13.2 Income tax

Only employed citizens generate wage income.

```rust
let income_tax =
    (total_wage_income as f32
        * config.income_tax_rate)
        .round() as i32;
```

### 13.3 Disposable income

```rust
let after_tax_income =
    total_wage_income - income_tax;

let disposable_income =
    after_tax_income - total_rent_paid;
```

### 13.4 Shopping spending

Shopping spending is the money side of realized sales, **not** an independent
clamp. Because every sale was already capped by `affordable_goods_demand`
(§13.0), total spending is just `goods actually sold × retail price`, and it is
guaranteed `<= disposable_income`:

```rust
let shopping_spending =
    total_goods_sold * economy.retail_goods_price;

debug_assert!(shopping_spending <= disposable_income.max(0));
```

This makes the retail leg money-conserving: citizen spending equals the sum of
commercial `revenue`, each of which is `goods_sold * retail_goods_price`. No
goods are sold on credit and no revenue is created from nothing.

---

## 14. Tax Model

The city should collect:

- Citizen income tax

- Sales tax

- Commercial business tax

- Industrial business tax

- Import tax

### 14.1 Business tax

Business tax is paid only on positive pre-tax profit.

```rust
let business_tax =
    (pre_tax_profit.max(0) as f32
        * config.business_tax_rate)
        .round() as i32;
```

### 14.2 Sales tax

```rust
let sales_tax =
    (shopping_spending as f32
        * config.sales_tax_rate)
        .round() as i32;
```

### 14.3 Import tax

Collect exactly the tax already embedded in the import price (§10.2). The base is
the **pre-tax landed value** of the imported goods, so city revenue equals the
tax portion buyers paid — not a fresh charge on the tax-inclusive price:

```rust
let import_tax =
    imported_units * import_tax_per_unit_x100 / 100;
```

Invariant: `Σ import_tax collected == Σ import-tax component baked into §10.2
prices`.

### 14.4 City tax income

```rust
let total_tax_income =
    income_tax
        + sales_tax
        + commercial_business_tax
        + industrial_business_tax
        + import_tax;

city.money += total_tax_income;
```

City maintenance expenses should be subtracted separately.

---

## 15. Business Profit

### 15.1 Commercial profit

```rust
let pre_tax_profit =
    revenue
        - goods_cost
        - wage_cost
        - rent_cost
        - maintenance_cost;

let tax_paid =
    (pre_tax_profit.max(0) as f32
        * config.business_tax_rate)
        .round() as i32;

let profit =
    pre_tax_profit - tax_paid;
```

### 15.2 Industrial profit

```rust
let pre_tax_profit =
    revenue
        - input_cost
        - wage_cost
        - rent_cost
        - maintenance_cost;

let tax_paid =
    (pre_tax_profit.max(0) as f32
        * config.business_tax_rate)
        .round() as i32;

let profit =
    pre_tax_profit - tax_paid;
```

### 15.3 Business state update

```rust
business.cash += profit;

if profit > 0 {
    business.lifetime_profit += profit;
    business.profitable_days += 1;
} else {
    business.profitable_days = 0;
}
```

---

## 16. Upgrade Cost and Eligibility

### 16.1 Payback-based upgrade cost

Do **not** simulate the market at the next level to find
`next_level_expected_profit` — that is a nested solve. Estimate it cheaply by
scaling the building's *current realized* profit by the capacity ratio from the
pre-baked level table (§6):

```rust
// capacity scales from CAPACITY_SCALE (§6); ratio in centi-units.
let capacity_ratio_x100 =
    CAPACITY_SCALE[level as usize] * 100 / CAPACITY_SCALE[(level - 1) as usize];

let projected_next_profit =
    current_realized_profit * capacity_ratio_x100 / 100;

let expected_profit_gain =
    (projected_next_profit - current_realized_profit).max(0);

// Payback-based cost, floored to the level's build cost so a near-zero gain
// cannot make the upgrade almost free (the old `.max(1)` collapsed to a single
// tick of payback).
let upgrade_cost =
    (expected_profit_gain * config.upgrade_payback_ticks)
        .max(level_spec.upgrade_cost);
```

If current profit is ≤ 0 the gain is 0 and cost falls back to the level's build
cost; §16.2 already blocks upgrading an unprofitable building via
`profitable_days`, so this never lets a losing business upgrade cheaply.

### 16.2 Upgrade conditions

```rust
let can_upgrade =
    business.cash >= upgrade_cost
        && business.profitable_days >= required_profitable_days
        && local_happiness >= 60
        && power_available
        && enough_workers_available;
```

### 16.3 Upgrade execution

```rust
business.cash -= upgrade_cost;
building.level += 1;
```

After upgrading, the business is not guaranteed to remain profitable because:

- Wage costs increase

- Rent increases

- Maintenance increases

- More workers are required

- More goods or customers are required

---

## 17. Happiness Effects

Economic conditions should affect happiness gradually.

Negative factors:

- High unemployment

- High rent

- Housing shortage

- Goods shortage

- High retail prices

- Low disposable income

Positive factors:

- Available jobs

- Affordable rent

- Affordable goods

- Positive disposable income

- Good local services

Example:

```rust
let mut happiness_delta = 0;

if economy.unemployment_rate > 0.15 {
    happiness_delta -= 2;
}

if economy.housing_pressure > 1.0 {
    happiness_delta -= 1;
}

if economy.goods_demand
    > economy.local_goods_supply
        + economy.imported_goods
{
    happiness_delta -= 2;
}

if disposable_income > 0 {
    happiness_delta += 1;
}

happiness =
    (happiness + happiness_delta).clamp(0, 100);
```

---

## 18. Economy Tick Order

Use the following simulation order.

### Step 1: Collect city state

Collect:

- Population

- Working population

- Housing capacity

- Commercial jobs

- Industrial jobs

- Commercial inventories

- Industrial inventories

- Land values

- Service values

### Step 2: Build level specifications

Calculate:

- Commercial level capacities

- Industrial level capacities

- Residential type properties

### Step 3: Solve labor market

Calculate:

- Commercial wage

- Industrial wage

- Commercial employment

- Industrial employment

- Unemployment

### Step 4: Run industrial production

Calculate:

- Production per industrial building

- Output storage limits

- Local goods supply

### Step 5: Run commercial purchasing

Commercial buildings:

1. Calculate desired inventory

2. Buy local goods

3. Import remaining shortage

4. Calculate blended wholesale cost

### Step 6: Calculate goods prices

Update:

- Local wholesale price

- Import wholesale price

- Retail goods price

### Step 7: Calculate rent

For each residential building:

- Determine house type

- Read land value

- Apply housing pressure

- Apply service modifier

- Smooth current rent

- Collect rent into the building's own cash and pay its maintenance (§12.6)

### Step 8: Calculate citizen income

Calculate:

- Commercial payroll

- Industrial payroll

- Income tax

- Rent payments

- Disposable income

- Citizen goods demand and affordable goods demand (§13.0)

### Step 9: Run commercial sales

Sales are limited by:

- Affordable citizen demand (the §13.0 shared pool)

- Customer capacity

- Employee sales capacity

- Inventory

### Step 10: Calculate commercial accounting

Calculate:

- Revenue

- Goods cost

- Wage cost

- Rent

- Maintenance

- Business tax

- Final profit

### Step 11: Calculate industrial accounting

Calculate:

- Wholesale revenue

- Input cost

- Wage cost

- Rent

- Maintenance

- Business tax

- Final profit

### Step 12: Update city budget

Add:

- Income tax

- Sales tax

- Business tax

- Import tax

Subtract:

- Infrastructure maintenance

- Power maintenance

- Service maintenance

### Step 13: Update happiness and land value

Apply:

- Unemployment pressure

- Housing pressure

- Price pressure

- Disposable-income effects

- Service effects

- Pollution effects

### Step 14: Evaluate building upgrades

Check:

- Business cash

- Profitable days

- Expected profit gain

- Upgrade cost

- Worker availability

- Power

- Happiness

---

## 19. Inspection and Debug Views

### 19.1 Economy view

```text
Economy

Commercial wage: 10
Industrial wage: 12

Commercial workers: 8 / 10
Industrial workers: 12 / 15
Unemployment: 12%

Local goods: 80
Imported goods: 20

Local wholesale price: 2
Import wholesale price: 4
Retail price: 5

Average rent: 7
Housing pressure: 0.86

Income tax: 40
Sales tax: 15
Business tax: 12
Import tax: 6
```

### 19.2 Commercial building inspection

```text
Commercial Lv2

Workers: 3 / 4
Customers: 22 / 30
Inventory: 40 / 60
Goods sold: 18

Revenue: 90
Goods cost: -36
Wages: -30
Rent: -8
Maintenance: -5
Tax: -1

Profit: +10
Cash: 180
```

### 19.3 Industrial building inspection

```text
Industrial Lv2

Workers: 5 / 6
Production: 20
Inventory: 70 / 96
Goods sold: 18

Revenue: 80
Input cost: -10
Wages: -40
Rent: -8
Maintenance: -8
Tax: -1

Profit: +13
Cash: 120
```

### 19.4 Residential inspection

```text
Medium-Density Residential

Population: 8 / 10
Land value: 65
Rent: 9
Service modifier: 1.10
Housing pressure modifier: 1.05
Disposable income: 6
```

---

## 20. Testing Plan

### 20.1 Labor tests

- Commercial job pressure raises commercial wage.

- Industrial job pressure raises industrial wage.

- Worker allocation responds to wage and job demand.

- Unemployment rises when labor supply exceeds jobs.

- Employment never exceeds available jobs.

- Total employment never exceeds available workers.

### 20.2 Commercial capacity tests

- Higher commercial levels have more employees.

- Higher commercial levels have more storage.

- Higher commercial levels have more customer capacity.

- Commercial sales are limited by inventory.

- Commercial sales are limited by workers.

- Commercial sales are limited by customer capacity.

- Inventory never exceeds storage capacity.

### 20.3 Industrial capacity tests

- Higher industrial levels have more employees.

- Higher industrial levels have higher production rates.

- Production is limited by workers.

- Production is limited by output storage.

- Production stops when storage is full.

- Inventory never exceeds output storage.

### 20.4 Goods and import tests

- A city without industry imports goods.

- Local industry reduces imports.

- Imported goods increase blended wholesale cost.

- Higher import tax increases import price.

- More local supply lowers local supply pressure.

- Retail price responds to commercial shortage.

### 20.5 Rent tests

- Higher land value increases rent.

- Higher-density housing has more capacity.

- Higher-density housing has higher base rent.

- Housing shortage increases rent.

- Excess housing reduces rent pressure.

- Rent changes gradually.

### 20.6 Tax tests

- Employed citizens pay income tax.

- Unemployed citizens generate no wage tax.

- Shopping generates sales tax.

- Profitable commercial buildings pay business tax.

- Profitable industrial buildings pay business tax.

- Unprofitable businesses pay no business tax.

- Imported goods generate import tax.

### 20.7 Profit tests

- Commercial profit includes goods, wages, rent, maintenance, and tax.

- Industrial profit includes inputs, wages, rent, maintenance, and tax.

- Higher-level buildings have higher potential revenue.

- Higher-level buildings also have higher operating costs.

- Higher-level buildings are not automatically profitable.

### 20.8 Upgrade tests

- Profitable businesses accumulate cash.

- Unprofitable businesses reset profitable days.

- Upgrade cost is derived from expected profit gain.

- Businesses cannot upgrade without sufficient cash.

- Businesses cannot upgrade without enough profitable days.

- Businesses cannot upgrade without power.

- An upgrade increases capacity.

- An upgrade also increases costs.

### 20.9 Long-running simulation tests

Run the simulation for at least 100–500 ticks and verify:

- Wages stay positive.

- Prices stay positive.

- Rent stays positive.

- Unemployment remains between 0 and 1.

- Inventories remain within capacity.

- Happiness stays between 0 and 100.

- Money values do not overflow.

- Prices do not oscillate uncontrollably.

- The economy does not produce goods from nothing.

- Citizens never buy more goods than they can afford (total shopping spending
  never exceeds total disposable income — no sales on credit).

- Commercial revenue equals citizen shopping spending on the retail leg.

- Total money (citizens + businesses + treasury) changes each tick by exactly
  `net_exports − net_imports − maintenance − upgrade_construction`; no internal
  transfer (wage, rent, retail, tax) creates or destroys money (§2.6).

- Rent collected by a residential building equals rent paid by its residents
  (rent stays in the building's cash; none reaches citizens or the treasury).

- Tax revenue matches actual economic activity.

---

## 21. Implementation Phases

### Phase A: Foundation

Implement:

- `EconomyBalanceConfig`

- `EconomyState`

- `smooth_i32`

- `CommercialLevelSpec`

- `IndustrialLevelSpec`

- `ResidentialType`

- `ResidentialEconomy`

Do not change most gameplay behavior yet.

### Phase B: Labor Market

Implement:

- Separate commercial wage

- Separate industrial wage

- Worker allocation

- Unemployment

- Citizen income tax

### Phase C: Industrial Production

Implement:

- Employee capacity

- Production per employee

- Output storage

- Local goods inventory

- Production limits

### Phase D: Commercial Goods and Sales

Implement:

- Employee capacity

- Goods storage

- Customer capacity

- Sales per employee

- Local purchasing

- Imported goods

- Blended goods cost

- Sales limits

### Phase E: Goods Prices

Implement:

- Local wholesale equilibrium price

- Import price

- Retail equilibrium price

- Supply-demand pressure

- Price smoothing

### Phase F: Rent

Implement:

- Residential types

- House-type base rent

- Land-value modifier

- Housing pressure

- Service modifier

- Per-building rent

### Phase G: Profit and Tax

Implement:

- Commercial accounting

- Industrial accounting

- Income tax

- Sales tax

- Business tax

- Import tax

- City budget updates

### Phase H: Building Upgrades

Implement:

- Expected profit calculation

- Payback-based upgrade cost

- Upgrade eligibility

- Level capacity scaling

- Level cost scaling

### Phase I: Views and Tests

Implement:

- `EconomyView`

- Commercial inspection

- Industrial inspection

- Residential inspection

- Unit tests

- Integration tests

- Long-running stability tests

---

## 22. First Milestone

Target milestone:

```text
v0.3 — Economy Core
```

Include:

- Separate commercial and industrial wages

- Commercial employee, storage, and customer capacity

- Industrial employee, production, and storage capacity

- Local and imported goods

- Product price calculation

- Rent from house type and land value

- Citizen income tax

- Business tax

- Sales tax

- Import tax

- Business profit

- Basic upgrade rules

- Economy inspection view

- Simulation tests

Do not include yet:

- Individual citizen agents

- Multiple goods categories

- Traffic-based shopping

- Loans

- Inflation

- Bankruptcy

- Property ownership

- Banks

- Stock markets

- Advanced land speculation

---

## 23. Completion Criteria

The implementation is complete when:

- All economic values are derived from city state.

- Commercial and industrial wages are separate.

- Commercial sales respect all capacity limits.

- Industrial production respects worker and storage limits.

- Local goods are used before imports.

- Imported goods cost more than local goods under normal conditions.

- Rent depends on house type and land value.

- Citizens with jobs pay income tax.

- Profitable businesses pay business tax.

- Higher building levels increase both capacity and cost.

- Higher-level buildings are not automatically profitable.

- All important values are visible through inspection views.

- All tests pass.

- `cargo fmt` succeeds.

- `cargo test` succeeds.

- `cargo clippy -- -D warnings` succeeds.
