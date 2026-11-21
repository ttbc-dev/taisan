//! The `judge_findings` draft: the operator's choices over the findings
//! the projection displayed for the current sealed review round. This is
//! selection state — it is persisted only through a submitted Command,
//! and a rejection for the displayed head or Work version discards it
//! rather than retargets it.

use workloop_core::command::{CommandBody, FindingChoice, FindingDecision, WorkVersion};
use workloop_core::domain::WorkId;
use workloop_core::identity::CommitRevision;
use workloop_core::projection::{FindingView, WorkDetailView};

/// The distinct findings a `work_detail` snapshot displays, in first-seen
/// order. The detail pane numbers this same list, and the draft's slot
/// list is built from it: a number on screen always names the finding
/// `pick` selects.
pub fn distinct_findings(detail: &WorkDetailView) -> impl Iterator<Item = &FindingView> + '_ {
    detail.findings.iter()
}

/// One displayed finding and the operator's pending choice. The
/// `occurrence_id` is exactly what the sealed round recorded — the draft
/// never invents or rewrites ids.
#[derive(Debug, Clone)]
pub struct FindingSlot {
    pub occurrence_id: String,
    pub source_key: String,
    /// The sealed claim's finding text — display only.
    pub summary: Option<String>,
    pub choice: Option<FindingChoice>,
    /// The disposition rationale dismiss/accept decisions require.
    pub rationale: Option<String>,
}

#[derive(Debug, Clone)]
pub struct JudgmentDraft {
    pub work_id: WorkId,
    /// The Work version the operator saw.
    pub work_version: WorkVersion,
    /// The change head the displayed findings are scoped to.
    pub expected_head: CommitRevision,
    pub findings: Vec<FindingSlot>,
}

impl JudgmentDraft {
    /// Start a draft from a `work_detail` result. `None` when there is no
    /// observed head or no head-scoped finding to decide.
    pub fn from_detail(detail: &WorkDetailView) -> Option<Self> {
        let Some((_, Some(head))) = detail.current_change.as_ref() else {
            return None;
        };
        if detail.findings.is_empty() {
            return None;
        }
        // One slot per displayed occurrence — `distinct_findings` is the
        // same set the detail pane numbers, so two rows can never compete
        // under the same key and no displayed number ever falls outside
        // the slot list.
        Some(Self {
            work_id: detail.work_id.clone(),
            work_version: detail.version,
            expected_head: head.clone(),
            findings: distinct_findings(detail)
                .map(|f| FindingSlot {
                    occurrence_id: f.occurrence_id.clone(),
                    source_key: f.source_key.clone(),
                    summary: f.summary.clone(),
                    choice: None,
                    rationale: None,
                })
                .collect(),
        })
    }

    /// Whether the draft covers exactly the occurrence ids a
    /// `work_detail` snapshot displays. A new review recorded for the
    /// same head grows the displayed set; a draft that no longer covers
    /// it must not submit — the Command would omit a displayed finding.
    pub fn covers(&self, findings: &[FindingView]) -> bool {
        let covered: std::collections::HashSet<&String> =
            self.findings.iter().map(|s| &s.occurrence_id).collect();
        covered.len() == self.findings.len()
            && covered
                == findings
                    .iter()
                    .map(|f| &f.occurrence_id)
                    .collect::<std::collections::HashSet<_>>()
    }

    /// Record exactly one choice per displayed occurrence id; a later
    /// pick for the same id replaces the earlier one, never duplicates
    /// it. `rationale` is required by `work` for dismiss/accept.
    pub fn choose(
        &mut self,
        occurrence_id: &str,
        choice: FindingChoice,
        rationale: Option<String>,
    ) -> Result<(), String> {
        let slot = self
            .findings
            .iter_mut()
            .find(|s| s.occurrence_id == occurrence_id)
            .ok_or_else(|| format!("finding '{occurrence_id}' is not in the displayed set"))?;
        slot.choice = Some(choice);
        slot.rationale = rationale;
        Ok(())
    }

    /// The Command the draft represents — only once every displayed
    /// occurrence id carries a supported decision, and always carrying
    /// the displayed Work version and head.
    pub fn command_body(&self) -> Result<CommandBody, String> {
        let mut decisions = Vec::with_capacity(self.findings.len());
        for slot in &self.findings {
            let Some(choice) = &slot.choice else {
                return Err(format!(
                    "finding '{}' has no decision yet",
                    slot.occurrence_id
                ));
            };
            decisions.push(FindingDecision {
                occurrence_id: slot.occurrence_id.clone(),
                choice: choice.clone(),
                rationale: slot.rationale.clone(),
            });
        }
        Ok(CommandBody::JudgeFindings {
            work_id: self.work_id.clone(),
            expected_work_version: self.work_version,
            expected_head: self.expected_head.clone(),
            decisions,
        })
    }
}
