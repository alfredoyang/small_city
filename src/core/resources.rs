use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CityResources {
    pub money: i32,
    pub turn: u32,
}

impl Default for CityResources {
    fn default() -> Self {
        Self {
            money: 100,
            turn: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CityStats {
    pub population: i32,
    pub jobs: i32,
    pub unemployment: i32,
    pub pollution: i32,
    pub happiness: i32,
    #[serde(default)]
    pub power: PowerStats,
}

impl Default for CityStats {
    fn default() -> Self {
        Self {
            population: 0,
            jobs: 0,
            unemployment: 0,
            pollution: 0,
            happiness: 50,
            power: PowerStats::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PowerStats {
    pub total_power_capacity: i32,
    pub total_power_demand: i32,
    pub total_power_supplied: i32,
    pub total_power_shortage: i32,
}
