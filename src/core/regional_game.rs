//! UI-facing facade for threaded regional simulation.
//!
//! `RegionalGame` owns the regional execution runner and exposes only owned view
//! models, request/reply values, and deterministic errors. It does not expose
//! ECS `World` storage or require UI callers to talk directly to workers or
//! runtimes.
//!
//! Patch 16 cross-region resources are currently a visibility surface: regions
//! can see rebuildable imported-resource summaries from neighbors, but economy,
//! jobs, happiness, and other local systems do not consume those imports yet.

use std::fmt;
use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::core::regional_game_runner::{
    RecoveredRegionalGame, RegionalGameRunner, RegionalGameRunnerError,
};
pub use crate::core::regional_types::{
    RegionCommand, RegionCommandReply, RegionViewSnapshot, RegionalGameView, UiReply, UiRequest,
    UiRequestId,
};
use crate::core::regions::{RegionId, RegionState, RegionStateSaveRecord};
use crate::interface::events::CommandResult;
use crate::interface::input::{BuildingKind, MapOverlayInput};
use crate::interface::view::{BuildPreviewView, GameView, InspectView};

const DEFAULT_SINGLE_REGION_ID: RegionId = RegionId(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Deterministic errors returned by regional facade operations.
pub enum RegionalGameError {
    DuplicateRegion {
        region_id: RegionId,
    },
    UnknownRegion {
        region_id: RegionId,
    },
    SnapshotReplyMissing {
        request_id: UiRequestId,
        region_id: RegionId,
    },
    CommandReplyMissing {
        request_id: UiRequestId,
        region_id: RegionId,
    },
    TickReplyMissing {
        request_id: UiRequestId,
        region_id: RegionId,
    },
    CommandReplyTypeMismatch {
        request_id: UiRequestId,
        region_id: RegionId,
    },
    NoSelectedRegion,
    WorkerRoutingFailed,
    RegionAttachFailed,
    WorkerStopped,
    WorkerPanicked,
}

#[derive(Debug)]
/// File and format errors returned by regional save/load operations.
pub enum RegionalGameSaveError {
    Io(std::io::Error),
    SaveFormat(serde_json::Error),
    Regional(RegionalGameError),
}

impl fmt::Display for RegionalGameSaveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "File error: {error}"),
            Self::SaveFormat(error) => write!(formatter, "Save file error: {error}"),
            Self::Regional(error) => write!(formatter, "Regional game error: {error:?}"),
        }
    }
}

impl std::error::Error for RegionalGameSaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::SaveFormat(error) => Some(error),
            Self::Regional(_) => None,
        }
    }
}

impl From<std::io::Error> for RegionalGameSaveError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for RegionalGameSaveError {
    fn from(error: serde_json::Error) -> Self {
        Self::SaveFormat(error)
    }
}

impl From<RegionalGameError> for RegionalGameSaveError {
    fn from(error: RegionalGameError) -> Self {
        Self::Regional(error)
    }
}

#[derive(Debug)]
/// Save failures that may return a restarted game to preserve progress.
pub enum RegionalGameSaveFailure {
    Recoverable {
        game: Box<RegionalGame>,
        error: RegionalGameSaveError,
    },
    Unrecoverable(RegionalGameSaveError),
}

impl fmt::Display for RegionalGameSaveFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Recoverable { error, .. } => write!(formatter, "{error}"),
            Self::Unrecoverable(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for RegionalGameSaveFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Recoverable { error, .. } | Self::Unrecoverable(error) => Some(error),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
/// Serialized regional game state containing only authoritative region data.
struct RegionalGameSave {
    selected_region: Option<RegionId>,
    regions: Vec<RegionStateSaveRecord>,
}

#[derive(Debug)]
/// UI-facing owner/facade for threaded regional simulation.
pub struct RegionalGame {
    runner: RegionalGameRunner,
    region_ids: Vec<RegionId>,
    selected_region: Option<RegionId>,
    next_request_id: AtomicU64,
}

impl RegionalGame {
    pub fn single_region(width: usize, height: usize) -> Result<Self, RegionalGameError> {
        Self::from_regions(vec![RegionState::new(
            DEFAULT_SINGLE_REGION_ID,
            width,
            height,
        )])
    }

    pub fn from_regions(regions: Vec<RegionState>) -> Result<Self, RegionalGameError> {
        let region_ids = regions.iter().map(RegionState::id).collect::<Vec<_>>();
        let selected_region = region_ids.first().copied();
        let runner = RegionalGameRunner::start(regions)?;

        let game = Self {
            runner,
            region_ids,
            selected_region,
            next_request_id: AtomicU64::new(1),
        };

        Ok(game)
    }

    pub fn two_region_default(width: usize, height: usize) -> Result<Self, RegionalGameError> {
        Self::from_regions(vec![
            RegionState::new(RegionId(1), width, height),
            RegionState::new(RegionId(2), width, height),
        ])
    }

    pub fn select_next_region(&mut self) -> Result<RegionId, RegionalGameError> {
        self.select_region_offset(1)
    }

    pub fn select_previous_region(&mut self) -> Result<RegionId, RegionalGameError> {
        self.select_region_offset(self.region_ids.len().saturating_sub(1))
    }

    pub fn selected_region(&self) -> Result<RegionId, RegionalGameError> {
        self.selected_region_or_first()
    }

    pub fn selected_region_position(&self) -> Result<(usize, usize), RegionalGameError> {
        let selected_region = self.selected_region_or_first()?;
        let index = self
            .region_ids
            .iter()
            .position(|region_id| *region_id == selected_region)
            .ok_or(RegionalGameError::UnknownRegion {
                region_id: selected_region,
            })?;
        Ok((index + 1, self.region_ids.len()))
    }

    pub fn view(&self) -> Result<RegionalGameView, RegionalGameError> {
        self.view_with_overlay(MapOverlayInput::Normal)
    }

    pub fn selected_region_view(&self) -> Result<GameView, RegionalGameError> {
        self.selected_region_view_with_overlay(MapOverlayInput::Normal)
    }

    pub fn view_with_overlay(
        &self,
        overlay: MapOverlayInput,
    ) -> Result<RegionalGameView, RegionalGameError> {
        let mut regions = Vec::new();
        for region_id in &self.region_ids {
            let UiReply::RegionSnapshotReady { snapshot, .. } = self
                .runner
                .request_region_snapshot_with_overlay(UiRequestId(0), *region_id, overlay)?;
            regions.push(snapshot);
        }

        Ok(RegionalGameView {
            regions,
            selected_region: self.selected_region,
        })
    }

    pub fn selected_region_view_with_overlay(
        &self,
        overlay: MapOverlayInput,
    ) -> Result<GameView, RegionalGameError> {
        let region_id = self.selected_region_or_first()?;
        let view = self.view_with_overlay(overlay)?;
        view.regions
            .into_iter()
            .find(|snapshot| snapshot.region_id == region_id)
            .map(|snapshot| snapshot.view)
            .ok_or(RegionalGameError::UnknownRegion { region_id })
    }

    pub fn inspect_region(
        &self,
        region_id: RegionId,
        x: usize,
        y: usize,
    ) -> Result<InspectView, RegionalGameError> {
        self.runner
            .inspect_region(region_id, x, y)
            .map_err(RegionalGameError::from)
    }

    pub fn inspect_selected_region(
        &self,
        x: usize,
        y: usize,
    ) -> Result<InspectView, RegionalGameError> {
        self.inspect_region(self.selected_region_or_first()?, x, y)
    }

    pub fn tick_region(&self, region_id: RegionId) -> Result<CommandResult, RegionalGameError> {
        let request_id = self.next_request_id();
        self.runner
            .tick_region(request_id, region_id)
            .map_err(RegionalGameError::from)
    }

    pub fn tick_all_regions(&self) -> Result<(), RegionalGameError> {
        for region_id in &self.region_ids {
            self.tick_region(*region_id)?;
        }
        Ok(())
    }

    pub fn tick_selected_region(&self) -> Result<CommandResult, RegionalGameError> {
        let region_id = self.selected_region_or_first()?;
        self.tick_region(region_id)
    }

    pub fn build(
        &self,
        region_id: RegionId,
        x: usize,
        y: usize,
        kind: BuildingKind,
    ) -> Result<CommandResult, RegionalGameError> {
        self.run_result_command(region_id, RegionCommand::Build { x, y, kind })
    }

    pub fn build_selected_region(
        &self,
        x: usize,
        y: usize,
        kind: BuildingKind,
    ) -> Result<CommandResult, RegionalGameError> {
        self.build(self.selected_region_or_first()?, x, y, kind)
    }

    pub fn preview_build(
        &self,
        region_id: RegionId,
        x: usize,
        y: usize,
        kind: BuildingKind,
    ) -> Result<BuildPreviewView, RegionalGameError> {
        let request_id = self.next_request_id();
        match self.run_command(
            request_id,
            region_id,
            RegionCommand::PreviewBuild { x, y, kind },
        )? {
            RegionCommandReply::BuildPreview(preview) => Ok(preview),
            RegionCommandReply::CommandResult(_) => {
                Err(RegionalGameError::CommandReplyTypeMismatch {
                    request_id,
                    region_id,
                })
            }
        }
    }

    pub fn preview_build_selected_region(
        &self,
        x: usize,
        y: usize,
        kind: BuildingKind,
    ) -> Result<BuildPreviewView, RegionalGameError> {
        self.preview_build(self.selected_region_or_first()?, x, y, kind)
    }

    pub fn bulldoze(
        &self,
        region_id: RegionId,
        x: usize,
        y: usize,
    ) -> Result<CommandResult, RegionalGameError> {
        self.run_result_command(region_id, RegionCommand::Bulldoze { x, y })
    }

    pub fn bulldoze_selected_region(
        &self,
        x: usize,
        y: usize,
    ) -> Result<CommandResult, RegionalGameError> {
        self.bulldoze(self.selected_region_or_first()?, x, y)
    }

    pub fn replace(
        &self,
        region_id: RegionId,
        x: usize,
        y: usize,
        kind: BuildingKind,
    ) -> Result<CommandResult, RegionalGameError> {
        self.run_result_command(region_id, RegionCommand::Replace { x, y, kind })
    }

    pub fn replace_selected_region(
        &self,
        x: usize,
        y: usize,
        kind: BuildingKind,
    ) -> Result<CommandResult, RegionalGameError> {
        self.replace(self.selected_region_or_first()?, x, y, kind)
    }

    pub fn upgrade(
        &self,
        region_id: RegionId,
        x: usize,
        y: usize,
    ) -> Result<CommandResult, RegionalGameError> {
        self.run_result_command(region_id, RegionCommand::Upgrade { x, y })
    }

    pub fn upgrade_selected_region(
        &self,
        x: usize,
        y: usize,
    ) -> Result<CommandResult, RegionalGameError> {
        self.upgrade(self.selected_region_or_first()?, x, y)
    }

    pub fn handle_ui_request(&self, request: UiRequest) -> Result<UiReply, RegionalGameError> {
        match request {
            UiRequest::GetRegionSnapshot {
                request_id,
                region_id,
            } => self.request_region_snapshot(request_id, region_id),
        }
    }

    fn request_region_snapshot(
        &self,
        request_id: UiRequestId,
        region_id: RegionId,
    ) -> Result<UiReply, RegionalGameError> {
        self.runner
            .request_region_snapshot(request_id, region_id)
            .map_err(RegionalGameError::from)
    }

    fn run_result_command(
        &self,
        region_id: RegionId,
        command: RegionCommand,
    ) -> Result<CommandResult, RegionalGameError> {
        let request_id = self.next_request_id();
        match self.run_command(request_id, region_id, command)? {
            RegionCommandReply::CommandResult(result) => Ok(result),
            RegionCommandReply::BuildPreview(_) => {
                Err(RegionalGameError::CommandReplyTypeMismatch {
                    request_id,
                    region_id,
                })
            }
        }
    }

    fn run_command(
        &self,
        request_id: UiRequestId,
        region_id: RegionId,
        command: RegionCommand,
    ) -> Result<RegionCommandReply, RegionalGameError> {
        self.runner
            .run_region_command(request_id, region_id, command)
            .map_err(RegionalGameError::from)
    }

    fn next_request_id(&self) -> UiRequestId {
        UiRequestId(self.next_request_id.fetch_add(1, Ordering::Relaxed))
    }

    fn selected_region_or_first(&self) -> Result<RegionId, RegionalGameError> {
        if let Some(region_id) = self.selected_region {
            if self.region_ids.contains(&region_id) {
                return Ok(region_id);
            }
        }

        self.region_ids
            .first()
            .copied()
            .ok_or(RegionalGameError::NoSelectedRegion)
    }

    fn select_region_offset(&mut self, offset: usize) -> Result<RegionId, RegionalGameError> {
        let current = self.selected_region_or_first()?;
        let current_index = self
            .region_ids
            .iter()
            .position(|region_id| *region_id == current)
            .ok_or(RegionalGameError::UnknownRegion { region_id: current })?;
        let next_index = (current_index + offset) % self.region_ids.len();
        let next_region = self.region_ids[next_index];
        self.selected_region = Some(next_region);
        Ok(next_region)
    }

    pub fn shutdown(self) -> Result<RecoveredRegionalGame, RegionalGameError> {
        self.runner.shutdown().map_err(RegionalGameError::from)
    }

    pub fn save_to_file(self, path: impl AsRef<Path>) -> Result<Self, RegionalGameSaveFailure> {
        let selected_region = self.selected_region;
        let region_ids = self.region_ids.clone();
        let recovered = self
            .shutdown()
            .map_err(|error| RegionalGameSaveFailure::Unrecoverable(error.into()))?;
        let save = RegionalGameSave {
            selected_region,
            regions: recovered
                .into_region_states_in_order(&region_ids)
                .into_iter()
                .map(RegionState::into_save_record)
                .collect(),
        };

        let file = match File::create(path) {
            Ok(file) => file,
            Err(error) => return Self::recover_save_failure(save, error.into()),
        };

        if let Err(error) = serde_json::to_writer_pretty(file, &save) {
            return Self::recover_save_failure(save, error.into());
        }

        Self::from_save(save)
            .map_err(RegionalGameSaveError::from)
            .map_err(RegionalGameSaveFailure::Unrecoverable)
    }

    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self, RegionalGameSaveError> {
        let file = File::open(path)?;
        let save = serde_json::from_reader(file)?;
        Self::from_save(save).map_err(RegionalGameSaveError::from)
    }

    fn from_save(save: RegionalGameSave) -> Result<Self, RegionalGameError> {
        let regions = save
            .regions
            .into_iter()
            .map(RegionState::from_save_record)
            .collect::<Vec<_>>();
        let mut game = Self::from_regions(regions)?;
        game.selected_region = save.selected_region;
        Ok(game)
    }

    fn recover_save_failure(
        save: RegionalGameSave,
        error: RegionalGameSaveError,
    ) -> Result<Self, RegionalGameSaveFailure> {
        let game = Self::from_save(save)
            .map_err(RegionalGameSaveError::from)
            .map_err(RegionalGameSaveFailure::Unrecoverable)?;
        Err(RegionalGameSaveFailure::Recoverable {
            game: Box::new(game),
            error,
        })
    }
}

impl From<RegionalGameRunnerError> for RegionalGameError {
    fn from(error: RegionalGameRunnerError) -> Self {
        match error {
            RegionalGameRunnerError::DuplicateRegion { region_id } => {
                Self::DuplicateRegion { region_id }
            }
            RegionalGameRunnerError::UnknownRegion { region_id } => {
                Self::UnknownRegion { region_id }
            }
            RegionalGameRunnerError::RegionAddFailed { .. } => Self::RegionAttachFailed,
            RegionalGameRunnerError::CommandReplyMissing {
                request_id,
                region_id,
            } => Self::CommandReplyMissing {
                request_id,
                region_id,
            },
            RegionalGameRunnerError::TickReplyMissing {
                request_id,
                region_id,
            } => Self::TickReplyMissing {
                request_id,
                region_id,
            },
            RegionalGameRunnerError::SnapshotReplyMissing {
                request_id,
                region_id,
            } => Self::SnapshotReplyMissing {
                request_id,
                region_id,
            },
            RegionalGameRunnerError::WorkerRoutingFailed { .. } => Self::WorkerRoutingFailed,
            RegionalGameRunnerError::WorkerStopped { .. } => Self::WorkerStopped,
            RegionalGameRunnerError::WorkerPanicked { .. } => Self::WorkerPanicked,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::regions::ResourceKind;

    #[test]
    fn imported_resource_generation_increases_when_export_count_decreases() {
        let game = RegionalGame::two_region_default(4, 4).unwrap();

        assert!(
            game.build(RegionId(1), 1, 1, BuildingKind::Park)
                .unwrap()
                .success
        );
        assert!(
            game.build(RegionId(1), 2, 1, BuildingKind::Park)
                .unwrap()
                .success
        );
        assert!(game.bulldoze(RegionId(1), 2, 1).unwrap().success);

        let states = game
            .shutdown()
            .unwrap()
            .into_region_states_in_order(&[RegionId(1), RegionId(2)]);
        let target = states
            .iter()
            .find(|state| state.id() == RegionId(2))
            .unwrap();
        let imported_park = target
            .imported_resources()
            .iter()
            .find(|resource| {
                resource.id.origin_region == RegionId(1)
                    && resource.id.resource_kind == ResourceKind::ParkAccess
            })
            .unwrap();

        assert_eq!(imported_park.id.generation, 3);
        assert_eq!(imported_park.remaining_capacity, 1);
    }
}
