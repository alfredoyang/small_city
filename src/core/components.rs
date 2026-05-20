//! ECS component data used by buildings, citizens, power, pollution, and happiness systems.

use serde::{Deserialize, Serialize};

use crate::interface::input::BuildingKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Grid location for cell-occupying entities.
///
/// Only buildings currently receive `Position`; citizens are intentionally off-grid and store
/// their residential building in `Citizen::home`. Systems use `Position` to inspect neighbors,
/// compute local effects, connect roads/power, and render map cells.
pub struct Position {
    pub x: usize,
    pub y: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Building identity and upgrade level for entities placed on the grid.
///
/// `kind` determines which companion components are attached by placement: residential buildings
/// get `Population` and `PowerConsumer`, power plants get `PowerProvider`, industrial buildings
/// get `PollutionSource`, and parks get `HappinessEffect`. The upgrade system mutates `level` and
/// updates any dependent component values such as capacity or population limit.
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
/// Residential capacity cache for a housing building.
///
/// `max` is the building's housing capacity. `current` is kept in sync from citizen `home`
/// assignments so existing building views can still show population without exposing citizen
/// storage. Population growth creates citizen entities, then the citizen system refreshes this
/// cache from the number of citizens whose home points at the residential entity.
pub struct Population {
    pub current: i32,
    pub max: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Individual resident state for an off-grid citizen entity.
///
/// Citizens do not occupy map cells. Stable citizen-only state is grouped here to keep the custom
/// ECS simple: `home` links to a residential building, `workplace` may later link to a commercial
/// or industrial job site, and `happiness` is updated by the citizen system from home conditions.
/// Future movement/pathfinding state should remain in separate reusable components instead of
/// growing this record.
pub struct Citizen {
    #[serde(default)]
    pub age: u32,
    pub home: crate::core::entity::Entity,
    pub workplace: Option<crate::core::entity::Entity>,
    pub happiness: i32,
    #[serde(default)]
    pub money: i32,
    /// Set by the economy system when rent cannot be paid; happiness reads it as rent stress.
    #[serde(default)]
    pub rent_stress: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Power capacity supplied by a power plant entity.
///
/// The power system adds this capacity to any road network orthogonally adjacent to the provider.
/// Consumers connected to that powered road network draw from the shared capacity in deterministic
/// map order.
pub struct PowerProvider {
    #[serde(default = "default_power_capacity", alias = "radius")]
    pub capacity: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Power demand and current powered state for a consuming building.
///
/// Residential, commercial, and industrial buildings use this component. `demand` is the capacity
/// they require from their road network, and `powered` is reset/recomputed by the power system each
/// refresh before downstream systems read it for growth, jobs, income, and inspect output.
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
/// Pollution contribution from an industrial building.
///
/// The city pollution system sums these sources for global pollution, and the local effects system
/// uses industrial buildings to create nearby pollution pressure and land-value penalties.
pub struct PollutionSource {
    pub amount: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// City-wide happiness bonus from a park building.
///
/// Parks use this component for the global happiness effect. Local effects are still derived from
/// the park building's position and kind, so this component intentionally stores only the broad
/// happiness amount.
pub struct HappinessEffect {
    pub amount: i32,
}
