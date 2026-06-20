//! ECS component data used by buildings, citizens, power, pollution, and happiness systems.

use serde::{Deserialize, Serialize};

use crate::core::entity::Entity;
use crate::core::regions::RegionId;
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
/// A building's rectangular footprint, anchored at its `Position` (top-left corner).
///
/// Buildings start 1x1 and (from the multi-cell upgrade work) grow to a larger rectangle on
/// upgrade. The footprint cells are `anchor.x .. anchor.x + width` by `anchor.y .. anchor.y +
/// height`; every one of those grid cells maps back to the same building entity.
pub struct Footprint {
    pub width: u8,
    pub height: u8,
}

impl Footprint {
    /// The default single-cell footprint.
    pub const fn single() -> Self {
        Self {
            width: 1,
            height: 1,
        }
    }
}

impl Default for Footprint {
    fn default() -> Self {
        Self::single()
    }
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
    /// Kind-specific building state. Most buildings have no extra data; commercial buildings
    /// keep durable local goods inventory here so it stays attached to the building itself.
    #[serde(default)]
    pub data: BuildingData,
    /// Rectangular footprint anchored at this building's `Position`. Defaults to 1x1 so saves
    /// written before multi-cell buildings load unchanged.
    #[serde(default)]
    pub footprint: Footprint,
}

fn default_building_level() -> u8 {
    1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Kind-specific state stored directly on the owning building.
///
/// This avoids a separate component map for state that only makes sense for one building kind,
/// while leaving room for later variants such as warehouse, office, or civic building data.
pub enum BuildingData {
    None,
    /// Commercial inventory for goods made by local industrial buildings.
    ///
    /// Imported goods are not stored here yet. They are an on-demand fallback used by the economy
    /// system when a customer shops and this local inventory is empty. If imports later become
    /// delayed, limited, or trucked, this variant can grow an `imported_goods_stored` field.
    Commercial {
        local_goods_stored: i32,
        #[serde(default)]
        business: BusinessFinance,
    },
    /// Industrial business state for tracking private profit and reinvestment.
    Industrial {
        #[serde(default)]
        business: BusinessFinance,
    },
}

impl Default for BuildingData {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
/// Private business finance attached to revenue-generating buildings.
///
/// City money still tracks public taxes and fees. This finance record tracks the building owner's
/// retained profit so commercial and industrial buildings can later reinvest in automatic upgrades.
pub struct BusinessFinance {
    #[serde(default)]
    pub business_cash: i32,
    #[serde(default)]
    pub lifetime_profit: i32,
    #[serde(default)]
    pub days_profitable: i32,
    #[serde(default)]
    pub last_period_profit: i32,
    #[serde(default)]
    pub last_period_goods_from_city: i32,
    #[serde(default)]
    pub last_period_goods_from_outside: i32,
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
/// ECS simple: `home` links to a residential building, `workplace_assignment`
/// stores the current derived local-or-remote job, and `morale` keeps the DT2
/// derived/time happiness boundary in one nested value.
/// Future movement/pathfinding state should remain in separate reusable components instead of
/// growing this record.
pub struct Citizen {
    #[serde(default)]
    pub age: u32,
    pub home: crate::core::entity::Entity,
    /// Derived local-or-remote workplace assignment used by simulation and views.
    ///
    /// This is rebuilt by the daily job phase. It is skipped on save for the same
    /// reason as imported power/job state: assignments are derived from local
    /// buildings, citizens, road components, and producer export allocations.
    #[serde(default, skip)]
    pub workplace_assignment: Option<WorkplaceAssignment>,
    #[serde(default)]
    pub morale: Morale,
    #[serde(default)]
    pub money: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Citizen morale, organized by the DT2 derived/time boundary.
///
/// ```text
/// conditions (home, work, power, amenities, pollution) --derived--> target
/// target - decay - rent_stress                         --time-----> actual
/// ```
///
/// `target` intentionally stores the raw, unclamped condition score so actual
/// happiness can preserve the old single-clamp formula. Views clamp target for
/// display.
pub struct Morale {
    pub actual: i32,
    pub target: i32,
    pub decay: i32,
    pub rent_stress: i32,
}

impl Default for Morale {
    fn default() -> Self {
        Self {
            actual: 50,
            target: 50,
            decay: 0,
            rent_stress: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Owned job assignment that can describe either local or producer-exported work.
///
/// UI adapters convert this to a view-safe shape and never expose the local ECS
/// entity or the remote opaque slot id. Core simulation still keeps that source
/// identity so local salary/tax and producer-owned export allocation stay
/// deterministic.
pub struct WorkplaceAssignment {
    pub region: RegionId,
    pub position: Position,
    pub salary: i32,
    pub source: WorkplaceSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Internal source identity for a workplace assignment.
pub enum WorkplaceSource {
    Local { entity: Entity },
    Remote { slot_id: u32 },
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
/// Source that granted power to a consumer during the latest power resolution.
///
/// This is derived state recomputed by the power system. R1 records only local
/// providers; regional export grants add owned cross-region source IDs without
/// changing the local request/grant flow.
pub enum PowerSource {
    Local(Entity),
    Imported { source_region: RegionId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Power demand and current powered state for a consuming building.
///
/// Residential, commercial, and industrial buildings use this component. `demand` is the capacity
/// they require from their road network, and `powered` is reset/recomputed by the power system each
/// refresh before downstream systems read it for growth, jobs, income, and inspect output. `source`
/// records the derived grant source for registry-based local and future cross-region accounting.
pub struct PowerConsumer {
    #[serde(default)]
    pub powered: bool,
    #[serde(default = "default_power_demand")]
    pub demand: i32,
    #[serde(default, skip_serializing)]
    pub source: Option<PowerSource>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn building_without_footprint_field_defaults_to_single_cell() {
        // Saves written before multi-cell buildings have no footprint; they must load as 1x1.
        let building: Building = serde_json::from_str(r#"{"kind":"Residential"}"#)
            .expect("legacy building json deserializes");
        assert_eq!(building.footprint, Footprint::single());
        assert_eq!(building.level, 1);
    }
}
