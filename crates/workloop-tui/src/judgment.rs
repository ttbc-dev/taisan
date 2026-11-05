//! The `judge_findings` draft: the operator's choices over the findings
//! the projection displayed for one PR head. This is selection state —
//! it is persisted only through a submitted Command, and a rejection for
//! the displayed head or Work version discards it rather than retargets
//! it.

use workloop_core::command::{CommandBody, FindingChoice, FindingDecision, PrHeadSha, WorkVersion};
use workloop_core::domain::WorkId;
use workloop_core::projection::WorkDetailView;

/// The distinct finding ids a `work_detail` snapshot displays, in
/// first-seen order. `work_detail` emits one finding string per recorded
/// review, and identical strings are the same finding id — so this is
/// the single numbering the detail pane prints and the draft's slot list
/// is built from: a number on screen always names the finding `pick`
/// selects, never a duplicate row that pushed the printed numbers off
/// the slots.
pub fn distinct_findings(detail: &WorkDetailView) -> impl Iterator<Item = &str> + '_ {
    let mut seen = std::collections::HashSet::new();
    detail
        .findings
        .iter()
        .map(|finding_id| finding_id.as_str())
        .filter(move |finding_id| seen.insert(*finding_id))
}

/// One displayed finding and the operator's pending choice. The
/// `finding_id` is the finding exactly as `work_detail` scoped it to the
/// current head — the draft never invents or rewrites ids.
#[derive(Debug, Clone)]
pub struct FindingSlot {
    pub finding_id: String,
    pub choice: Option<FindingChoice>,
}

#[derive(Debug, Clone)]
pub struct JudgmentDraft {
    pub work_id: WorkId,
    /// The Work version the operator saw.
    pub work_version: WorkVersion,
    /// The PR head the displayed findings are scoped to.
    pub pr_head: PrHeadSha,
    pub findings: Vec<FindingSlot>,
}

impl JudgmentDraft {
    /// Start a draft from a `work_detail` result. `None` when there is no
    /// observed head or no head-scoped finding to decide.
    pub fn from_detail(detail: &WorkDetailView) -> Option<Self> {
        let Some((_, Some(head))) = detail.current_pr.as_ref() else {
            return None;
        };
        if detail.findings.is_empty() {
            return None;
        }
        // One slot per distinct displayed finding id — `distinct_findings`
        // is the same set the detail pane numbers, so two rows can never
        // compete under the same key and no displayed number ever falls
        // outside the slot list.
        Some(Self {
            work_id: detail.work_id.clone(),
            work_version: detail.version,
            pr_head: head.clone(),
            findings: distinct_findings(detail)
                .map(|finding_id| FindingSlot {
                    finding_id: finding_id.to_string(),
                    choice: None,
                })
                .collect(),
        })
    }

    /// Whether the draft covers exactly the finding ids a `work_detail`
    /// snapshot displays. A new review recorded for the same head grows
    /// the displayed set; a draft that no longer covers it must not
    /// submit — the Command would omit a displayed finding.
    pub fn covers(&self, finding_ids: &[String]) -> bool {
        let covered: std::collections::HashSet<&String> =
            self.findings.iter().map(|s| &s.finding_id).collect();
        covered.len() == self.findings.len()
            && covered == finding_ids.iter().collect::<std::collections::HashSet<_>>()
    }

    /// Record exactly one choice per displayed finding id; a later pick
    /// for the same id replaces the earlier one, never duplicates it.
    pub fn choose(&mut self, finding_id: &str, choice: FindingChoice) -> Result<(), String> {
        let slot = self
            .findings
            .iter_mut()
            .find(|s| s.finding_id == finding_id)
            .ok_or_else(|| format!("finding '{finding_id}' is not in the displayed set"))?;
        slot.choice = Some(choice);
        Ok(())
    }

    /// The Command the draft represents — only once every displayed
    /// finding id carries a supported decision, and always carrying the
    /// displayed Work version and PR head.
    pub fn command_body(&self) -> Result<CommandBody, String> {
        let mut decisions = Vec::with_capacity(self.findings.len());
        for slot in &self.findings {
            let Some(choice) = &slot.choice else {
                return Err(format!("finding '{}' has no decision yet", slot.finding_id));
            };
            decisions.push(FindingDecision {
                finding_id: slot.finding_id.clone(),
                choice: choice.clone(),
            });
        }
        Ok(CommandBody::JudgeFindings {
            work_id: self.work_id.clone(),
            expected_work_version: self.work_version,
            expected_pr_head: self.pr_head.clone(),
            decisions,
        })
    }
}
