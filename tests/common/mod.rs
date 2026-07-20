//! Test-only facade for exercising single-region behavior through `RegionalGame`.
//!
//! Patch 23 removes the production single-city facade. Many existing scenario
//! tests still read more clearly with compact single-region method names, so
//! this helper keeps that ergonomic surface in tests while delegating every
//! operation to the regional facade.

#![allow(dead_code)]

use std::cell::RefCell;
use std::path::Path;

use serde_json::{Map, Value, json};
use small_city::core::regional_game::{
    RegionalGame, RegionalGameSaveError, RegionalGameSaveFailure,
};
use small_city::core::regions::RegionId;
use small_city::interface::events::CommandResult;
use small_city::interface::input::{BuildingKind, MapOverlayInput};
use small_city::interface::view::{BuildPreviewView, GameView, InspectView};

const TEST_REGION_ID: RegionId = RegionId(1);
const DEFAULT_MAP_WIDTH: usize = 20;
const DEFAULT_MAP_HEIGHT: usize = 15;

pub type SingleRegionTestGameError = RegionalGameSaveError;

#[derive(Debug)]
pub struct SingleRegionTestGame {
    inner: RefCell<Option<RegionalGame>>,
}

impl SingleRegionTestGame {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            inner: RefCell::new(Some(
                RegionalGame::single_region(width, height).expect("single-region test game"),
            )),
        }
    }

    pub fn view(&self) -> GameView {
        self.with_game(|game| game.selected_region_view().expect("selected region view"))
    }

    pub fn view_with_overlay(&self, overlay: MapOverlayInput) -> GameView {
        self.with_game(|game| {
            game.selected_region_view_with_overlay(overlay)
                .expect("selected region overlay view")
        })
    }

    pub fn build(&mut self, x: usize, y: usize, kind: BuildingKind) -> CommandResult {
        self.with_game(|game| {
            game.build(TEST_REGION_ID, x, y, kind)
                .expect("build command")
        })
    }

    pub fn preview_build(&self, x: usize, y: usize, kind: BuildingKind) -> BuildPreviewView {
        self.with_game(|game| {
            game.preview_build(TEST_REGION_ID, x, y, kind)
                .expect("preview build command")
        })
    }

    pub fn bulldoze(&mut self, x: usize, y: usize) -> CommandResult {
        self.with_game(|game| {
            game.bulldoze(TEST_REGION_ID, x, y)
                .expect("bulldoze command")
        })
    }

    pub fn replace(&mut self, x: usize, y: usize, kind: BuildingKind) -> CommandResult {
        self.with_game(|game| {
            game.replace(TEST_REGION_ID, x, y, kind)
                .expect("replace command")
        })
    }

    pub fn upgrade(&mut self, x: usize, y: usize) -> CommandResult {
        self.with_game(|game| game.upgrade(TEST_REGION_ID, x, y).expect("upgrade command"))
    }

    pub fn tick(&mut self) -> CommandResult {
        self.with_game(|game| game.tick_region(TEST_REGION_ID).expect("tick command"))
    }

    pub fn advance(&mut self) -> Option<CommandResult> {
        self.with_game(|game| game.advance().expect("advance command"))
    }

    pub fn inspect(&self, x: usize, y: usize) -> InspectView {
        self.with_game(|game| {
            game.inspect_region(TEST_REGION_ID, x, y)
                .expect("inspect command")
        })
    }

    pub fn save_to_file(&self, path: impl AsRef<Path>) -> Result<(), SingleRegionTestGameError> {
        let mut guard = self.inner.borrow_mut();
        let game = guard.take().expect("test game exists");

        match game.save_to_file(path) {
            Ok(restarted) => {
                *guard = Some(restarted);
                Ok(())
            }
            Err(RegionalGameSaveFailure::Recoverable { game, error }) => {
                *guard = Some(*game);
                Err(error)
            }
            Err(RegionalGameSaveFailure::Unrecoverable(error)) => Err(error),
        }
    }

    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self, SingleRegionTestGameError> {
        Ok(Self {
            inner: RefCell::new(Some(RegionalGame::load_from_file(path)?)),
        })
    }

    fn with_game<T>(&self, run: impl FnOnce(&RegionalGame) -> T) -> T {
        let guard = self.inner.borrow();
        run(guard.as_ref().expect("test game exists"))
    }
}

impl Default for SingleRegionTestGame {
    fn default() -> Self {
        Self::new(DEFAULT_MAP_WIDTH, DEFAULT_MAP_HEIGHT)
    }
}

pub fn write_legacy_single_city_save(
    path: impl AsRef<Path>,
    width: usize,
    height: usize,
    buildings: &[(usize, usize, BuildingKind)],
) -> Result<(), Box<dyn std::error::Error>> {
    // This intentionally writes the retired v1 bare-World save shape, not the
    // current regional save format. Keep it pinned to that historical on-disk
    // contract so regional loading continues to prove legacy compatibility.
    let mut grid_cells = vec![Value::Null; width * height];
    let mut entities = Map::new();
    let mut positions = Map::new();
    let mut building_components = Map::new();
    let mut populations = Map::new();
    let mut power_providers = Map::new();
    let mut power_consumers = Map::new();
    let mut pollution_sources = Map::new();
    let mut happiness_effects = Map::new();
    let mut money = 100;

    for (entity_id, &(x, y, kind)) in buildings.iter().enumerate() {
        let id = entity_id.to_string();
        money -= kind.cost();
        grid_cells[y * width + x] = json!(entity_id);
        entities.insert(id.clone(), entity_record(kind));
        positions.insert(id.clone(), json!({ "x": x, "y": y }));
        building_components.insert(
            id.clone(),
            json!({
                "kind": kind,
                "level": 1,
                "data": building_data(kind)
            }),
        );

        match kind {
            BuildingKind::Residential => {
                populations.insert(id.clone(), json!({ "current": 0, "max": 5 }));
                power_consumers.insert(id, json!({ "powered": false, "demand": 1 }));
            }
            BuildingKind::Commercial => {
                power_consumers.insert(id, json!({ "powered": false, "demand": 2 }));
            }
            BuildingKind::Industrial => {
                power_consumers.insert(id.clone(), json!({ "powered": false, "demand": 3 }));
                pollution_sources.insert(id, json!({ "amount": 2 }));
            }
            BuildingKind::PowerPlant => {
                power_providers.insert(id, json!({ "capacity": 10 }));
            }
            BuildingKind::Park => {
                happiness_effects.insert(id, json!({ "amount": 3 }));
            }
            BuildingKind::Road => {}
        }
    }

    let save = json!({
        "next_entity_id": buildings.len() as u32,
        "entities": entities,
        "grid": {
            "width": width,
            "height": height,
            "cells": grid_cells,
        },
        "resources": {
            "money": money,
            "turn": 0,
            "time": { "total_hours": 0 },
        },
        "stats": {
            "population": 0,
            "jobs": 0,
            "unemployment": 0,
            "pollution": 0,
            "happiness": 50,
            "power": {
                "total_power_capacity": 0,
                "total_power_demand": 0,
                "total_power_supplied": 0,
                "total_power_shortage": 0,
            },
        },
        "local_effects": {
            "width": width,
            "height": height,
            "cells": vec![default_local_effect(); width * height],
        },
        "positions": positions,
        "buildings": building_components,
        "populations": populations,
        "citizens": {},
        "power_providers": power_providers,
        "power_consumers": power_consumers,
        "pollution_sources": pollution_sources,
        "happiness_effects": happiness_effects,
    });

    let file = std::fs::File::create(path)?;
    serde_json::to_writer_pretty(file, &save)?;
    Ok(())
}

fn entity_record(kind: BuildingKind) -> Value {
    json!({
        "kind": kind,
        "has_position": true,
        "has_population": matches!(kind, BuildingKind::Residential),
        "has_citizen": false,
        "has_power_provider": matches!(kind, BuildingKind::PowerPlant),
        "has_power_consumer": matches!(
            kind,
            BuildingKind::Residential | BuildingKind::Commercial | BuildingKind::Industrial
        ),
        "has_pollution_source": matches!(kind, BuildingKind::Industrial),
        "has_happiness_effect": matches!(kind, BuildingKind::Park),
    })
}

fn building_data(kind: BuildingKind) -> Value {
    match kind {
        BuildingKind::Commercial => json!({
            "Commercial": {
                "local_goods_stored": 0,
                "business": default_business_finance(),
            }
        }),
        BuildingKind::Industrial => json!({
            "Industrial": {
                "business": default_business_finance(),
            }
        }),
        BuildingKind::Road
        | BuildingKind::Residential
        | BuildingKind::PowerPlant
        | BuildingKind::Park => json!("None"),
    }
}

fn default_business_finance() -> Value {
    json!({
        "business_cash": 0,
        "lifetime_profit": 0,
        "days_profitable": 0,
        "last_period_profit": 0,
    })
}

fn default_local_effect() -> Value {
    json!({
        "land_value": 4,
        "pollution_pressure": 0,
        "accessibility": 0,
        "desirability": 4,
    })
}
