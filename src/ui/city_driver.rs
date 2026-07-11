//! UI-local city driver for the regional facade backend.
//!
//! Terminal frontends call this adapter for commands, snapshots, inspect data,
//! and save/load. The adapter keeps worker/runtime details out of UI modules
//! while rendering only from interface view models.

use std::fmt;
use std::path::Path;

use crate::core::regional_game::{
    RegionalGame, RegionalGameError, RegionalGameSaveError, RegionalGameSaveFailure,
};
use crate::core::regions::{BorderEdge, RegionId};
use crate::interface::events::{CommandResult, GameEventView};
use crate::interface::input::{BuildingKind, MapOverlayInput};
use crate::interface::view::{
    BuildPreviewView, CitizenDetailView, GameView, InspectView, RoadTravelerPanelSeedView,
};

const DEFAULT_MAP_WIDTH: usize = 20;
const DEFAULT_MAP_HEIGHT: usize = 15;

/// UI-facing errors from selecting or driving a city backend.
#[derive(Debug)]
pub enum CityDriverError {
    Regional(RegionalGameError),
    RegionalSave(RegionalGameSaveError),
    RegionalSaveFailure(RegionalGameSaveFailure),
    Unavailable(String),
}

impl fmt::Display for CityDriverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Regional(error) => write!(formatter, "Regional game error: {error:?}"),
            Self::RegionalSave(error) => write!(formatter, "{error}"),
            Self::RegionalSaveFailure(error) => write!(formatter, "{error}"),
            Self::Unavailable(message) => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for CityDriverError {}

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
    RegionalMultiRegion(Box<RegionalGame>),
    Unavailable { message: String },
}

impl CityDriver {
    pub fn regional_multi_region() -> Result<Self, CityDriverError> {
        Self::regional_with_size(DEFAULT_MAP_WIDTH, DEFAULT_MAP_HEIGHT)
    }

    pub fn regional_with_size(width: usize, height: usize) -> Result<Self, CityDriverError> {
        let game = Box::new(RegionalGame::three_by_three_default(width, height)?);
        let last_view = game.selected_region_view()?;
        Ok(Self {
            backend: CityBackend::RegionalMultiRegion(game),
            last_view,
            read_error: None,
        })
    }

    pub fn select_next_region(&mut self) -> String {
        match &mut self.backend {
            CityBackend::RegionalMultiRegion(game) => game
                .select_next_region()
                .map(|_| self.region_label())
                .unwrap_or_else(|error| format!("Regional game error: {error:?}")),
            CityBackend::Unavailable { message, .. } => message.clone(),
        }
    }

    pub fn select_previous_region(&mut self) -> String {
        match &mut self.backend {
            CityBackend::RegionalMultiRegion(game) => game
                .select_previous_region()
                .map(|_| self.region_label())
                .unwrap_or_else(|error| format!("Regional game error: {error:?}")),
            CityBackend::Unavailable { message, .. } => message.clone(),
        }
    }

    /// Selects a specific region by id (used to jump to a remote citizen's region).
    /// Returns the new region label, or an error message if the id is unknown.
    pub fn select_region(&mut self, region_id: RegionId) -> String {
        match &mut self.backend {
            CityBackend::RegionalMultiRegion(game) => game
                .select_region_by_id(region_id)
                .map(|_| self.region_label())
                .unwrap_or_else(|error| format!("Regional game error: {error:?}")),
            CityBackend::Unavailable { message, .. } => message.clone(),
        }
    }

    pub fn region_label(&self) -> String {
        match &self.backend {
            CityBackend::RegionalMultiRegion(game) => match game.selected_region_position() {
                Ok((index, count)) => match game.selected_region() {
                    Ok(region_id) => format!("Region: {index}/{count} ({})", region_id.0),
                    Err(error) => format!("Region: unavailable ({error:?})"),
                },
                Err(error) => format!("Region: unavailable ({error:?})"),
            },
            CityBackend::Unavailable { message, .. } => {
                format!("Region: unavailable - {message}")
            }
        }
    }

    pub fn move_cursor_across_region(
        &mut self,
        x: usize,
        y: usize,
        dx: isize,
        dy: isize,
        view: &GameView,
    ) -> (usize, usize) {
        let max_x = view.map.width.saturating_sub(1);
        let max_y = view.map.height.saturating_sub(1);
        let crossing = match (dx, dy) {
            (-1, 0) if x == 0 => Some((BorderEdge::West, max_x, y)),
            (1, 0) if x == max_x => Some((BorderEdge::East, 0, y)),
            (0, -1) if y == 0 => Some((BorderEdge::North, x, max_y)),
            (0, 1) if y == max_y => Some((BorderEdge::South, x, 0)),
            _ => None,
        };

        if let Some((edge, next_x, next_y)) = crossing
            && self.select_neighbor(edge)
        {
            let neighbor_view = self.view();
            return (
                next_x.min(neighbor_view.map.width.saturating_sub(1)),
                next_y.min(neighbor_view.map.height.saturating_sub(1)),
            );
        }

        (
            x.saturating_add_signed(dx).min(max_x),
            y.saturating_add_signed(dy).min(max_y),
        )
    }

    pub fn view(&mut self) -> GameView {
        self.view_with_overlay(MapOverlayInput::Normal)
    }

    pub fn view_with_overlay(&mut self, overlay: MapOverlayInput) -> GameView {
        match &self.backend {
            CityBackend::RegionalMultiRegion(game) => {
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
            CityBackend::RegionalMultiRegion(game) => {
                game.inspect_selected_region(x, y).unwrap_or_else(|error| {
                    self.fallback_inspect(x, y, format!("Regional game error: {error:?}"))
                })
            }
            CityBackend::Unavailable { message, .. } => {
                self.fallback_inspect(x, y, message.clone())
            }
        }
    }

    /// Cross-region commuters staffing the workplace at `(x, y)` in the selected
    /// region. Empty on any backend error or when the cell has no remote workers,
    /// so the roster simply shows its local workers.
    pub fn remote_workers_at(&mut self, x: usize, y: usize) -> Vec<CitizenDetailView> {
        match &self.backend {
            CityBackend::RegionalMultiRegion(game) => game
                .remote_workers_at_selected_region(x, y)
                .unwrap_or_default(),
            CityBackend::Unavailable { .. } => Vec::new(),
        }
    }

    /// Enter-panel road-traveler detail at `(x, y)` in the selected region. Empty
    /// on any backend error (matches `remote_workers_at`'s fail-open behavior).
    pub fn road_traveler_panel_seed(&mut self, x: usize, y: usize) -> RoadTravelerPanelSeedView {
        match &self.backend {
            CityBackend::RegionalMultiRegion(game) => game
                .road_traveler_panel_seed_selected_region(x, y)
                .unwrap_or_default(),
            CityBackend::Unavailable { .. } => RoadTravelerPanelSeedView::default(),
        }
    }

    pub fn preview_build(&mut self, x: usize, y: usize, kind: BuildingKind) -> BuildPreviewView {
        match &self.backend {
            CityBackend::RegionalMultiRegion(game) => game
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
            CityBackend::RegionalMultiRegion(game) => game
                .build_selected_region(x, y, kind)
                .unwrap_or_else(command_failure),
            CityBackend::Unavailable { message, .. } => driver_failure(message),
        }
    }

    pub fn replace(&mut self, x: usize, y: usize, kind: BuildingKind) -> CommandResult {
        match &mut self.backend {
            CityBackend::RegionalMultiRegion(game) => game
                .replace_selected_region(x, y, kind)
                .unwrap_or_else(command_failure),
            CityBackend::Unavailable { message, .. } => driver_failure(message),
        }
    }

    pub fn upgrade(&mut self, x: usize, y: usize) -> CommandResult {
        match &mut self.backend {
            CityBackend::RegionalMultiRegion(game) => game
                .upgrade_selected_region(x, y)
                .unwrap_or_else(command_failure),
            CityBackend::Unavailable { message, .. } => driver_failure(message),
        }
    }

    pub fn bulldoze(&mut self, x: usize, y: usize) -> CommandResult {
        match &mut self.backend {
            CityBackend::RegionalMultiRegion(game) => game
                .bulldoze_selected_region(x, y)
                .unwrap_or_else(command_failure),
            CityBackend::Unavailable { message, .. } => driver_failure(message),
        }
    }

    pub fn tick(&mut self) -> CommandResult {
        match &mut self.backend {
            CityBackend::RegionalMultiRegion(game) => {
                // One tick press = one game hour = 6 movement sub-ticks (which also
                // runs the hourly economy on the first). `advance` keeps every region
                // on one clock and steps movement in lockstep; the economy result
                // (from sub-tick 0) is the selected region's status line.
                let mut economy = None;
                for _ in 0..6 {
                    match game.advance() {
                        Ok(Some(result)) => economy = Some(result),
                        Ok(None) => {}
                        Err(error) => return command_failure(error),
                    }
                }
                economy.unwrap_or_else(|| driver_failure("no economy result this hour"))
            }
            CityBackend::Unavailable { message, .. } => driver_failure(message),
        }
    }

    /// P7d: advance one 10-minute movement sub-tick (smooth cell-by-cell animation).
    /// Returns `Some` only when there is a message to surface — the hourly economy
    /// result (on the first sub-tick of each hour) or an error — and `None` on a
    /// movement-only sub-tick, so the caller leaves the status line as-is.
    pub fn advance(&mut self) -> Option<CommandResult> {
        match &mut self.backend {
            CityBackend::RegionalMultiRegion(game) => match game.advance() {
                Ok(result) => result,
                Err(error) => Some(command_failure(error)),
            },
            CityBackend::Unavailable { message, .. } => Some(driver_failure(message)),
        }
    }

    pub fn save_to_file(&mut self, path: impl AsRef<Path>) -> Result<(), CityDriverError> {
        match &mut self.backend {
            CityBackend::RegionalMultiRegion(_) => self.save_regional_to_file(path),
            CityBackend::Unavailable { message, .. } => {
                Err(CityDriverError::Unavailable(message.clone()))
            }
        }
    }

    pub fn load_from_file(&mut self, path: impl AsRef<Path>) -> Result<(), CityDriverError> {
        match &self.backend {
            CityBackend::RegionalMultiRegion(_) => {
                let game = RegionalGame::load_from_file(path)?;
                let view = game.selected_region_view()?;
                self.backend = CityBackend::RegionalMultiRegion(Box::new(game));
                self.remember_view(view);
                Ok(())
            }
            CityBackend::Unavailable { .. } => {
                let game = Box::new(RegionalGame::load_from_file(path)?);
                let view = game.selected_region_view()?;
                self.backend = CityBackend::RegionalMultiRegion(game);
                self.remember_view(view);
                Ok(())
            }
        }
    }

    pub fn take_read_error_message(&mut self) -> Option<String> {
        self.read_error.take()
    }

    fn save_regional_to_file(&mut self, path: impl AsRef<Path>) -> Result<(), CityDriverError> {
        let current = std::mem::replace(
            &mut self.backend,
            CityBackend::Unavailable {
                message: "Regional game save is in progress".to_string(),
            },
        );
        let game = match current {
            CityBackend::RegionalMultiRegion(game) => game,
            other => {
                self.backend = other;
                return Ok(());
            }
        };

        match game.save_to_file(path) {
            Ok(saved_game) => {
                self.backend = CityBackend::RegionalMultiRegion(Box::new(saved_game));
                Ok(())
            }
            Err(RegionalGameSaveFailure::Recoverable { game, error }) => {
                self.backend = CityBackend::RegionalMultiRegion(game);
                Err(CityDriverError::RegionalSave(error))
            }
            Err(error @ RegionalGameSaveFailure::Unrecoverable(_)) => {
                let message = format!("Regional game unavailable after save failure: {error}");
                self.backend = CityBackend::Unavailable { message };
                Err(CityDriverError::RegionalSaveFailure(error))
            }
        }
    }

    fn select_neighbor(&mut self, edge: BorderEdge) -> bool {
        let CityBackend::RegionalMultiRegion(game) = &mut self.backend else {
            return false;
        };
        let Ok(current) = game.selected_region() else {
            return false;
        };
        let Some(neighbor) = game.neighbor_region(current, edge) else {
            return false;
        };
        game.select_region_by_id(neighbor).is_ok()
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
            flags: Vec::new(),
            explanations: vec![message],
            roster: Vec::new(),
            road_traveler_count: 0,
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
    fn driver_uses_regional_facade_command_and_view_surface() {
        let mut driver = CityDriver::regional_with_size(3, 3).expect("regional UI driver");

        let result = driver.build(1, 1, BuildingKind::Residential);
        let view = driver.view();

        assert!(result.success);
        assert_eq!(view.map.width, 3);
        assert_eq!(view.map.height, 3);
        assert_eq!(view.map.cells[4].building, Some(BuildingKind::Residential));
        assert!(driver.region_label().contains("1/9"));
    }

    #[test]
    fn regional_driver_uses_facade_commands_and_snapshots() {
        let mut driver = CityDriver::regional_multi_region().expect("regional UI driver");

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
    fn cursor_crosses_east_edge_into_neighbor_region() {
        let mut driver = CityDriver::regional_with_size(3, 3).expect("regional UI driver");
        let view = driver.view();

        let cursor = driver.move_cursor_across_region(2, 1, 1, 0, &view);

        assert_eq!(cursor, (0, 1));
        assert!(driver.region_label().contains("2/9"));
    }

    #[test]
    fn cursor_crosses_west_edge_back_into_neighbor_region() {
        let mut driver = CityDriver::regional_with_size(3, 3).expect("regional UI driver");
        let view = driver.view();
        let _ = driver.move_cursor_across_region(2, 1, 1, 0, &view);
        let view = driver.view();

        let cursor = driver.move_cursor_across_region(0, 1, -1, 0, &view);

        assert_eq!(cursor, (2, 1));
        assert!(driver.region_label().contains("1/9"));
    }

    #[test]
    fn cursor_at_north_edge_without_neighbor_stays_clamped() {
        let mut driver = CityDriver::regional_with_size(3, 3).expect("regional UI driver");
        let view = driver.view();

        let cursor = driver.move_cursor_across_region(1, 0, 0, -1, &view);

        assert_eq!(cursor, (1, 0));
        assert!(driver.region_label().contains("1/9"));
    }

    #[test]
    fn unavailable_backend_reuses_last_view_and_reports_read_error() {
        let mut driver = CityDriver::regional_with_size(3, 3).expect("regional UI driver");
        let last_view = driver.view();
        driver.backend = CityBackend::Unavailable {
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
    fn unavailable_backend_rejects_commands_without_replacing_regional_backend() {
        let mut driver = CityDriver::regional_multi_region().expect("regional UI driver");
        driver.backend = CityBackend::Unavailable {
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
