use crate::interface::input::BuildingKind;

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

    pub fn message(&self) -> String {
        match &self.event {
            GameEventView::Built { x, y, kind } => {
                format!("Built {} at ({}, {})", kind.label(), x, y)
            }
            GameEventView::BuildFailed { reason } => reason.clone(),
            GameEventView::TurnAdvanced { turn } => format!("Advanced to turn {turn}"),
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
