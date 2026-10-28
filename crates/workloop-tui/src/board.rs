//! The kanban view model: columns derived from `work_list`, `capacity`,
//! and `attention` Query results. A card is a display row over a
//! `WorkSummary` — phase, outcome, and attention count come from the
//! projection; nothing here is a stored record of its own.

use workloop_core::command::WorkVersion;
use workloop_core::domain::{ExecutionId, WorkId};
use workloop_core::observation::Outcome;
use workloop_core::projection::{AttentionView, CapacityView, WorkListView, WorkPhase};

/// One display row on the board.
#[derive(Debug, Clone)]
pub struct Card {
    pub work_id: WorkId,
    pub version: WorkVersion,
    pub execution_id: Option<ExecutionId>,
    pub attention_count: usize,
    pub outcome: Option<Outcome>,
}

#[derive(Debug, Clone)]
pub struct Column {
    pub phase: WorkPhase,
    pub cards: Vec<Card>,
}

/// Pipeline order for the kanban columns — display layout only, never a
/// stored ordering.
pub const COLUMN_ORDER: &[WorkPhase] = &[
    WorkPhase::Queued,
    WorkPhase::Implementing,
    WorkPhase::Reviewing,
    WorkPhase::Converging,
    WorkPhase::Waiting,
    WorkPhase::Paused,
    WorkPhase::Stopping,
    WorkPhase::Unknown,
    WorkPhase::Stopped,
    WorkPhase::Completed,
];

/// The board: the `work_list` items grouped into columns plus the
/// `capacity` and `attention` Query results for the status pane.
#[derive(Debug, Clone)]
pub struct Board {
    pub columns: Vec<Column>,
    pub capacity: Option<CapacityView>,
    pub attention: Vec<AttentionView>,
}

impl Board {
    /// Derive columns purely from Query results.
    pub fn from_queries(
        list: WorkListView,
        capacity: Option<CapacityView>,
        attention: Vec<AttentionView>,
    ) -> Self {
        let columns = COLUMN_ORDER
            .iter()
            .map(|phase| Column {
                phase: phase.clone(),
                cards: list
                    .items
                    .iter()
                    .filter(|item| &item.phase == phase)
                    .map(|item| Card {
                        work_id: item.work_id.clone(),
                        version: item.version,
                        execution_id: item.current_execution_id.clone(),
                        attention_count: item.attention_count,
                        outcome: item.outcome.clone(),
                    })
                    .collect(),
            })
            .collect();
        Self {
            columns,
            capacity,
            attention,
        }
    }

    /// Work IDs in column-major order — the numbering `board_frame`
    /// prints on each card and `open <n>` resolves.
    pub fn order(&self) -> Vec<WorkId> {
        self.columns
            .iter()
            .flat_map(|c| c.cards.iter().map(|card| card.work_id.clone()))
            .collect()
    }
}
