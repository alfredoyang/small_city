//! UI-safe command result and event types returned by the public Game API.

use crate::interface::input::BuildingKind;
use crate::interface::view::GameTimeView;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EconomyBreakdownView {
    pub salaries_paid: i32,
    pub workplace_tax: i32,
    pub rent_income: i32,
    pub commercial_sales_tax: i32,
    pub shoppers_served: i32,
    pub local_goods_produced: i32,
    pub local_goods_stored: i32,
    pub local_goods_sold: i32,
    pub imported_goods_sold: i32,
    pub exported_goods: i32,
    pub manufacturing_tax: i32,
    pub export_tax: i32,
    pub rent_failures: i32,
    pub maintenance_cost: i32,
    pub net: i32,
}

impl GameEventView {
    /// Converts one event into terminal-ready text without exposing ECS storage.
    pub fn message(&self) -> String {
        match self {
            GameEventView::Built { x, y, kind } => {
                format!("Built {} at ({}, {})", kind.label(), x, y)
            }
            GameEventView::BuildFailed { reason } => reason.clone(),
            GameEventView::BuildingBulldozed { x, y } => {
                format!("Bulldozed building at ({x}, {y})")
            }
            GameEventView::BulldozeFailed { reason } => reason.clone(),
            GameEventView::BuildingReplaced { x, y, kind } => {
                format!("Replaced building at ({x}, {y}) with {}", kind.label())
            }
            GameEventView::ReplaceFailed { reason } => reason.clone(),
            GameEventView::BuildingUpgraded { x, y, kind, level } => {
                format!("Upgraded {} at ({x}, {y}) to level {level}", kind.label())
            }
            GameEventView::UpgradeFailed { reason } => reason.clone(),
            GameEventView::TurnAdvanced { turn } => format!("Advanced to turn {turn}"),
            GameEventView::TickSummary {
                turn,
                time,
                population,
                money,
                happiness,
                pollution,
                unemployment,
                powered_buildings,
                economy,
            } => format!(
                "Advanced to turn {turn} ({}): population {} ({:+}), money {} ({:+}), happiness {} ({:+}), pollution {} ({:+}), unemployment {} ({:+}), powered buildings {} ({:+})\nEconomy: salaries paid {}, workplace tax +{}, rent +{}, sales tax +{}, shoppers {}, local goods produced {}, stored {}, sold {}, imported {}, exported {}, manufacturing tax +{}, export tax +{}, rent failures {}, maintenance -{}, net {:+}",
                time.label,
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
                powered_buildings.delta(),
                economy.salaries_paid,
                economy.workplace_tax,
                economy.rent_income,
                economy.commercial_sales_tax,
                economy.shoppers_served,
                economy.local_goods_produced,
                economy.local_goods_stored,
                economy.local_goods_sold,
                economy.imported_goods_sold,
                economy.exported_goods,
                economy.manufacturing_tax,
                economy.export_tax,
                economy.rent_failures,
                economy.maintenance_cost,
                economy.net
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
    BuildingBulldozed {
        x: usize,
        y: usize,
    },
    BulldozeFailed {
        reason: String,
    },
    BuildingReplaced {
        x: usize,
        y: usize,
        kind: BuildingKind,
    },
    ReplaceFailed {
        reason: String,
    },
    BuildingUpgraded {
        x: usize,
        y: usize,
        kind: BuildingKind,
        level: u8,
    },
    UpgradeFailed {
        reason: String,
    },
    TurnAdvanced {
        turn: u32,
    },
    TickSummary {
        turn: u32,
        time: GameTimeView,
        population: MetricChange<i32>,
        money: MetricChange<i32>,
        happiness: MetricChange<i32>,
        pollution: MetricChange<i32>,
        unemployment: MetricChange<i32>,
        powered_buildings: MetricChange<i32>,
        economy: EconomyBreakdownView,
    },
}
