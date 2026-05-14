use crate::interface::input::BuildingKind;

/// UI-safe result returned by Game API commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub success: bool,
    pub event: GameEventView,
    pub events: Vec<GameEventView>,
}

impl CommandResult {
    pub fn success(event: GameEventView) -> Self {
        Self {
            success: true,
            events: vec![event.clone()],
            event,
        }
    }

    pub fn failure(event: GameEventView) -> Self {
        Self {
            success: false,
            events: vec![event.clone()],
            event,
        }
    }

    /// Converts command feedback into terminal-ready text without exposing event matching to UI.
    pub fn message(&self) -> String {
        self.events
            .iter()
            .map(GameEventView::message)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Before and after values for one UI-safe simulation metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricChange<T> {
    pub before: T,
    pub after: T,
}

impl<T> MetricChange<T>
where
    T: Copy + std::ops::Sub<Output = T>,
{
    pub fn delta(self) -> T {
        self.after - self.before
    }
}

impl GameEventView {
    /// Converts one event into terminal-ready text without exposing ECS storage.
    pub fn message(&self) -> String {
        match self {
            GameEventView::Built { x, y, kind } => {
                format!("Built {} at ({}, {})", kind.label(), x, y)
            }
            GameEventView::BuildFailed { reason } => reason.clone(),
            GameEventView::TurnAdvanced { turn } => format!("Advanced to turn {turn}"),
            GameEventView::TickSummary {
                turn,
                population,
                money,
                happiness,
                pollution,
                unemployment,
                powered_buildings,
            } => format!(
                "Advanced to turn {turn}: population {} ({:+}), money {} ({:+}), happiness {} ({:+}), pollution {} ({:+}), unemployment {} ({:+}), powered buildings {} ({:+})",
                population.after,
                population.delta(),
                money.after,
                money.delta(),
                happiness.after,
                happiness.delta(),
                pollution.after,
                pollution.delta(),
                unemployment.after,
                unemployment.delta(),
                powered_buildings.after,
                powered_buildings.delta()
            ),
        }
    }
}

/// Event vocabulary emitted by the Game API after commands.
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
    TickSummary {
        turn: u32,
        population: MetricChange<i32>,
        money: MetricChange<i32>,
        happiness: MetricChange<i32>,
        pollution: MetricChange<i32>,
        unemployment: MetricChange<i32>,
        powered_buildings: MetricChange<i32>,
    },
}
