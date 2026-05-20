//! Global resources and derived city-wide data such as time, stats, power, and local effects.

use serde::{Deserialize, Serialize};

pub const HOURS_PER_DAY: u64 = 24;
pub const DAYS_PER_WEEK: u64 = 7;
pub const WEEKS_PER_MONTH: u64 = 4;
pub const MONTHS_PER_YEAR: u64 = 12;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CityResources {
    pub money: i32,
    pub turn: u32,
    #[serde(default)]
    pub time: GameTime,
}

impl Default for CityResources {
    fn default() -> Self {
        Self {
            money: 100,
            turn: 0,
            time: GameTime::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
/// Simulation calendar stored as total elapsed hours.
///
/// Calendar fields are derived instead of stored separately so save files cannot
/// drift into invalid dates. The game uses a simple city-builder calendar:
/// 24 hours per day, 7 days per week, 4 weeks per month, and 12 months per year.
pub struct GameTime {
    pub total_hours: u64,
}

impl GameTime {
    pub fn advance_hours(&mut self, hours: u64) {
        self.total_hours = self.total_hours.saturating_add(hours);
    }

    pub fn hour_of_day(self) -> u8 {
        (self.total_hours % HOURS_PER_DAY) as u8
    }

    pub fn day_of_week(self) -> u8 {
        ((self.total_hours / HOURS_PER_DAY) % DAYS_PER_WEEK) as u8 + 1
    }

    pub fn week_of_month(self) -> u8 {
        ((self.total_hours / (HOURS_PER_DAY * DAYS_PER_WEEK)) % WEEKS_PER_MONTH) as u8 + 1
    }

    pub fn month(self) -> u8 {
        ((self.total_hours / (HOURS_PER_DAY * DAYS_PER_WEEK * WEEKS_PER_MONTH)) % MONTHS_PER_YEAR)
            as u8
            + 1
    }

    pub fn year(self) -> u32 {
        (self.total_hours / (HOURS_PER_DAY * DAYS_PER_WEEK * WEEKS_PER_MONTH * MONTHS_PER_YEAR))
            as u32
            + 1
    }

    pub fn label(self) -> String {
        format!(
            "Year {}, Month {}, Week {}, Day {}, {:02}:00",
            self.year(),
            self.month(),
            self.week_of_month(),
            self.day_of_week(),
            self.hour_of_day()
        )
    }
}

pub fn is_new_day(before: GameTime, after: GameTime) -> bool {
    crossed_period(before.total_hours, after.total_hours, HOURS_PER_DAY)
}

pub fn is_new_week(before: GameTime, after: GameTime) -> bool {
    crossed_period(
        before.total_hours,
        after.total_hours,
        HOURS_PER_DAY * DAYS_PER_WEEK,
    )
}

fn crossed_period(before: u64, after: u64, period: u64) -> bool {
    before / period < after / period
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

#[cfg(test)]
mod tests {
    use super::{GameTime, is_new_day, is_new_week};

    #[test]
    fn game_time_derives_calendar_fields_from_total_hours() {
        let time = GameTime {
            total_hours: 24 * 8,
        };

        assert_eq!(time.hour_of_day(), 0);
        assert_eq!(time.day_of_week(), 2);
        assert_eq!(time.week_of_month(), 2);
        assert_eq!(time.month(), 1);
        assert_eq!(time.year(), 1);
        assert_eq!(time.label(), "Year 1, Month 1, Week 2, Day 2, 00:00");
    }

    #[test]
    fn cadence_helpers_detect_day_and_week_boundaries() {
        assert!(!is_new_day(
            GameTime { total_hours: 22 },
            GameTime { total_hours: 23 }
        ));
        assert!(is_new_day(
            GameTime { total_hours: 23 },
            GameTime { total_hours: 24 }
        ));
        assert!(is_new_week(
            GameTime {
                total_hours: 24 * 7 - 1
            },
            GameTime {
                total_hours: 24 * 7
            }
        ));
    }
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
