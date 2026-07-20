//! UI-safe view models used by renderers instead of exposing ECS internals.

use serde::{Deserialize, Serialize};

use crate::core::city_refs::CityCellRef;
use crate::core::regions::RegionId;
use crate::interface::input::BuildingKind;

/// Complete read-only snapshot required to render the city UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameView {
    pub map: MapView,
    pub status: CityStatusView,
    pub build_options: Vec<BuildOptionView>,
    /// P4: road cells that currently hold a moving citizen (deduped, sorted).
    /// Pure derived data — renderers draw a dot here (the TUI only in the Normal
    /// overlay). No entity ids, paths, or graph leak through.
    pub travelers: Vec<CitizenTravelView>,
}

/// P4: a single moving-citizen marker — just the map cell it occupies this tick.
/// No identity, heading, or destination (P4 draws a plain dot; facing is deferred).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CitizenTravelView {
    pub x: usize,
    pub y: usize,
}

/// Map dimensions plus cells in row-major order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapView {
    pub width: usize,
    pub height: usize,
    pub cells: Vec<CellView>,
}

/// UI-safe description of one map cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellView {
    pub x: usize,
    pub y: usize,
    pub symbol: char,
    pub building: Option<BuildingKind>,
    pub label: String,
    pub buildable: bool,
    pub population: Option<i32>,
    pub max_population: Option<i32>,
    pub powered: Option<bool>,
    pub power_demand: Option<i32>,
    pub road_connected: Option<bool>,
    pub road_links: RoadLinks,
    pub upgrade_level: Option<u8>,
    pub job_assignments: Vec<JobAssignmentView>,
    pub local_effects: LocalEffectsView,
    /// `true` when this cell is the building's anchor (top-left) cell — always true
    /// for a 1x1 building. Renderers draw the building icon here and a continuation
    /// fill on the other footprint cells, so a multi-cell building reads as one lot.
    pub footprint_anchor: bool,
    /// Number of cells in this building's footprint (1, 2, or 4); 0 for empty cells.
    /// Drives the multi-cell continuation fill and the size-brightness tint.
    pub footprint_area: u8,
}

/// UI-safe workplace location for one employed resident.
///
/// Local ECS entity ids and remote producer slot ids are intentionally omitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobAssignmentView {
    /// The workplace cell, region-tagged (self-describing). `is_remote` is derived from
    /// whether `cell.region` is the inspected region.
    pub cell: CityCellRef,
    pub salary: i32,
    pub is_remote: bool,
}

/// One citizen's UI-safe detail for the roster popup.
///
/// Carries only display data; the ECS `Entity` id is intentionally omitted, like
/// every other view model. `happiness` is the citizen's current morale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CitizenDetailView {
    pub age: u32,
    pub happiness: i32,
    pub money: i32,
    /// Whether this assigned citizen has not reached work since the last daily
    /// settlement. Only meaningful for a residential roster entry with `WorksAt`.
    #[serde(default)]
    pub unpaid_since_daily_settlement: bool,
    pub relation: CitizenRelation,
}

/// How a rostered citizen relates to the inspected building.
///
/// ```text
///  inspected building   roster entry shows
///  ------------------   ------------------------------------
///  Residential          WorksAt{..} | Unemployed   (where the resident works)
///  Commercial/Industrial LivesAt{..}               (where the worker lives)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CitizenRelation {
    /// Residential roster: where this resident works (local or remote region).
    WorksAt {
        /// The workplace cell, region-tagged (self-describing).
        cell: CityCellRef,
        salary: i32,
        is_remote: bool,
    },
    /// Residential roster: a resident with no workplace.
    Unemployed,
    /// Workplace roster: where this worker lives.
    ///
    /// `region` is `None` when the worker lives in the inspected region itself (a
    /// local worker) and `Some(r)` for a remote commuter whose home is in region
    /// `r`. A local worker is reported as `None` (the view stays relative to the
    /// inspected region); the remote-worker reverse lookup at the `RegionState` layer
    /// fills in the home region for commuters. (`World` now records its own
    /// `region_id`, but this view deliberately keeps the local case relative.)
    LivesAt {
        region: Option<RegionId>,
        x: usize,
        y: usize,
    },
}

/// Orthogonal road neighbors for a road cell, exposed as derived UI-safe data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RoadLinks {
    pub north: bool,
    pub east: bool,
    pub south: bool,
    pub west: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LocalEffectsView {
    pub land_value: i32,
    pub pollution_pressure: i32,
    pub accessibility: i32,
    pub desirability: i32,
}

/// Aggregate city numbers shown by status panels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CityStatusView {
    pub money: i32,
    pub turn: u32,
    pub time: GameTimeView,
    pub population: i32,
    pub citizens: i32,
    pub jobs: i32,
    pub unemployment: i32,
    pub pollution: i32,
    pub happiness: i32,
    pub average_citizen_happiness: Option<i32>,
    pub average_citizen_happiness_target: Option<i32>,
    pub average_citizen_money: Option<i32>,
    pub demand: CityDemand,
    pub power: PowerStatusView,
    pub goods: CityGoodsView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CityGoodsView {
    pub city_goods_produced: i32,
    pub goods_imported_from_outside: i32,
    pub goods_exported_outside: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameTimeView {
    pub total_hours: u64,
    pub year: u32,
    pub month: u8,
    pub week: u8,
    pub day: u8,
    pub hour: u8,
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerStatusView {
    pub total_capacity: i32,
    pub total_demand: i32,
    pub total_supplied: i32,
    pub total_shortage: i32,
}

/// Simple zone demand levels derived from current city stats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DemandLevel {
    Low,
    Medium,
    High,
}

/// Residential, commercial, and industrial demand exposed through the UI boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CityDemand {
    pub residential: DemandLevel,
    pub commercial: DemandLevel,
    pub industrial: DemandLevel,
}

/// Build menu entry exposed to UI without requiring access to core systems or resources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildOptionView {
    pub kind: BuildingKind,
    pub label: String,
    pub cost: i32,
    pub maintenance_cost: i32,
}

/// UI-safe explanation of whether a build command would succeed at a coordinate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildPreviewView {
    pub kind: BuildingKind,
    pub label: String,
    pub cost: i32,
    pub can_build: bool,
    pub reason: Option<String>,
    pub effects: Vec<String>,
}

/// Result of inspecting one coordinate, including out-of-bounds information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectView {
    pub x: usize,
    pub y: usize,
    pub in_bounds: bool,
    pub cell: Option<CellView>,
    pub details: Option<InspectDetailsView>,
    pub local_effects: Option<LocalEffectsView>,
    pub flags: Vec<InspectFlag>,
    pub explanations: Vec<String>,
    /// Per-citizen roster for the inspected building: residents (Residential) or
    /// local workers (Commercial/Industrial). Empty for every other cell. Remote
    /// workers imported from another region are not listed (they live in their
    /// home region's world); residents holding a remote job are still listed.
    pub roster: Vec<CitizenDetailView>,
    /// Count of travel tokens (local residents and visiting bodies alike) whose
    /// `current_cell` is this road cell. Zero for non-road cells. Hover-only
    /// summary; per-traveler detail is a separate Enter-panel facade.
    pub road_traveler_count: usize,
}

/// Enter-panel detail for the travelers standing on one road cell. Built only on
/// demand (not part of the hover-cost `InspectView`) via a dedicated facade call.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RoadTravelerPanelSeedView {
    /// Full detail rows for travelers whose home is this region, in the same
    /// shape as a building roster.
    pub local_details: Vec<CitizenDetailView>,
    /// Endpoint summaries for visiting bodies whose home is elsewhere. Sorted
    /// and grouped: several visitors sharing the same home/work endpoint
    /// summarize as one row with `count > 1` rather than one row per traveler,
    /// so the group never silently loses how many travelers it represents.
    pub visitor_endpoints: Vec<RoadTravelerEndpointView>,
}

/// A visitor's origin/destination, known only from the fields already carried by
/// its `TravelToken`. `local_workplace` is populated only when the workplace is
/// in the inspected region; otherwise the visitor's exact remote position is
/// unknown without a cross-region query (out of scope for v1). `count` is the
/// number of visitors sharing this exact endpoint (always >= 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RoadTravelerEndpointView {
    pub home_region: RegionId,
    pub work_region: Option<RegionId>,
    pub local_workplace: Option<CityCellRef>,
    pub count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Typed inspect diagnostics rendered as compact chips by UI frontends.
pub enum InspectFlag {
    GrowthBlockedNoJobs,
    GoodsSupplyNeighbor,
    GoodsSupplyMissing,
}

/// Type-specific details for the inspected coordinate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InspectDetailsView {
    Empty {
        buildable: bool,
    },
    Road,
    Residential {
        powered: bool,
        power_demand: i32,
        road_connected: bool,
        upgrade_level: u8,
        maintenance_cost: i32,
        rent_per_citizen: i32,
        population: i32,
        max_population: i32,
        citizens: i32,
        average_happiness: Option<i32>,
        average_happiness_target: Option<i32>,
        average_money: Option<i32>,
        /// Assigned residents who have not reached their workplace since the
        /// last daily settlement.
        #[serde(default)]
        unpaid_citizens: i32,
        job_assignments: Vec<JobAssignmentView>,
    },
    Commercial {
        powered: bool,
        power_demand: i32,
        road_connected: bool,
        upgrade_level: u8,
        maintenance_cost: i32,
        sales_tax_per_shopper: i32,
        goods_stored: i32,
        goods_capacity: i32,
        business_cash: i32,
        upgrade_threshold: Option<i32>,
        recent_profit: i32,
        upgrade_ready: bool,
        jobs: i32,
        goods_sold_from_city: i32,
        goods_sold_from_outside: i32,
    },
    Industrial {
        powered: bool,
        power_demand: i32,
        road_connected: bool,
        upgrade_level: u8,
        maintenance_cost: i32,
        goods_production: i32,
        business_cash: i32,
        upgrade_threshold: Option<i32>,
        recent_profit: i32,
        upgrade_ready: bool,
        jobs: i32,
    },
    PowerPlant {
        road_connected: bool,
        connected_to_road_network: bool,
        upgrade_level: u8,
        maintenance_cost: i32,
        power_capacity: i32,
    },
    Park {
        road_connected: bool,
        upgrade_level: u8,
        maintenance_cost: i32,
        happiness_effect: i32,
    },
    Unknown,
}
