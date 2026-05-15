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

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) struct LocalEffectsMap {
    pub width: usize,
    pub height: usize,
    pub cells: Vec<LocalEffects>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LocalEffects {
    pub land_value: i32,
    pub pollution_pressure: i32,
    pub accessibility: i32,
    pub desirability: i32,
}

impl Default for LocalEffects {
    fn default() -> Self {
        Self {
            land_value: 4,
            pollution_pressure: 0,
            accessibility: 0,
            desirability: 4,
        }
    }
}

impl LocalEffectsMap {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            cells: vec![LocalEffects::default(); width * height],
        }
    }

    pub fn get(&self, x: usize, y: usize) -> LocalEffects {
        if x >= self.width || y >= self.height {
            return LocalEffects::default();
        }

        self.cells
            .get(y * self.width + x)
            .copied()
            .unwrap_or_default()
    }
}
