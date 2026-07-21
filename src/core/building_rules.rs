//! Tunable per-building rules (footprint area per upgrade level).
//!
//! These are game *rules*, not constants: a JSON ruleset is baked into the binary
//! (the embedded default) and an external `config/game_settings.json` may override it.
//! The active ruleset is meant to travel with a save so replays stay deterministic
//! (the save-stamping is wired where the rules are first read).
//!
//! Only the zoned buildings (Residential/Commercial/Industrial) have configurable
//! footprints; Road/Power/Park are always 1x1.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::interface::input::BuildingKind;

/// JSON ruleset baked into the binary; the floor the game always has.
const DEFAULT_JSON: &str = include_str!("buildings_default.json");

/// Every configurable zone must define a footprint area for each level up to the
/// building upgrade cap. Keep in step with `systems::upgrade::MAX_UPGRADE_LEVEL`
/// (the multi-cell plan goes to level 3).
const REQUIRED_LEVELS: usize = 3;

/// Per-zone tunables. Nested under a per-type object so capacity/cost can join later
/// without reshaping the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ZoneRules {
    /// Footprint area required at each level (index 0 = level 1). Positive and
    /// non-decreasing (an upgrade must never shrink a footprint).
    footprint_area_per_level: Vec<u32>,
    /// Industrial-only truck fleet size by level. Other zones may omit it.
    #[serde(default)]
    truck_count_per_level: Option<Vec<u16>>,
}

/// Tunable building rules, keyed by zone name ("Residential" / "Commercial" /
/// "Industrial"). String keys avoid enum-as-JSON-key pitfalls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildingRules {
    buildings: BTreeMap<String, ZoneRules>,
}

/// The zones that have configurable footprints. Order is fixed for deterministic validation.
const ZONES: [BuildingKind; 3] = [
    BuildingKind::Residential,
    BuildingKind::Commercial,
    BuildingKind::Industrial,
];

/// Stable JSON key for a configurable zone; `None` for fixed 1x1 buildings.
fn zone_key(kind: BuildingKind) -> Option<&'static str> {
    match kind {
        BuildingKind::Residential => Some("Residential"),
        BuildingKind::Commercial => Some("Commercial"),
        BuildingKind::Industrial => Some("Industrial"),
        BuildingKind::Road | BuildingKind::PowerPlant | BuildingKind::Park => None,
    }
}

impl Default for BuildingRules {
    fn default() -> Self {
        Self::embedded_default()
    }
}

impl BuildingRules {
    /// The ruleset baked into the binary. Guaranteed valid (a test enforces it).
    pub fn embedded_default() -> Self {
        serde_json::from_str(DEFAULT_JSON).expect("embedded buildings_default.json must be valid")
    }

    /// Loads the override at `path` if it exists, otherwise the embedded default.
    /// A present-but-malformed or invalid override fails loudly rather than silently
    /// falling back, so a typo in the config is never mistaken for the default.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_json(&text),
            // Only a genuinely-absent file falls back to the default; any other IO
            // error (permissions, etc.) is surfaced rather than silently ignored.
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Self::embedded_default()),
            Err(error) => Err(format!("reading {}: {error}", path.display())),
        }
    }

    /// Parses and validates a ruleset from JSON text.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let rules: BuildingRules = serde_json::from_str(text)
            .map_err(|error| format!("parsing building rules: {error}"))?;
        rules.validate()?;
        Ok(rules)
    }

    /// Footprint area required for `kind` at `level` (level is 1-based). Fixed-size
    /// buildings and levels beyond the table clamp sensibly to a single cell / the
    /// last entry.
    pub fn footprint_area(&self, kind: BuildingKind, level: u8) -> u32 {
        let Some(key) = zone_key(kind) else {
            return 1;
        };
        let Some(table) = self
            .buildings
            .get(key)
            .map(|zone| &zone.footprint_area_per_level)
            .filter(|table| !table.is_empty())
        else {
            return 1;
        };
        let index = (level.max(1) as usize - 1).min(table.len() - 1);
        table[index]
    }

    pub fn industrial_truck_count(&self, level: u8) -> u16 {
        let Some(table) = self
            .buildings
            .get("Industrial")
            .and_then(|zone| zone.truck_count_per_level.as_ref())
            .filter(|table| !table.is_empty())
        else {
            return default_industrial_truck_count(level);
        };
        let index = (level.max(1) as usize - 1).min(table.len() - 1);
        table[index]
    }

    /// Every configurable zone must have a non-empty, strictly-positive,
    /// non-decreasing area table.
    fn validate(&self) -> Result<(), String> {
        for kind in ZONES {
            let key = zone_key(kind).expect("ZONES entries are configurable");
            let zone = self
                .buildings
                .get(key)
                .ok_or_else(|| format!("missing footprint rules for {key}"))?;
            let table = &zone.footprint_area_per_level;
            if table.len() < REQUIRED_LEVELS {
                return Err(format!(
                    "{key} footprint_area_per_level needs at least {REQUIRED_LEVELS} levels, got {}",
                    table.len()
                ));
            }
            if table.contains(&0) {
                return Err(format!("{key} footprint area must be positive"));
            }
            if table.windows(2).any(|pair| pair[1] < pair[0]) {
                return Err(format!("{key} footprint area must be non-decreasing"));
            }
            // Level 1 is the single placed cell, and the growth algorithm extends one full side per
            // level, so each area must be reachable from the previous one in a single step
            // (e.g. 1 -> 2 -> 4 works; 1 -> 3 does not). Reject tables the algorithm cannot realize.
            if table[0] != 1 {
                return Err(format!("{key} level-1 footprint area must be 1"));
            }
            if let Some(pair) = table
                .windows(2)
                .find(|pair| pair[0] != pair[1] && !area_reachable_in_one_step(pair[0], pair[1]))
            {
                return Err(format!(
                    "{key} footprint area {} is not reachable from {} by extending one side",
                    pair[1], pair[0]
                ));
            }
            if kind == BuildingKind::Industrial
                && let Some(trucks) = &zone.truck_count_per_level
            {
                if trucks.len() < REQUIRED_LEVELS {
                    return Err(format!(
                        "{key} truck_count_per_level needs at least {REQUIRED_LEVELS} levels, got {}",
                        trucks.len()
                    ));
                }
                if trucks.contains(&0) {
                    return Err(format!("{key} truck count must be positive"));
                }
            }
        }
        Ok(())
    }
}

fn default_industrial_truck_count(level: u8) -> u16 {
    const DEFAULT: [u16; 3] = [1, 2, 4];
    let index = (level.max(1) as usize - 1).min(DEFAULT.len() - 1);
    DEFAULT[index]
}

/// Whether a rectangle of area `from` can become a rectangle of area `to` by extending one full
/// side. From a factor pair `(w, h)` of `from`, a single side-extension reaches `from + w` (a new
/// column) or `from + h` (a new row).
fn area_reachable_in_one_step(from: u32, to: u32) -> bool {
    if to <= from {
        return false;
    }
    (1..=from)
        .filter(|w| from.is_multiple_of(*w))
        .any(|w| to == from + w || to == from + from / w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_default_parses_and_validates() {
        let rules = BuildingRules::embedded_default();
        rules.validate().expect("embedded default must validate");
        // Matches docs/multi-cell-buildings-plan.md.
        assert_eq!(rules.footprint_area(BuildingKind::Residential, 1), 1);
        assert_eq!(rules.footprint_area(BuildingKind::Residential, 2), 2);
        assert_eq!(rules.footprint_area(BuildingKind::Residential, 3), 4);
        assert_eq!(rules.industrial_truck_count(1), 1);
        assert_eq!(rules.industrial_truck_count(2), 2);
        assert_eq!(rules.industrial_truck_count(3), 4);
    }

    #[test]
    fn fixed_buildings_and_overshoot_clamp_to_sensible_values() {
        let rules = BuildingRules::embedded_default();
        // Road/Power/Park are always one cell.
        assert_eq!(rules.footprint_area(BuildingKind::Road, 2), 1);
        assert_eq!(rules.footprint_area(BuildingKind::PowerPlant, 3), 1);
        // Level beyond the table clamps to the last entry; level 0 clamps up to level 1.
        assert_eq!(rules.footprint_area(BuildingKind::Commercial, 9), 4);
        assert_eq!(rules.footprint_area(BuildingKind::Commercial, 0), 1);
    }

    #[test]
    fn good_override_loads_from_disk() {
        let path = std::env::temp_dir().join("small_city_rules_ok.json");
        // A reachable override that differs from the default (1 -> 2 -> 3 instead of 1 -> 2 -> 4).
        std::fs::write(
            &path,
            r#"{"buildings":{"Residential":{"footprint_area_per_level":[1,2,3]},
                "Commercial":{"footprint_area_per_level":[1,2,4]},
                "Industrial":{"footprint_area_per_level":[1,2,4]}}}"#,
        )
        .unwrap();

        let rules = BuildingRules::load(&path).expect("good override loads");
        assert_eq!(rules.footprint_area(BuildingKind::Residential, 3), 3);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn missing_override_falls_back_to_default() {
        let path = std::env::temp_dir().join("small_city_rules_absent.json");
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            BuildingRules::load(&path).expect("absent override is fine"),
            BuildingRules::embedded_default()
        );
    }

    #[test]
    fn malformed_override_is_rejected() {
        assert!(BuildingRules::from_json("{ not json").is_err());
    }

    #[test]
    fn shrinking_or_zero_area_is_rejected() {
        let shrinking = r#"{"buildings":{"Residential":{"footprint_area_per_level":[1,4,2]},
            "Commercial":{"footprint_area_per_level":[1,2,4]},
            "Industrial":{"footprint_area_per_level":[1,2,4]}}}"#;
        assert!(BuildingRules::from_json(shrinking).is_err());

        let zero = r#"{"buildings":{"Residential":{"footprint_area_per_level":[0,1,2]},
            "Commercial":{"footprint_area_per_level":[1,2,4]},
            "Industrial":{"footprint_area_per_level":[1,2,4]}}}"#;
        assert!(BuildingRules::from_json(zero).is_err());
    }

    #[test]
    fn too_short_table_is_rejected() {
        // Must cover every upgrade level so growth never silently clamps.
        let short = r#"{"buildings":{"Residential":{"footprint_area_per_level":[1,2]},
            "Commercial":{"footprint_area_per_level":[1,2,4]},
            "Industrial":{"footprint_area_per_level":[1,2,4]}}}"#;
        assert!(BuildingRules::from_json(short).is_err());
    }

    #[test]
    fn invalid_industrial_truck_table_is_rejected() {
        let short = r#"{"buildings":{"Residential":{"footprint_area_per_level":[1,2,4]},
            "Commercial":{"footprint_area_per_level":[1,2,4]},
            "Industrial":{"footprint_area_per_level":[1,2,4],"truck_count_per_level":[1,2]}}}"#;
        assert!(BuildingRules::from_json(short).is_err());

        let zero = r#"{"buildings":{"Residential":{"footprint_area_per_level":[1,2,4]},
            "Commercial":{"footprint_area_per_level":[1,2,4]},
            "Industrial":{"footprint_area_per_level":[1,2,4],"truck_count_per_level":[1,0,4]}}}"#;
        assert!(BuildingRules::from_json(zero).is_err());
    }

    #[test]
    fn unreachable_area_step_is_rejected() {
        // 1 -> 3 cannot be reached by one side-extension (1x1 only grows to area 2 in one step).
        let unreachable = r#"{"buildings":{"Residential":{"footprint_area_per_level":[1,3,4]},
            "Commercial":{"footprint_area_per_level":[1,2,4]},
            "Industrial":{"footprint_area_per_level":[1,2,4]}}}"#;
        assert!(BuildingRules::from_json(unreachable).is_err());

        // From a 1x1 (area 1) one step reaches area 2; from area 2 (a 2x1) it reaches 3 or 4.
        assert!(area_reachable_in_one_step(1, 2));
        assert!(area_reachable_in_one_step(2, 3));
        assert!(area_reachable_in_one_step(2, 4));
        assert!(!area_reachable_in_one_step(1, 3));
        assert!(!area_reachable_in_one_step(2, 6));
    }

    #[test]
    fn missing_zone_is_rejected() {
        let no_industrial = r#"{"buildings":{"Residential":{"footprint_area_per_level":[1,2,4]},
            "Commercial":{"footprint_area_per_level":[1,2,4]}}}"#;
        assert!(BuildingRules::from_json(no_industrial).is_err());
    }
}
