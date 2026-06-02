//! Shared UI-safe regional request, reply, and snapshot values.
//!
//! These owned values are used by the regional facade and runner without making
//! the runner or lower-level threading code depend on the `RegionalGame` facade
//! module.

use crate::core::regions::RegionId;
use crate::interface::view::GameView;

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
