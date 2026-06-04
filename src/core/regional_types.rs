//! Shared UI-safe regional request, reply, and snapshot values.
//!
//! These owned values are used by the regional facade and runner without making
//! the runner or lower-level threading code depend on the `RegionalGame` facade
//! module.

use crate::core::regions::RegionId;
use crate::interface::events::CommandResult;
use crate::interface::input::BuildingKind;
use crate::interface::view::BuildPreviewView;
use crate::interface::view::GameView;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Stable request identity for UI-to-region event replies.
///
/// Regional work is queued and processed asynchronously by worker passes, while
/// facade APIs still present synchronous calls. The runner matches replies by
/// both this ID and `RegionId` so an older queued event for the same region
/// cannot be mistaken for the request currently being awaited.
pub struct UiRequestId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
/// Owned UI-safe snapshot for one region.
pub struct RegionViewSnapshot {
    pub region_id: RegionId,
    pub revision: u64,
    pub view: GameView,
}

impl RegionViewSnapshot {
    pub fn from_view(region_id: RegionId, view: GameView) -> Self {
        Self {
            region_id,
            revision: view.status.turn as u64,
            view,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Correlated owned snapshot reply produced by a region runtime event.
pub struct RegionSnapshotResponse {
    pub request_id: UiRequestId,
    pub region_id: RegionId,
    pub snapshot: RegionViewSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Correlated regional tick reply produced by a runtime event.
pub struct RegionTickResponse {
    pub request_id: UiRequestId,
    pub region_id: RegionId,
    pub result: CommandResult,
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
/// Owned player command payload for one region.
pub enum RegionCommand {
    Build {
        x: usize,
        y: usize,
        kind: BuildingKind,
    },
    PreviewBuild {
        x: usize,
        y: usize,
        kind: BuildingKind,
    },
    Bulldoze {
        x: usize,
        y: usize,
    },
    Replace {
        x: usize,
        y: usize,
        kind: BuildingKind,
    },
    Upgrade {
        x: usize,
        y: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Owned result for one regional command.
pub enum RegionCommandReply {
    CommandResult(CommandResult),
    BuildPreview(BuildPreviewView),
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Correlated regional command reply produced by a runtime event.
pub struct RegionCommandResponse {
    pub request_id: UiRequestId,
    pub region_id: RegionId,
    pub reply: RegionCommandReply,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::regions::RegionState;

    #[test]
    fn snapshot_revision_is_derived_from_view_turn() {
        let mut region = RegionState::new(RegionId(7), 3, 3);
        region.tick_local();
        region.tick_local();

        let snapshot = RegionViewSnapshot::from_view(RegionId(7), region.view());

        assert_eq!(snapshot.region_id, RegionId(7));
        assert_eq!(snapshot.revision, 2);
        assert_eq!(snapshot.view.status.turn, 2);
    }
}
