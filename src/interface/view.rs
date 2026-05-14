use crate::interface::input::BuildingKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameView {
    pub map: MapView,
    pub status: CityStatusView,
    pub build_options: Vec<BuildOptionView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapView {
    pub width: usize,
    pub height: usize,
    pub cells: Vec<CellView>,
}

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
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildOptionView {
    pub kind: BuildingKind,
    pub label: String,
    pub cost: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectView {
    pub x: usize,
    pub y: usize,
    pub in_bounds: bool,
    pub cell: Option<CellView>,
}
