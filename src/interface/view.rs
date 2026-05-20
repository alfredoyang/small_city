//! UI-safe view models used by renderers instead of exposing ECS internals.

use crate::interface::input::BuildingKind;

/// Complete read-only snapshot required to render the city UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameView {
    pub map: MapView,
    pub status: CityStatusView,
    pub build_options: Vec<BuildOptionView>,
}

/// Map dimensions plus cells in row-major order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapView {
    pub width: usize,
    pub height: usize,
    pub cells: Vec<CellView>,
}

/// UI-safe description of one map cell.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    pub upgrade_level: Option<u8>,
    pub local_effects: LocalEffectsView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalEffectsView {
    pub land_value: i32,
    pub pollution_pressure: i32,
    pub accessibility: i32,
    pub desirability: i32,
}

/// Aggregate city numbers shown by status panels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CityStatusView {
    pub money: i32,
    pub turn: u32,
    pub population: i32,
    pub citizens: i32,
    pub jobs: i32,
    pub unemployment: i32,
    pub pollution: i32,
    pub happiness: i32,
    pub average_citizen_happiness: Option<i32>,
    pub average_citizen_money: Option<i32>,
    pub demand: CityDemand,
    pub power: PowerStatusView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerStatusView {
    pub total_capacity: i32,
    pub total_demand: i32,
    pub total_supplied: i32,
    pub total_shortage: i32,
}

/// Simple zone demand levels derived from current city stats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemandLevel {
    Low,
    Medium,
    High,
}

/// Residential, commercial, and industrial demand exposed through the UI boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CityDemand {
    pub residential: DemandLevel,
    pub commercial: DemandLevel,
    pub industrial: DemandLevel,
}

/// Build menu entry exposed to UI without requiring access to core systems or resources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildOptionView {
    pub kind: BuildingKind,
    pub label: String,
    pub cost: i32,
    pub maintenance_cost: i32,
}

/// UI-safe explanation of whether a build command would succeed at a coordinate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildPreviewView {
    pub kind: BuildingKind,
    pub label: String,
    pub cost: i32,
    pub can_build: bool,
    pub reason: Option<String>,
    pub effects: Vec<String>,
}

/// Result of inspecting one coordinate, including out-of-bounds information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectView {
    pub x: usize,
    pub y: usize,
    pub in_bounds: bool,
    pub cell: Option<CellView>,
    pub details: Option<InspectDetailsView>,
    pub local_effects: Option<LocalEffectsView>,
    pub explanations: Vec<String>,
}

/// Type-specific details for the inspected coordinate.
#[derive(Debug, Clone, PartialEq, Eq)]
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
        average_money: Option<i32>,
    },
    Commercial {
        powered: bool,
        power_demand: i32,
        road_connected: bool,
        maintenance_cost: i32,
        sales_tax_per_shopper: i32,
        goods_stored: i32,
        goods_capacity: i32,
        jobs: i32,
    },
    Industrial {
        powered: bool,
        power_demand: i32,
        road_connected: bool,
        maintenance_cost: i32,
        goods_production: i32,
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
