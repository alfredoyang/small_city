use serde::{Deserialize, Serialize};

use crate::interface::input::BuildingKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub x: usize,
    pub y: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Building {
    pub kind: BuildingKind,
    /// Player-facing building level. New buildings start at level 1; missing save data is treated as level 1.
    #[serde(default = "default_building_level")]
    pub level: u8,
}

fn default_building_level() -> u8 {
    1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Population {
    pub current: i32,
    pub max: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Citizen {
    #[serde(default)]
    pub age: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Home {
    pub residential: crate::core::entity::Entity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Employment {
    pub workplace: Option<crate::core::entity::Entity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CitizenHappiness {
    pub value: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerProvider {
    #[serde(default = "default_power_capacity", alias = "radius")]
    pub capacity: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerConsumer {
    #[serde(default)]
    pub powered: bool,
    #[serde(default = "default_power_demand")]
    pub demand: i32,
}

fn default_power_capacity() -> i32 {
    10
}

fn default_power_demand() -> i32 {
    1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollutionSource {
    pub amount: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HappinessEffect {
    pub amount: i32,
}
