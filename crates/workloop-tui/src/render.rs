//! Plain-text rendering of the board, the open Work, the judgment
//! draft, and Command results. Pure functions over the Query snapshots
//! — rendering is the only layer that knows about screen layout.

use workloop_core::command::CommandDecision;
use workloop_core::domain::WorkId;
use workloop_core::identity::CommitRevision;
use workloop_core::projection::{Availability, SourceStatus, WorkDetailView, WorkPhase};
use workloop_core::work::ActionPreview;

use crate::board::Board;
use crate::judgment::{JudgmentDraft, distinct_findings};

const COL_WIDTH: usize = 24;

fn phase_name(phase: &WorkPhase) -> &'static str {
    match phase {
        WorkPhase::Queued => "QUEUED",
        WorkPhase::Implementing => "IMPLEMENTING",
        WorkPhase::Reviewing => "REVIEWING",
        WorkPhase::Converging => "CONVERGING",
        WorkPhase::Waiting => "WAITING",
        WorkPhase::Paused => "PAUSED",
        WorkPhase::Stopping => "STOPPING",
        WorkPhase::Unknown => "UNKNOWN",
        WorkPhase::Stopped => "STOPPED",
        WorkPhase::Completed => "COMPLETED",
    }
}

fn cell(text: &str) -> String {
    let mut s = text.chars().take(COL_WIDTH - 1).collect::<String>();
    while s.chars().count() < COL_WIDTH {
        s.push(' ');
    }
    s
}

/// The kanban board: one column per phase in pipeline order, cards
/// numbered in `Board::order()` sequence (column-major) so `open <n>`
/// selects the card the operator counted. Each card shows Work id,
/// displayed version, and attention count. `selected` marks the open
/// card.
pub fn board_frame(board: &Board, selected: Option<&WorkId>) -> String {
    let mut numbers: std::collections::HashMap<&WorkId, usize> = std::collections::HashMap::new();
    for (i, card) in board
        .columns
        .iter()
        .flat_map(|c| c.cards.iter())
        .enumerate()
    {
        numbers.insert(&card.work_id, i + 1);
    }
    let mut out = String::new();
    if let Some(cap) = &board.capacity {
        out.push_str(&format!(
            "capacity {}/{}  reservations {}  deferred {}\n",
            cap.used,
            cap.limit,
            cap.reservations.len(),
            cap.deferred.len()
        ));
    }
    out.push_str(&format!("attention {}\n", board.attention.len()));
    for col in &board.columns {
        out.push_str(&cell(phase_name(&col.phase)));
    }
    out.push('\n');
    let height = board
        .columns
        .iter()
        .map(|c| c.cards.len())
        .max()
        .unwrap_or(0);
    for row in 0..height {
        for col in &board.columns {
            let text = match col.cards.get(row) {
                Some(card) => {
                    let mark = if selected == Some(&card.work_id) {
                        "*"
                    } else {
                        " "
                    };
                    let attn = if card.attention_count > 0 {
                        format!(" !{}", card.attention_count)
                    } else {
                        String::new()
                    };
                    let n = numbers.get(&card.work_id).copied().unwrap_or(0);
                    format!(
                        "{}{}. {} v{}{}",
                        mark, n, card.work_id.0, card.version, attn
                    )
                }
                None => String::new(),
            };
            out.push_str(&cell(&text));
        }
        out.push('\n');
    }
    out
}

/// The open Work's detail pane: version, executions, current head,
/// head-scoped findings (numbered for `pick`), attention, evidence.
/// Findings are numbered per distinct finding id — the same set, in the
/// same order, as the judgment draft's slots — so the number printed
/// beside a finding always names the slot `pick <n>` selects. A finding
/// string recorded twice is one id and gets one number.
pub fn detail_frame(detail: &WorkDetailView) -> String {
    let mut out = String::new();
    out.push_str(&format!("work {} v{}\n", detail.work_id.0, detail.version));
    for e in &detail.executions {
        out.push_str(&format!(
            "  execution {} {:?} ownership={}\n",
            e.execution_id.0, e.state, e.ownership
        ));
    }
    if let Some((change, head)) = &detail.current_change {
        out.push_str(&format!(
            "  change {} head {}\n",
            change,
            head.as_ref().map(|h| h.object.as_str()).unwrap_or("?")
        ));
    }
    for (i, f) in distinct_findings(detail).enumerate() {
        out.push_str(&format!("  finding {}: {}\n", i + 1, f.source_key));
    }
    for j in &detail.judged {
        out.push_str(&format!("  judged {j}\n"));
    }
    for a in &detail.attention {
        out.push_str(&format!(
            "  attention {}: {} -> {}\n",
            a.attention_id, a.reason, a.next_action
        ));
    }
    for e in &detail.evidence {
        out.push_str(&format!("  evidence {e}\n"));
    }
    if let Some(o) = &detail.outcome {
        out.push_str(&format!(
            "  outcome {:?} {}\n",
            o.state,
            o.detail.clone().unwrap_or_default()
        ));
    }
    out
}

/// Source availability carried with a detail snapshot — rendered so an
/// un-read source is visible rather than mistaken for a clean result.
pub fn sources_frame(sources: &[SourceStatus]) -> String {
    let mut out = String::new();
    for s in sources {
        let availability = match s.availability {
            Availability::Known => "known",
            Availability::Unknown => "unknown",
            Availability::Unavailable => "unavailable",
        };
        if s.availability != Availability::Known {
            out.push_str(&format!("  source {}: {availability}\n", s.source));
        }
    }
    out
}

/// The judgment draft: findings, choices so far, and whether it is
/// ready to arm.
pub fn draft_frame(draft: &JudgmentDraft) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "judgment {} v{} head {}\n",
        draft.work_id.0, draft.work_version, draft.expected_head.object
    ));
    for (i, slot) in draft.findings.iter().enumerate() {
        let choice = slot
            .choice
            .as_ref()
            .map(|c| format!("{c:?}"))
            .unwrap_or_else(|| "-".into());
        out.push_str(&format!("  {} {} [{}]\n", i + 1, slot.source_key, choice));
    }
    out
}

pub fn preview_frame(preview: &ActionPreview) -> String {
    let mut out = format!("availability {:?}\n", preview.availability);
    if let Some(v) = preview.observed_work_version {
        out.push_str(&format!("  observed_work_version {v}\n"));
    }
    if let Some(h) = &preview.observed_head {
        out.push_str(&format!("  observed_head {}\n", h.object));
    }
    for r in &preview.reasons {
        out.push_str(&format!("  reason {r:?}\n"));
    }
    for e in &preview.evidence {
        out.push_str(&format!("  evidence {e}\n"));
    }
    out
}

pub fn decision_frame(decision: &CommandDecision) -> String {
    match decision {
        CommandDecision::Accepted {
            command_id,
            work_id,
            work_version,
            effect,
            ..
        } => format!(
            "accepted {} work={:?} version={:?} effect={effect:?}\n",
            command_id.0, work_id, work_version
        ),
        CommandDecision::Rejected {
            command_id,
            reason,
            current_version,
            evidence,
            ..
        } => format!(
            "rejected {} reason={reason:?} current_version={current_version:?} evidence={evidence:?}\n",
            command_id.0
        ),
    }
}

/// A pending request rendered as unknown — the display carries the key
/// the operator can recover by, never a guessed outcome.
pub fn pending_frame(idempotency_key: &str, head_hint: Option<&CommitRevision>) -> String {
    let mut out = format!("submit result: UNKNOWN (idempotency_key {idempotency_key})\n");
    if let Some(h) = head_hint {
        out.push_str(&format!("  expected head at issue time {}\n", h.object));
    }
    out.push_str("  recover with: recover\n");
    out
}
