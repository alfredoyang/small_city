//! UI-local city driver that selects the single-city or regional facade backend.
//!
//! Terminal frontends call this adapter for commands, snapshots, inspect data,
//! and save/load. The adapter keeps worker/runtime details out of UI modules
//! while preserving the default single-city `Game` path.

use std::fmt;
use std::path::Path;

use crate::core::game::{Game, GameError};
use crate::core::regional_game::{
    RegionalGame, RegionalGameError, RegionalGameSaveError, RegionalGameSaveFailure,
};
use crate::interface::events::{CommandResult, GameEventView};
use crate::interface::input::{BuildingKind, MapOverlayInput};
use crate::interface::view::{BuildPreviewView, GameView, InspectView};

const DEFAULT_MAP_WIDTH: usize = 20;
const DEFAULT_MAP_HEIGHT: usize = 15;

/// Launch mode selected by the binary before entering a terminal frontend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CityLaunchMode {
    SingleCity,
    RegionalSingleRegion,
}

/// UI-facing errors from selecting or driving a city backend.
#[derive(Debug)]
pub enum CityDriverError {
    Game(GameError),
    Regional(RegionalGameError),
    RegionalSave(RegionalGameSaveError),
    RegionalSaveFailure(RegionalGameSaveFailure),
    Unavailable(String),
}

impl fmt::Display for CityDriverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Game(error) => write!(formatter, "{error}"),
            Self::Regional(error) => write!(formatter, "Regional game error: {error:?}"),
            Self::RegionalSave(error) => write!(formatter, "{error}"),
            Self::RegionalSaveFailure(error) => write!(formatter, "{error}"),
            Self::Unavailable(message) => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for CityDriverError {}

impl From<GameError> for CityDriverError {
    fn from(error: GameError) -> Self {
        Self::Game(error)
    }
}

impl From<RegionalGameError> for CityDriverError {
    fn from(error: RegionalGameError) -> Self {
        Self::Regional(error)
    }
}

impl From<RegionalGameSaveError> for CityDriverError {
    fn from(error: RegionalGameSaveError) -> Self {
        Self::RegionalSave(error)
    }
}

/// Shared command/view surface used by ASCII and ratatui frontends.
#[derive(Debug)]
pub struct CityDriver {
    backend: CityBackend,
    last_view: GameView,
    read_error: Option<String>,
}

#[derive(Debug)]
enum CityBackend {
    SingleCity(Box<Game>),
    RegionalSingleRegion(Box<RegionalGame>),
    Unavailable {
        mode: CityLaunchMode,
        message: String,
    },
}

impl CityDriver {
    pub fn new(mode: CityLaunchMode) -> Result<Self, CityDriverError> {
        match mode {
            CityLaunchMode::SingleCity => Ok(Self::single_city()),
            CityLaunchMode::RegionalSingleRegion => Self::regional_single_region(),
        }
    }

    pub fn single_city() -> Self {
        let game = Box::<Game>::default();
        let last_view = game.view();
        Self {
            backend: CityBackend::SingleCity(game),
            last_view,
            read_error: None,
        }
    }

    pub fn single_city_with_size(width: usize, height: usize) -> Self {
        let game = Box::new(Game::new(width, height));
        let last_view = game.view();
        Self {
            backend: CityBackend::SingleCity(game),
            last_view,
            read_error: None,
        }
    }

    pub fn regional_single_region() -> Result<Self, CityDriverError> {
        let game = Box::new(RegionalGame::single_region(
            DEFAULT_MAP_WIDTH,
            DEFAULT_MAP_HEIGHT,
        )?);
        let last_view = game.selected_region_view()?;
        Ok(Self {
            backend: CityBackend::RegionalSingleRegion(game),
            last_view,
            read_error: None,
        })
    }

    pub fn view(&mut self) -> GameView {
        self.view_with_overlay(MapOverlayInput::Normal)
    }

    pub fn view_with_overlay(&mut self, overlay: MapOverlayInput) -> GameView {
        match &self.backend {
            CityBackend::SingleCity(game) => self.remember_view(game.view_with_overlay(overlay)),
            CityBackend::RegionalSingleRegion(game) => {
                match game.selected_region_view_with_overlay(overlay) {
                    Ok(view) => self.remember_view(view),
                    Err(error) => self.fallback_view(format!("Regional game error: {error:?}")),
                }
            }
            CityBackend::Unavailable { message, .. } => self.fallback_view(message.clone()),
        }
    }

    pub fn inspect(&mut self, x: usize, y: usize) -> InspectView {
        match &self.backend {
            CityBackend::SingleCity(game) => game.inspect(x, y),
            CityBackend::RegionalSingleRegion(game) => {
                game.inspect_selected_region(x, y).unwrap_or_else(|error| {
                    self.fallback_inspect(x, y, format!("Regional game error: {error:?}"))
                })
            }
            CityBackend::Unavailable { message, .. } => {
                self.fallback_inspect(x, y, message.clone())
            }
        }
    }

    pub fn preview_build(&mut self, x: usize, y: usize, kind: BuildingKind) -> BuildPreviewView {
        match &self.backend {
            CityBackend::SingleCity(game) => game.preview_build(x, y, kind),
            CityBackend::RegionalSingleRegion(game) => game
                .preview_build_selected_region(x, y, kind)
                .unwrap_or_else(|error| {
                    self.fallback_preview(kind, format!("Regional game error: {error:?}"))
                }),
            CityBackend::Unavailable { message, .. } => {
                self.fallback_preview(kind, message.clone())
            }
        }
    }

    pub fn build(&mut self, x: usize, y: usize, kind: BuildingKind) -> CommandResult {
        match &mut self.backend {
            CityBackend::SingleCity(game) => game.build(x, y, kind),
            CityBackend::RegionalSingleRegion(game) => game
                .build_selected_region(x, y, kind)
                .unwrap_or_else(command_failure),
            CityBackend::Unavailable { message, .. } => driver_failure(message),
        }
    }

    pub fn replace(&mut self, x: usize, y: usize, kind: BuildingKind) -> CommandResult {
        match &mut self.backend {
            CityBackend::SingleCity(game) => game.replace(x, y, kind),
            CityBackend::RegionalSingleRegion(game) => game
                .replace_selected_region(x, y, kind)
                .unwrap_or_else(command_failure),
            CityBackend::Unavailable { message, .. } => driver_failure(message),
        }
    }

    pub fn upgrade(&mut self, x: usize, y: usize) -> CommandResult {
        match &mut self.backend {
            CityBackend::SingleCity(game) => game.upgrade(x, y),
            CityBackend::RegionalSingleRegion(game) => game
                .upgrade_selected_region(x, y)
                .unwrap_or_else(command_failure),
            CityBackend::Unavailable { message, .. } => driver_failure(message),
        }
    }

    pub fn bulldoze(&mut self, x: usize, y: usize) -> CommandResult {
        match &mut self.backend {
            CityBackend::SingleCity(game) => game.bulldoze(x, y),
            CityBackend::RegionalSingleRegion(game) => game
                .bulldoze_selected_region(x, y)
                .unwrap_or_else(command_failure),
            CityBackend::Unavailable { message, .. } => driver_failure(message),
        }
    }

    pub fn tick(&mut self) -> CommandResult {
        match &mut self.backend {
            CityBackend::SingleCity(game) => game.tick(),
            CityBackend::RegionalSingleRegion(game) => {
                game.tick_selected_region().unwrap_or_else(command_failure)
            }
            CityBackend::Unavailable { message, .. } => driver_failure(message),
        }
    }

    pub fn save_to_file(&mut self, path: impl AsRef<Path>) -> Result<(), CityDriverError> {
        match &mut self.backend {
            CityBackend::SingleCity(game) => game.save_to_file(path).map_err(Into::into),
            CityBackend::RegionalSingleRegion(_) => self.save_regional_to_file(path),
            CityBackend::Unavailable { message, .. } => {
                Err(CityDriverError::Unavailable(message.clone()))
            }
        }
    }

    pub fn load_from_file(&mut self, path: impl AsRef<Path>) -> Result<(), CityDriverError> {
        match &self.backend {
            CityBackend::SingleCity(_) => {
                let game = Box::new(Game::load_from_file(path)?);
                let view = game.view();
                self.backend = CityBackend::SingleCity(game);
                self.remember_view(view);
                Ok(())
            }
            CityBackend::RegionalSingleRegion(_) => {
                let game = RegionalGame::load_from_file(path)?;
                let view = game.selected_region_view()?;
                self.backend = CityBackend::RegionalSingleRegion(Box::new(game));
                self.remember_view(view);
                Ok(())
            }
            CityBackend::Unavailable { mode, .. } => match mode {
                CityLaunchMode::SingleCity => {
                    let game = Box::new(Game::load_from_file(path)?);
                    let view = game.view();
                    self.backend = CityBackend::SingleCity(game);
                    self.remember_view(view);
                    Ok(())
                }
                CityLaunchMode::RegionalSingleRegion => {
                    let game = Box::new(RegionalGame::load_from_file(path)?);
                    let view = game.selected_region_view()?;
                    self.backend = CityBackend::RegionalSingleRegion(game);
                    self.remember_view(view);
                    Ok(())
                }
            },
        }
    }

    pub fn take_read_error_message(&mut self) -> Option<String> {
        self.read_error.take()
    }

    fn save_regional_to_file(&mut self, path: impl AsRef<Path>) -> Result<(), CityDriverError> {
        let current = std::mem::replace(
            &mut self.backend,
            CityBackend::Unavailable {
                mode: CityLaunchMode::RegionalSingleRegion,
                message: "Regional game save is in progress".to_string(),
            },
        );
        let game = match current {
            CityBackend::RegionalSingleRegion(game) => game,
            other => {
                self.backend = other;
                return Ok(());
            }
        };

        match game.save_to_file(path) {
            Ok(saved_game) => {
                self.backend = CityBackend::RegionalSingleRegion(Box::new(saved_game));
                Ok(())
            }
            Err(RegionalGameSaveFailure::Recoverable { game, error }) => {
                self.backend = CityBackend::RegionalSingleRegion(game);
                Err(CityDriverError::RegionalSave(error))
            }
            Err(error @ RegionalGameSaveFailure::Unrecoverable(_)) => {
                let message = format!("Regional game unavailable after save failure: {error}");
                self.backend = CityBackend::Unavailable {
                    mode: CityLaunchMode::RegionalSingleRegion,
                    message,
                };
                Err(CityDriverError::RegionalSaveFailure(error))
            }
        }
    }

    fn remember_view(&mut self, view: GameView) -> GameView {
        self.last_view = view.clone();
        self.read_error = None;
        view
    }

    fn fallback_view(&mut self, message: String) -> GameView {
        self.read_error = Some(message);
        self.last_view.clone()
    }

    fn fallback_inspect(&mut self, x: usize, y: usize, message: String) -> InspectView {
        self.read_error = Some(message.clone());
        InspectView {
            x,
            y,
            in_bounds: false,
            cell: None,
            details: None,
            local_effects: None,
            explanations: vec![message],
        }
    }

    fn fallback_preview(&mut self, kind: BuildingKind, message: String) -> BuildPreviewView {
        self.read_error = Some(message.clone());
        BuildPreviewView {
            kind,
            label: kind.label().to_string(),
            cost: kind.cost(),
            can_build: false,
            reason: Some(message),
            effects: Vec::new(),
        }
    }
}

fn command_failure(error: RegionalGameError) -> CommandResult {
    driver_failure(&format!("Regional game error: {error:?}"))
}

fn driver_failure(reason: &str) -> CommandResult {
    CommandResult::failure(GameEventView::BuildFailed {
        reason: reason.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_city_driver_uses_game_command_and_view_surface() {
        let mut driver = CityDriver::single_city_with_size(3, 3);

        let result = driver.build(1, 1, BuildingKind::Residential);
        let view = driver.view();

        assert!(result.success);
        assert_eq!(view.map.width, 3);
        assert_eq!(view.map.height, 3);
        assert_eq!(view.map.cells[4].building, Some(BuildingKind::Residential));
    }

    #[test]
    fn regional_driver_uses_facade_commands_and_snapshots() {
        let mut driver = CityDriver::regional_single_region().expect("regional UI driver");

        let result = driver.build(1, 1, BuildingKind::Residential);
        let inspect = driver.inspect(1, 1);
        let before_turn = driver.view().status.turn;
        let tick = driver.tick();
        let after_turn = driver.view_with_overlay(MapOverlayInput::Power).status.turn;

        assert!(result.success);
        assert_eq!(
            inspect.cell.expect("regional cell").building,
            Some(BuildingKind::Residential)
        );
        assert!(tick.success);
        assert_eq!(before_turn + 1, after_turn);
    }

    #[test]
    fn unavailable_backend_reuses_last_view_and_reports_read_error() {
        let mut driver = CityDriver::single_city_with_size(3, 3);
        let last_view = driver.view();
        driver.backend = CityBackend::Unavailable {
            mode: CityLaunchMode::RegionalSingleRegion,
            message: "regional worker stopped".to_string(),
        };

        let fallback_view = driver.view_with_overlay(MapOverlayInput::Power);
        let inspect = driver.inspect(1, 1);
        let preview = driver.preview_build(1, 1, BuildingKind::Residential);

        assert_eq!(fallback_view, last_view);
        assert!(inspect.explanations[0].contains("regional worker stopped"));
        assert_eq!(preview.reason.as_deref(), Some("regional worker stopped"));
        assert_eq!(
            driver.take_read_error_message().as_deref(),
            Some("regional worker stopped")
        );
    }

    #[test]
    fn unavailable_backend_rejects_commands_without_switching_to_single_city() {
        let mut driver = CityDriver::regional_single_region().expect("regional UI driver");
        driver.backend = CityBackend::Unavailable {
            mode: CityLaunchMode::RegionalSingleRegion,
            message: "regional game unavailable".to_string(),
        };

        let result = driver.build(1, 1, BuildingKind::Residential);
        let save_error = driver
            .save_to_file("/tmp/small_city_unavailable_backend_test.json")
            .expect_err("unavailable backend cannot save");

        assert!(!result.success);
        assert!(result.message().contains("regional game unavailable"));
        assert!(matches!(save_error, CityDriverError::Unavailable(_)));
        assert!(matches!(driver.backend, CityBackend::Unavailable { .. }));
    }
}
