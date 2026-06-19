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

use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::core::regional_game_runner::{
    RecoveredRegionalGame, RegionalGameRunner, RegionalGameRunnerError,
};
pub use crate::core::regional_types::{
    RegionCommand, RegionCommandReply, RegionViewSnapshot, RegionalGameView, UiReply, UiRequest,
    UiRequestId,
};
use crate::core::regions::{
    BorderEdge, RegionId, RegionNeighborLink, RegionState, RegionStateSaveRecord,
};
use crate::interface::events::{CommandResult, EconomyBreakdownView, GameEventView};
use crate::interface::input::{BuildingKind, MapOverlayInput};
use crate::interface::view::{BuildPreviewView, CityGoodsView, GameView, InspectView};

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
    InvalidLayout {
        rows: usize,
        columns: usize,
        region_count: usize,
    },
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
///
/// Topology is saved indirectly: `regions` are ordered row-major and `layout`
/// stores the grid shape. At load/start time, adjacent grid cells derive
/// `RegionNeighborLink` edges for the worker.
struct RegionalGameSave {
    selected_region: Option<RegionId>,
    regions: Vec<RegionStateSaveRecord>,
    layout: RegionalLayoutSave,
}

#[derive(Debug, Deserialize)]
/// Save reader that can infer layout for saves written before layout existed.
struct RegionalGameSaveWire {
    selected_region: Option<RegionId>,
    regions: Vec<RegionStateSaveRecord>,
    #[serde(default)]
    layout: Option<RegionalLayoutSave>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Compact row-major regional map shape saved instead of explicit topology.
///
/// A `rows x columns` layout maps save order onto the in-game regional grid:
///
/// ```text
/// 2 rows x 3 columns
///
/// index:   [0] -- [1] -- [2]
///           |      |      |
///          [3] -- [4] -- [5]
///
/// saved regions: [R0, R1, R2, R3, R4, R5]
///
/// derived topology:
///   R0 East <-> R1 West     R0 South <-> R3 North
///   R1 East <-> R2 West     R1 South <-> R4 North
///   R3 East <-> R4 West     R2 South <-> R5 North
///   R4 East <-> R5 West
/// ```
struct RegionalLayoutSave {
    rows: usize,
    columns: usize,
}

#[derive(Debug)]
/// UI-facing owner/facade for threaded regional simulation.
pub struct RegionalGame {
    runner: RegionalGameRunner,
    region_ids: Vec<RegionId>,
    layout: RegionalLayoutSave,
    worker_count: usize,
    region_worker_indexes: Option<Vec<usize>>,
    selected_region: Option<RegionId>,
    next_request_id: AtomicU64,
    latest_goods_by_region: Mutex<BTreeMap<RegionId, CityGoodsView>>,
}

impl RegionalGame {
    pub fn single_region(width: usize, height: usize) -> Result<Self, RegionalGameError> {
        Self::from_regions(vec![RegionState::new(
            DEFAULT_SINGLE_REGION_ID,
            width,
            height,
        )])
    }

    /// Builds a regional game from owned region states, laid out row-major `1 x N`
    /// (a single row): region `i` borders region `i+1` west/east. This is the right
    /// topology for 1-2 regions; a true multi-row grid needs an explicit layout, so
    /// route any future public multi-row constructor through `from_regions_with_layout`
    /// rather than here.
    pub fn from_regions(regions: Vec<RegionState>) -> Result<Self, RegionalGameError> {
        let layout = infer_layout_for_region_count(regions.len());
        Self::from_regions_with_layout(regions, layout)
    }

    pub fn from_regions_with_worker_count(
        regions: Vec<RegionState>,
        worker_count: usize,
    ) -> Result<Self, RegionalGameError> {
        let layout = infer_layout_for_region_count(regions.len());
        Self::from_regions_with_layout_and_worker_setup(regions, layout, worker_count, None)
    }

    pub fn from_regions_with_worker_assignments(
        regions: Vec<RegionState>,
        worker_count: usize,
        region_worker_indexes: Vec<usize>,
    ) -> Result<Self, RegionalGameError> {
        let layout = infer_layout_for_region_count(regions.len());
        Self::from_regions_with_layout_and_worker_setup(
            regions,
            layout,
            worker_count,
            Some(region_worker_indexes),
        )
    }

    fn from_regions_with_layout(
        regions: Vec<RegionState>,
        layout: RegionalLayoutSave,
    ) -> Result<Self, RegionalGameError> {
        Self::from_regions_with_layout_and_worker_setup(regions, layout, 1, None)
    }

    fn from_regions_with_layout_and_worker_setup(
        regions: Vec<RegionState>,
        layout: RegionalLayoutSave,
        worker_count: usize,
        region_worker_indexes: Option<Vec<usize>>,
    ) -> Result<Self, RegionalGameError> {
        let region_ids = regions.iter().map(RegionState::id).collect::<Vec<_>>();
        validate_layout(region_ids.len(), layout)?;
        let topology = derive_topology(&region_ids, layout);
        let selected_region = region_ids.first().copied();
        let runner = match region_worker_indexes.clone() {
            Some(indexes) => RegionalGameRunner::start_with_topology_and_worker_assignments(
                regions,
                topology,
                worker_count,
                indexes,
            )?,
            None => RegionalGameRunner::start_with_topology_and_worker_count(
                regions,
                topology,
                worker_count,
            )?,
        };

        let game = Self {
            runner,
            region_ids,
            layout,
            worker_count,
            region_worker_indexes,
            selected_region,
            next_request_id: AtomicU64::new(1),
            latest_goods_by_region: Mutex::new(BTreeMap::new()),
        };

        Ok(game)
    }

    pub fn two_region_default(width: usize, height: usize) -> Result<Self, RegionalGameError> {
        let left = RegionId(1);
        let right = RegionId(2);
        Self::from_regions_with_layout(
            vec![
                RegionState::new(left, width, height),
                RegionState::new(right, width, height),
            ],
            RegionalLayoutSave {
                rows: 1,
                columns: 2,
            },
        )
    }

    pub fn three_by_three_default(width: usize, height: usize) -> Result<Self, RegionalGameError> {
        let regions = (1..=9)
            .map(|id| RegionState::new(RegionId(id), width, height))
            .collect();
        Self::from_regions_with_layout_and_worker_setup(
            regions,
            RegionalLayoutSave {
                rows: 3,
                columns: 3,
            },
            3,
            None,
        )
    }

    pub fn select_next_region(&mut self) -> Result<RegionId, RegionalGameError> {
        self.select_region_offset(1)
    }

    pub fn select_previous_region(&mut self) -> Result<RegionId, RegionalGameError> {
        self.select_region_offset(self.region_ids.len().saturating_sub(1))
    }

    pub fn select_region_by_id(
        &mut self,
        region_id: RegionId,
    ) -> Result<RegionId, RegionalGameError> {
        if self.region_ids.contains(&region_id) {
            self.selected_region = Some(region_id);
            Ok(region_id)
        } else {
            Err(RegionalGameError::UnknownRegion { region_id })
        }
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

    pub fn neighbor_region(&self, region_id: RegionId, edge: BorderEdge) -> Option<RegionId> {
        let index = self.region_ids.iter().position(|id| *id == region_id)?;
        let row = index / self.layout.columns;
        let column = index % self.layout.columns;
        let neighbor_index = match edge {
            BorderEdge::North if row > 0 => index.checked_sub(self.layout.columns)?,
            BorderEdge::South if row + 1 < self.layout.rows => index + self.layout.columns,
            BorderEdge::West if column > 0 => index.checked_sub(1)?,
            BorderEdge::East if column + 1 < self.layout.columns => index + 1,
            _ => return None,
        };
        self.region_ids.get(neighbor_index).copied()
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

        let goods = self.city_goods();
        for snapshot in &mut regions {
            snapshot.view.status.goods = goods;
        }

        Ok(RegionalGameView {
            regions,
            selected_region: self.selected_region,
            goods,
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
        let result = self
            .runner
            .tick_region(request_id, region_id)
            .map_err(RegionalGameError::from)?;
        self.record_tick_goods(region_id, &result);
        Ok(result)
    }

    pub fn tick_all_regions(&self) -> Result<(), RegionalGameError> {
        let requests = self
            .region_ids
            .iter()
            .copied()
            .map(|region_id| (self.next_request_id(), region_id))
            .collect::<Vec<_>>();
        let results = self.runner.tick_regions(&requests)?;
        for ((_, region_id), result) in requests.iter().zip(results.iter()) {
            self.record_tick_goods(*region_id, result);
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

    fn record_tick_goods(&self, region_id: RegionId, result: &CommandResult) {
        let Some(goods) = result.events.iter().find_map(tick_goods_summary) else {
            return;
        };
        self.latest_goods_by_region
            .lock()
            .expect("regional goods summary lock poisoned")
            .insert(region_id, goods);
    }

    fn city_goods(&self) -> CityGoodsView {
        self.latest_goods_by_region
            .lock()
            .expect("regional goods summary lock poisoned")
            .values()
            .fold(CityGoodsView::default(), |mut total, goods| {
                total.city_goods_produced += goods.city_goods_produced;
                total.goods_imported_from_outside += goods.goods_imported_from_outside;
                total.goods_exported_outside += goods.goods_exported_outside;
                total
            })
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
        let layout = self.layout;
        let worker_count = self.worker_count;
        let region_worker_indexes = self.region_worker_indexes.clone();
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
            layout,
        };

        let file = match File::create(path) {
            Ok(file) => file,
            Err(error) => {
                return Self::recover_save_failure(
                    save,
                    worker_count,
                    region_worker_indexes,
                    error.into(),
                );
            }
        };

        if let Err(error) = serde_json::to_writer_pretty(file, &save) {
            return Self::recover_save_failure(
                save,
                worker_count,
                region_worker_indexes,
                error.into(),
            );
        }

        Self::from_save_with_worker_setup(save, worker_count, region_worker_indexes)
            .map_err(RegionalGameSaveError::from)
            .map_err(RegionalGameSaveFailure::Unrecoverable)
    }

    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self, RegionalGameSaveError> {
        let bytes = std::fs::read(path)?;
        Self::from_save_bytes(&bytes)
    }

    fn from_save(save: RegionalGameSave) -> Result<Self, RegionalGameError> {
        Self::from_save_with_worker_setup(save, 1, None)
    }

    fn from_save_with_worker_setup(
        save: RegionalGameSave,
        worker_count: usize,
        region_worker_indexes: Option<Vec<usize>>,
    ) -> Result<Self, RegionalGameError> {
        let regions = save
            .regions
            .into_iter()
            .map(RegionState::from_save_record)
            .collect::<Vec<_>>();
        let mut game = Self::from_regions_with_layout_and_worker_setup(
            regions,
            save.layout,
            worker_count,
            region_worker_indexes,
        )?;
        game.selected_region = save.selected_region;
        game.runner.settle_power_imports(game.next_request_id())?;
        Ok(game)
    }

    fn from_save_bytes(bytes: &[u8]) -> Result<Self, RegionalGameSaveError> {
        match serde_json::from_slice::<RegionalGameSaveWire>(bytes) {
            Ok(save) => Self::from_save(save.into_current()).map_err(RegionalGameSaveError::from),
            Err(regional_error) => match Self::from_legacy_world_bytes(bytes) {
                Ok(game) => Ok(game),
                Err(RegionalGameSaveError::SaveFormat(_)) => {
                    Err(RegionalGameSaveError::SaveFormat(regional_error))
                }
                Err(error) => Err(error),
            },
        }
    }

    fn from_legacy_world_bytes(bytes: &[u8]) -> Result<Self, RegionalGameSaveError> {
        let region = RegionState::from_legacy_world_bytes(DEFAULT_SINGLE_REGION_ID, bytes)
            .map_err(RegionalGameSaveError::SaveFormat)?;
        Self::from_regions(vec![region]).map_err(RegionalGameSaveError::from)
    }

    fn recover_save_failure(
        save: RegionalGameSave,
        worker_count: usize,
        region_worker_indexes: Option<Vec<usize>>,
        error: RegionalGameSaveError,
    ) -> Result<Self, RegionalGameSaveFailure> {
        let game = Self::from_save_with_worker_setup(save, worker_count, region_worker_indexes)
            .map_err(RegionalGameSaveError::from)
            .map_err(RegionalGameSaveFailure::Unrecoverable)?;
        Err(RegionalGameSaveFailure::Recoverable {
            game: Box::new(game),
            error,
        })
    }
}

impl RegionalGameSaveWire {
    fn into_current(self) -> RegionalGameSave {
        let layout = self
            .layout
            .unwrap_or_else(|| infer_layout_for_region_count(self.regions.len()));
        RegionalGameSave {
            selected_region: self.selected_region,
            regions: self.regions,
            layout,
        }
    }
}

fn infer_layout_for_region_count(region_count: usize) -> RegionalLayoutSave {
    match region_count {
        0 => RegionalLayoutSave {
            rows: 0,
            columns: 0,
        },
        1 => RegionalLayoutSave {
            rows: 1,
            columns: 1,
        },
        count => RegionalLayoutSave {
            rows: 1,
            columns: count,
        },
    }
}

fn validate_layout(
    region_count: usize,
    layout: RegionalLayoutSave,
) -> Result<(), RegionalGameError> {
    let valid_empty = region_count == 0 && layout.rows == 0 && layout.columns == 0;
    let valid_grid = layout.rows > 0
        && layout.columns > 0
        && layout.rows.checked_mul(layout.columns) == Some(region_count);

    if valid_empty || valid_grid {
        Ok(())
    } else {
        Err(RegionalGameError::InvalidLayout {
            rows: layout.rows,
            columns: layout.columns,
            region_count,
        })
    }
}

fn tick_goods_summary(event: &GameEventView) -> Option<CityGoodsView> {
    let GameEventView::TickSummary { economy, .. } = event else {
        return None;
    };
    let goods = goods_summary_from_economy(*economy);
    (goods.city_goods_produced > 0
        || goods.goods_imported_from_outside > 0
        || goods.goods_exported_outside > 0)
        .then_some(goods)
}

fn goods_summary_from_economy(economy: EconomyBreakdownView) -> CityGoodsView {
    CityGoodsView {
        city_goods_produced: economy.local_goods_produced,
        goods_imported_from_outside: economy.imported_goods_sold,
        goods_exported_outside: economy.exported_goods,
    }
}

fn derive_topology(region_ids: &[RegionId], layout: RegionalLayoutSave) -> Vec<RegionNeighborLink> {
    let mut topology = Vec::new();

    for row in 0..layout.rows {
        for column in 0..layout.columns {
            let index = row * layout.columns + column;
            let region = region_ids[index];

            if column + 1 < layout.columns {
                let east = region_ids[index + 1];
                topology.push(RegionNeighborLink::new(region, BorderEdge::East, east));
                topology.push(RegionNeighborLink::new(east, BorderEdge::West, region));
            }

            if row + 1 < layout.rows {
                let south = region_ids[index + layout.columns];
                topology.push(RegionNeighborLink::new(region, BorderEdge::South, south));
                topology.push(RegionNeighborLink::new(south, BorderEdge::North, region));
            }
        }
    }

    topology
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
            RegionalGameRunnerError::InvalidWorkerCount { .. } => Self::RegionAttachFailed,
            RegionalGameRunnerError::InvalidWorkerAssignmentCount { .. } => {
                Self::RegionAttachFailed
            }
            RegionalGameRunnerError::InvalidWorkerAssignment { .. } => Self::RegionAttachFailed,
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

    #[test]
    fn derives_row_major_topology_from_layout() {
        let region_ids = [
            RegionId(10),
            RegionId(11),
            RegionId(12),
            RegionId(13),
            RegionId(14),
            RegionId(15),
        ];

        let topology = derive_topology(
            &region_ids,
            RegionalLayoutSave {
                rows: 2,
                columns: 3,
            },
        );

        assert_eq!(
            topology,
            vec![
                RegionNeighborLink::new(RegionId(10), BorderEdge::East, RegionId(11)),
                RegionNeighborLink::new(RegionId(11), BorderEdge::West, RegionId(10)),
                RegionNeighborLink::new(RegionId(10), BorderEdge::South, RegionId(13)),
                RegionNeighborLink::new(RegionId(13), BorderEdge::North, RegionId(10)),
                RegionNeighborLink::new(RegionId(11), BorderEdge::East, RegionId(12)),
                RegionNeighborLink::new(RegionId(12), BorderEdge::West, RegionId(11)),
                RegionNeighborLink::new(RegionId(11), BorderEdge::South, RegionId(14)),
                RegionNeighborLink::new(RegionId(14), BorderEdge::North, RegionId(11)),
                RegionNeighborLink::new(RegionId(12), BorderEdge::South, RegionId(15)),
                RegionNeighborLink::new(RegionId(15), BorderEdge::North, RegionId(12)),
                RegionNeighborLink::new(RegionId(13), BorderEdge::East, RegionId(14)),
                RegionNeighborLink::new(RegionId(14), BorderEdge::West, RegionId(13)),
                RegionNeighborLink::new(RegionId(14), BorderEdge::East, RegionId(15)),
                RegionNeighborLink::new(RegionId(15), BorderEdge::West, RegionId(14)),
            ]
        );
    }

    #[test]
    fn invalid_layout_is_rejected_before_runner_start() {
        let result = validate_layout(
            2,
            RegionalLayoutSave {
                rows: 2,
                columns: 2,
            },
        );

        assert_eq!(
            result,
            Err(RegionalGameError::InvalidLayout {
                rows: 2,
                columns: 2,
                region_count: 2,
            })
        );
    }

    #[test]
    fn three_by_three_default_uses_nine_regions_and_three_workers() {
        let game = RegionalGame::three_by_three_default(4, 4).unwrap();

        assert_eq!(game.region_ids, (1..=9).map(RegionId).collect::<Vec<_>>());
        assert_eq!(
            game.layout,
            RegionalLayoutSave {
                rows: 3,
                columns: 3,
            }
        );
        assert_eq!(game.worker_count, 3);
        assert_eq!(game.selected_region_position().unwrap(), (1, 9));
    }
}
