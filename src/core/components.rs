use crate::interface::input::BuildingKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub x: usize,
    pub y: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Building {
    pub kind: BuildingKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Population {
    pub current: i32,
    pub max: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerProvider {
    pub radius: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerConsumer {
    pub powered: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollutionSource {
    pub amount: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HappinessEffect {
    pub amount: i32,
}
