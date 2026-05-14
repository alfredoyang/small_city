use crate::core::components::BuildingKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub success: bool,
    pub event: GameEventView,
}

impl CommandResult {
    pub fn success(event: GameEventView) -> Self {
        Self {
            success: true,
            event,
        }
    }

    pub fn failure(event: GameEventView) -> Self {
        Self {
            success: false,
            event,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GameEventView {
    Built {
        x: usize,
        y: usize,
        kind: BuildingKind,
    },
    BuildFailed {
        reason: String,
    },
    TurnAdvanced {
        turn: u32,
    },
}
