//! UI-facing facade for single-threaded regional simulation.
//!
//! `RegionalGame` owns region runtimes and exposes only owned view models,
//! request/reply values, and deterministic errors. It does not expose ECS
//! `World` storage or require UI callers to talk directly to `RegionRuntime`.

use crate::core::regions::runtime::{RegionEvent, RegionRuntime};
use crate::core::regions::{RegionId, RegionState};
use crate::interface::view::{GameView, InspectView};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Stable request identity for UI-to-region snapshot requests.
pub struct UiRequestId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
/// Owned UI-safe snapshot for one region.
pub struct RegionViewSnapshot {
    pub region_id: RegionId,
    pub revision: u64,
    pub view: GameView,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Owned UI-safe composed view of all known regions.
pub struct RegionalGameView {
    pub regions: Vec<RegionViewSnapshot>,
    pub selected_region: Option<RegionId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// UI-facing request payloads for the regional facade.
pub enum UiRequest {
    GetRegionSnapshot {
        request_id: UiRequestId,
        region_id: RegionId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// UI-facing reply payloads returned by the regional facade.
pub enum UiReply {
    RegionSnapshotReady {
        request_id: UiRequestId,
        region_id: RegionId,
        snapshot: RegionViewSnapshot,
    },
}

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
}

#[derive(Debug)]
/// Single-threaded owner/facade for regional simulation.
pub struct RegionalGame {
    runtimes: Vec<RegionRuntime>,
    selected_region: Option<RegionId>,
}

impl RegionalGame {
    pub fn from_regions(regions: Vec<RegionState>) -> Result<Self, RegionalGameError> {
        let mut runtimes = Vec::new();

        for region in regions {
            let region_id = region.id();
            if runtimes
                .iter()
                .any(|runtime: &RegionRuntime| runtime.region_id() == region_id)
            {
                return Err(RegionalGameError::DuplicateRegion { region_id });
            }
            runtimes.push(RegionRuntime::new(region));
        }

        let selected_region = runtimes.first().map(RegionRuntime::region_id);
        Ok(Self {
            runtimes,
            selected_region,
        })
    }

    pub fn view(&self) -> RegionalGameView {
        RegionalGameView {
            regions: self.runtimes.iter().map(snapshot_from_runtime).collect(),
            selected_region: self.selected_region,
        }
    }

    pub fn inspect_region(
        &self,
        region_id: RegionId,
        x: usize,
        y: usize,
    ) -> Result<InspectView, RegionalGameError> {
        let runtime = self
            .region(region_id)
            .ok_or(RegionalGameError::UnknownRegion { region_id })?;

        Ok(runtime.state().inspect(x, y))
    }

    pub fn tick_region(&mut self, region_id: RegionId) -> Result<(), RegionalGameError> {
        let runtime = self
            .region_mut(region_id)
            .ok_or(RegionalGameError::UnknownRegion { region_id })?;

        runtime.push_event(RegionEvent::Tick);
        runtime.process_next_event();
        Ok(())
    }

    pub fn tick_all_regions(&mut self) {
        let region_ids = self
            .runtimes
            .iter()
            .map(RegionRuntime::region_id)
            .collect::<Vec<_>>();

        for region_id in region_ids {
            // The region IDs come from the owned runtime list, so this cannot
            // fail unless the list is mutated during this method.
            let _ = self.tick_region(region_id);
        }
    }

    pub fn handle_ui_request(&mut self, request: UiRequest) -> Result<UiReply, RegionalGameError> {
        match request {
            UiRequest::GetRegionSnapshot {
                request_id,
                region_id,
            } => self.request_region_snapshot(request_id, region_id),
        }
    }

    fn request_region_snapshot(
        &mut self,
        request_id: UiRequestId,
        region_id: RegionId,
    ) -> Result<UiReply, RegionalGameError> {
        let runtime = self
            .region(region_id)
            .ok_or(RegionalGameError::UnknownRegion { region_id })?;

        Ok(UiReply::RegionSnapshotReady {
            request_id,
            region_id,
            snapshot: snapshot_from_runtime(runtime),
        })
    }

    fn region(&self, region_id: RegionId) -> Option<&RegionRuntime> {
        self.runtimes
            .iter()
            .find(|runtime| runtime.region_id() == region_id)
    }

    fn region_mut(&mut self, region_id: RegionId) -> Option<&mut RegionRuntime> {
        self.runtimes
            .iter_mut()
            .find(|runtime| runtime.region_id() == region_id)
    }
}

fn snapshot_from_runtime(runtime: &RegionRuntime) -> RegionViewSnapshot {
    let view = runtime.state().view();
    RegionViewSnapshot {
        region_id: runtime.region_id(),
        revision: view.status.turn as u64,
        view,
    }
}
