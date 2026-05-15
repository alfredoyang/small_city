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
    pub road_connected: Option<bool>,
}

/// Aggregate city numbers shown by status panels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CityStatusView {
    pub money: i32,
    pub turn: u32,
    pub population: i32,
    pub jobs: i32,
    pub unemployment: i32,
    pub pollution: i32,
    pub happiness: i32,
}

/// Build menu entry exposed to UI without requiring access to core systems or resources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildOptionView {
    pub kind: BuildingKind,
    pub label: String,
    pub cost: i32,
}

/// Result of inspecting one coordinate, including out-of-bounds information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectView {
    pub x: usize,
    pub y: usize,
    pub in_bounds: bool,
    pub cell: Option<CellView>,
    pub details: Option<InspectDetailsView>,
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
        road_connected: bool,
        population: i32,
        max_population: i32,
    },
    Commercial {
        powered: bool,
        road_connected: bool,
        jobs: i32,
    },
    Industrial {
        powered: bool,
        road_connected: bool,
        jobs: i32,
    },
    PowerPlant {
        road_connected: bool,
        power_radius: usize,
    },
    Park {
        road_connected: bool,
        happiness_effect: i32,
    },
    Unknown,
}
