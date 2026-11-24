//! Read-only evidence history access and the `rui-evidence/1` exporter.
//!
//! The exporter replays the append-only `event_log` — the durable
//! recorded history — rather than any latest-view projection. An open
//! page sequence is pinned to the watermark its continuation token
//! binds: replaying the rows with `seq <= watermark` reproduces the
//! same record stream because recorded effects are immutable, so a
//! retried cursor returns the same logical page and writes that commit
//! after the watermark can never perturb an open sequence.
//!
//! Where recorded data cannot supply a wire fact the export emits an
//! explicit gap instead of fabricating one: finding bodies were never
//! retained as text, review rounds carry no execution column (the
//! execution is derived from the produced-head handoff binding), and
//! endpoint revisions missing from retained history are named by a
//! relation gap while the link is still exported as unsupported
//! evidence — matching `docs/proposal-rui-evidence-export.md` T2.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use workloop_core::command::FindingChoice;
use workloop_core::domain::*;
use workloop_core::store::{Effect, StoreError};
use workloop_evidence_protocol::*;

use crate::artifact_class::ArtifactClass;
use crate::sqlite::{SqliteStore, internal, map_err};

/// One `event_log` row: the recorded sequence, the row kind and the
/// verbatim payload JSON.
#[derive(Debug, Clone)]
pub struct HistoryEvent {
    pub seq: u64,
    pub kind: String,
    pub payload: String,
}

/// A consistent point-in-time read of the recorded history: the
/// identity binding, the upper watermark, the retained boundary, and
/// every log row — all captured inside one deferred transaction so a
/// concurrent writer can never tear the snapshot.
#[derive(Debug, Clone)]
pub struct HistorySnapshot {
    pub installation_id: String,
    pub store_epoch: String,
    /// `MAX(seq)` — 0 on an empty log.
    pub watermark: u64,
    /// `MIN(seq)` — 0 on an empty log.
    pub retained_from_seq: u64,
    /// `effect` rows in sequence order (the only rows that project to
    /// evidence records).
    pub events: Vec<HistoryEvent>,
    /// Explicit artifact classifications recorded beside the history.
    /// An entry is authoritative for its artifact id; unclassified
    /// artifacts fall back to id/payload classification.
    pub artifact_classes: BTreeMap<String, ArtifactClass>,
}

fn artifact_id(seq: u64) -> String {
    format!("event:{seq}")
}

fn descriptor(
    installation_id: &str,
    store_epoch: &str,
    seq: u64,
    payload: &[u8],
) -> ArtifactDescriptor {
    ArtifactDescriptor {
        artifact_id: artifact_id(seq),
        identity: ArtifactIdentity {
            installation_id: installation_id.into(),
            store_epoch: store_epoch.into(),
            source_id: "event_log".into(),
            revision: format!("seq-{seq}"),
            revision_seq: seq,
        },
        sha256: sha256(payload),
        byte_length: payload.len() as u64,
        media_type: "application/json".into(),
        // The artifact command cannot serve payloads over
        // MAX_ARTIFACT_BYTES, so the descriptor must not advertise
        // them as available; the citing record carries an artifact
        // gap instead.
        availability: if payload.len() > MAX_ARTIFACT_BYTES {
            Availability::Unavailable
        } else {
            Availability::Available
        },
    }
}

/// The source reference and descriptor for another retained event —
/// the cited fragment behind a binding the producer derived rather
/// than recorded. `snap.events` is seq-ordered, so the lookup is a
/// binary search; a seq naming no retained effect row cites nothing.
fn supporting_ref(snap: &HistorySnapshot, seq: u64) -> Option<(SourceRef, ArtifactDescriptor)> {
    let event = snap
        .events
        .binary_search_by_key(&seq, |e| e.seq)
        .ok()
        .map(|i| &snap.events[i])?;
    let payload = event.payload.as_bytes();
    Some((
        SourceRef {
            artifact_id: artifact_id(event.seq),
            sha256: sha256(payload),
            locator: Locator::JsonPointer {
                pointer: String::new(),
            },
        },
        descriptor(&snap.installation_id, &snap.store_epoch, event.seq, payload),
    ))
}

impl SqliteStore {
    /// A consistent read of the recorded history for the evidence
    /// exporter. Read-only: the query holds a deferred transaction, no
    /// row is ever written and no authority table is consulted as the
    /// record source.
    pub fn evidence_history(&self) -> Result<HistorySnapshot, StoreError> {
        let mut guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(internal)?;
        let installation_id: String = tx
            .query_row(
                "SELECT value FROM store_meta WHERE key='store_id'",
                [],
                |r| r.get(0),
            )
            .map_err(map_err)?;
        let watermark: i64 = tx
            .query_row("SELECT MAX(seq) FROM event_log", [], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .map_err(map_err)?
            .unwrap_or(0);
        let retained_from: i64 = tx
            .query_row("SELECT MIN(seq) FROM event_log", [], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .map_err(map_err)?
            .unwrap_or(0);
        let mut stmt = tx
            .prepare(
                "SELECT seq, kind, payload FROM event_log \
                 WHERE kind='effect' ORDER BY seq",
            )
            .map_err(internal)?;
        let events = stmt
            .query_map([], |r| {
                Ok(HistoryEvent {
                    seq: r.get::<_, i64>(0)? as u64,
                    kind: r.get(1)?,
                    payload: r.get(2)?,
                })
            })
            .map_err(map_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_err)?;
        drop(stmt);
        let mut classes = tx
            .prepare("SELECT key, value FROM store_meta WHERE key LIKE 'artifact_class:%'")
            .map_err(internal)?;
        let artifact_classes = classes
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(map_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_err)?
            .into_iter()
            .filter_map(|(key, value)| {
                let id = key.strip_prefix("artifact_class:")?.to_string();
                let class = ArtifactClass::parse(&value)?;
                Some((id, class))
            })
            .collect();
        drop(classes);
        let store_epoch: String = tx
            .query_row(
                "SELECT value FROM store_meta WHERE key='history_epoch'",
                [],
                |r| r.get(0),
            )
            .map_err(map_err)?;
        tx.commit().map_err(internal)?;
        Ok(HistorySnapshot {
            installation_id,
            store_epoch,
            watermark: watermark as u64,
            retained_from_seq: retained_from as u64,
            events,
            artifact_classes,
        })
    }

    /// Record an explicit class for one artifact id. Classification is
    /// metadata only: it never changes the retained bytes or their
    /// digest. Re-registering the same id overwrites the earlier class.
    pub fn set_artifact_class(
        &self,
        artifact_id: &str,
        class: ArtifactClass,
    ) -> Result<(), StoreError> {
        let guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        guard
            .execute(
                "INSERT INTO store_meta (key, value) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![format!("artifact_class:{artifact_id}"), class.as_str()],
            )
            .map_err(map_err)?;
        Ok(())
    }

    /// Read one explicit artifact class, if registered.
    pub fn artifact_class(&self, artifact_id: &str) -> Result<Option<ArtifactClass>, StoreError> {
        let guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let value: Option<String> = guard
            .query_row(
                "SELECT value FROM store_meta WHERE key=?1",
                params![format!("artifact_class:{artifact_id}")],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_err)?;
        Ok(value.as_deref().and_then(ArtifactClass::parse))
    }

    /// Every explicitly registered artifact class.
    pub fn artifact_classes(&self) -> Result<BTreeMap<String, ArtifactClass>, StoreError> {
        Ok(self.evidence_history()?.artifact_classes)
    }

    /// One retained `effect` row by sequence — the byte source behind
    /// an `event:<seq>` artifact id.
    pub fn evidence_event_payload(&self, seq: u64) -> Result<Option<String>, StoreError> {
        let guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let payload: Option<String> = guard
            .query_row(
                "SELECT payload FROM event_log WHERE seq=?1 AND kind='effect'",
                params![seq as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_err)?;
        Ok(payload)
    }

    /// The installation identity binding a token must name.
    pub fn evidence_identity(&self) -> Result<(String, String), StoreError> {
        let guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let installation_id: String = guard
            .query_row(
                "SELECT value FROM store_meta WHERE key='store_id'",
                [],
                |r| r.get(0),
            )
            .map_err(map_err)?;
        let store_epoch: String = guard
            .query_row(
                "SELECT value FROM store_meta WHERE key='history_epoch'",
                [],
                |r| r.get(0),
            )
            .map_err(map_err)?;
        Ok((installation_id, store_epoch))
    }
}

/// Resolve an `event:<seq>` artifact id to its descriptor and retained
/// bytes. Every failure mode maps to a frozen reason code; the returned
/// bytes are the verbatim recorded payload.
pub fn artifact_bytes(
    request: &ArtifactRequest,
    snapshot: &HistorySnapshot,
) -> Result<(ArtifactDescriptor, Vec<u8>), ExportError> {
    let seq = artifact_seq(&request.artifact_id).ok_or_else(|| {
        ExportError::new(
            ErrorCode::ArtifactUnavailable,
            "artifact id is not in the retained namespace",
        )
    })?;
    let event = snapshot
        .events
        .iter()
        .find(|e| e.seq == seq)
        .ok_or_else(|| {
            ExportError::new(ErrorCode::ArtifactUnavailable, "artifact is not retained")
        })?;
    let bytes = event.payload.as_bytes();
    if bytes.len() > MAX_ARTIFACT_BYTES {
        return Err(ExportError::new(
            ErrorCode::ArtifactTooLarge,
            "artifact exceeds the retained byte limit",
        ));
    }
    let d = descriptor(&snapshot.installation_id, &snapshot.store_epoch, seq, bytes);
    if d.sha256 != request.sha256 {
        return Err(ExportError::new(
            ErrorCode::DigestMismatch,
            "requested digest does not match the retained bytes",
        ));
    }
    Ok((d, bytes.to_vec()))
}

/// Descriptor + bytes behind one `event:<seq>` id, resolved against the
/// store for the artifact retrieval command.
pub fn artifact_from_payload(
    request: &ArtifactRequest,
    installation_id: &str,
    store_epoch: &str,
    seq: u64,
    payload: &str,
) -> Result<(ArtifactDescriptor, Vec<u8>), ExportError> {
    // The descriptor id is minted from `seq`; a request naming a
    // non-canonical alias (`event:01`) would produce a receipt whose
    // id fails request/result equality, so only the canonical spelling
    // of this seq is served.
    if request.artifact_id != artifact_id(seq) {
        return Err(ExportError::new(
            ErrorCode::ArtifactUnavailable,
            "artifact id is not in the retained namespace",
        ));
    }
    let bytes = payload.as_bytes();
    if bytes.len() > MAX_ARTIFACT_BYTES {
        return Err(ExportError::new(
            ErrorCode::ArtifactTooLarge,
            "artifact exceeds the retained byte limit",
        ));
    }
    let d = descriptor(installation_id, store_epoch, seq, bytes);
    if d.sha256 != request.sha256 {
        return Err(ExportError::new(
            ErrorCode::DigestMismatch,
            "requested digest does not match the retained bytes",
        ));
    }
    Ok((d, bytes.to_vec()))
}

/// The retained row sequence an `event:<seq>` artifact id names. Only
/// the canonical spelling resolves: `event:01` parses to seq 1 but
/// names no retained artifact, so the minted id must round-trip.
pub fn artifact_seq(id: &str) -> Option<u64> {
    let seq = id.strip_prefix("event:")?.parse::<u64>().ok()?;
    (artifact_id(seq) == id).then_some(seq)
}

/// The class of one page descriptor under the snapshot's explicit
/// classification, id convention, and payload class field.
pub fn classify_descriptor(
    snap: &HistorySnapshot,
    descriptor: &ArtifactDescriptor,
) -> ArtifactClass {
    let payload = artifact_seq(&descriptor.artifact_id)
        .and_then(|seq| snap.events.iter().find(|event| event.seq == seq))
        .map(|event| event.payload.as_bytes());
    crate::artifact_class::classify_artifact(
        &descriptor.artifact_id,
        payload,
        snap.artifact_classes.get(&descriptor.artifact_id).copied(),
    )
}

// ------------------------------------------------------------------
// Recorded-history projection
// ------------------------------------------------------------------

const EXPORTER_AUTHOR: &str = "taisan-evidence-exporter";

fn derived(rationale: &str) -> Assertion {
    Assertion::ModelInterpretation {
        author: EXPORTER_AUTHOR.into(),
        tool: "taisan".into(),
        version: PROTOCOL.into(),
        rationale: rationale.into(),
    }
}

fn exec_state(s: &ExecutionState) -> State {
    match s {
        ExecutionState::Queued
        | ExecutionState::Ready
        | ExecutionState::Waiting
        | ExecutionState::Paused => State::Pending,
        ExecutionState::Running | ExecutionState::Stopping => State::Running,
        ExecutionState::Uncertain => State::Unknown,
        ExecutionState::Succeeded => State::Passed,
        ExecutionState::Failed => State::Failed,
        ExecutionState::Cancelled => State::Cancelled,
    }
}

fn invocation_state(s: &InvocationState) -> State {
    match s {
        InvocationState::Prepared => State::Pending,
        InvocationState::Running => State::Running,
        InvocationState::Done => State::Passed,
        InvocationState::Abandoned => State::Failed,
        InvocationState::Unknown => State::Unknown,
    }
}

fn completeness(s: &str) -> Completeness {
    if s == "complete" {
        Completeness::Complete
    } else if s.starts_with("partial") {
        Completeness::Partial
    } else {
        Completeness::Unknown
    }
}

fn rref(source_id: &str, revision: &str) -> RevisionRef {
    RevisionRef {
        source_id: source_id.into(),
        revision: revision.into(),
    }
}

fn work_sid(work_id: &str) -> String {
    format!("work:{work_id}")
}
fn exec_sid(execution_id: &str) -> String {
    format!("exec:{execution_id}")
}
fn round_sid(round_id: &str) -> String {
    format!("round:{round_id}")
}
fn finding_sid(occurrence_id: &str) -> String {
    format!("finding:{occurrence_id}")
}
fn repair_sid(artifact_id: &str) -> String {
    format!("repair:{artifact_id}")
}
fn invocation_sid(invocation_id: &str) -> String {
    format!("inv:{invocation_id}")
}
fn verification_sid(check_id: &str) -> String {
    format!("verification:{check_id}")
}

/// One stream item: the projected wire record plus the artifact
/// descriptors its source references name and the gaps its own
/// evidence requires. `refs` lists every endpoint revision the record
/// needs resolved so the assembly pass can emit missing-endpoint gaps.
#[derive(Debug)]
struct StreamItem {
    record: Record,
    kind: RecordKind,
    refs: Vec<RevisionRef>,
    artifacts: Vec<ArtifactDescriptor>,
    gaps: Vec<Gap>,
}

/// Replay state over the recorded effects. Revisions are minted in
/// stream order so `revision_seq` is per-source monotone; `produced`
/// records every revision that exists (emitted or skipped) with the
/// change sequence that minted it, which is what the endpoint-gap check
/// resolves against.
#[derive(Default)]
struct Replay {
    /// source_id -> next revision_seq to mint.
    next_seq: BTreeMap<String, u64>,
    /// source_id -> revision of the latest emitted revision.
    latest: BTreeMap<String, String>,
    /// (source_id, revision) -> minting change_seq, for every projected
    /// revision including ones whose record was skipped.
    produced: BTreeMap<(String, String), u64>,
    /// (source_id, revision) present in this export's stream.
    emitted: BTreeSet<(String, String)>,
    specs: BTreeMap<(String, u64), SpecSnapshot>,
    spec_entities: BTreeSet<(String, u64)>,
    /// exec_id -> recorded_seq of the last emitted outcome revision.
    outcomes: BTreeMap<String, i64>,
    /// work_id -> sealed rounds in sequence order.
    work_rounds: BTreeMap<String, Vec<(u64, String)>>,
    /// round_id -> the sealed round and its immutable occurrence set.
    rounds: BTreeMap<String, (ReviewRound, Vec<FindingOccurrence>)>,
    /// round_id -> recorded judgments in sequence order.
    judgments: BTreeMap<String, Vec<(u64, RoundJudgment)>>,
    /// occurrence_id -> (change_seq, revision) of every emitted finding
    /// revision, in order.
    finding_revs: BTreeMap<String, Vec<(u64, String)>>,
    /// produced head -> (seq, artifact_id, execution_id) bindings from
    /// the durable handoff records.
    produced_heads: BTreeMap<String, Vec<(u64, String, String)>>,
    /// artifact_id -> (work_id, produced head) of the latest recorded
    /// handoff state.
    handoffs: BTreeMap<String, (String, Option<String>)>,
    /// request_id -> (seq, InvocationRequest).
    requests: BTreeMap<String, (u64, InvocationRequest)>,
    /// work_id -> fix-role requests in sequence order.
    fix_requests: BTreeMap<String, Vec<(u64, String)>>,
    /// execution_id -> work_id, for entities that name no work.
    exec_work: BTreeMap<String, String>,
    minted_links: BTreeSet<String>,
    /// Orphan gaps (skipped records) riding the stream by sequence.
    orphan_gaps: Vec<(u64, Gap)>,
}

struct Materialized {
    items: Vec<StreamItem>,
    /// Orphan gaps (skipped records) in sequence order; they ride the
    /// next page containing an item at or after their change_seq.
    gaps: Vec<(u64, Gap)>,
    produced: BTreeMap<(String, String), u64>,
    emitted: BTreeSet<(String, String)>,
}

impl Replay {
    /// Mint the next revision for `source_id` at `seq`, registering it
    /// in `produced` so endpoint names resolve. Used directly when the
    /// record's revision must be known before it is emitted (execution
    /// revisions are named by their outcome entity).
    fn mint(&mut self, source_id: &str, seq: u64) -> (String, u64) {
        let n = self.next_seq.entry(source_id.into()).or_insert(1);
        let rev = format!("r{n}");
        *n += 1;
        self.produced.insert((source_id.into(), rev.clone()), seq);
        (rev, *n - 1)
    }

    /// Mint and emit a record in one step.
    #[allow(clippy::too_many_arguments)]
    fn emit(
        &mut self,
        items: &mut Vec<StreamItem>,
        snap: &HistorySnapshot,
        event: &HistoryEvent,
        source_id: &str,
        work_id: &str,
        assertion: Assertion,
        data: RecordData,
        kind: RecordKind,
        refs: Vec<RevisionRef>,
        gaps: Vec<Gap>,
    ) -> RevisionRef {
        self.emit_supported(
            items,
            snap,
            event,
            source_id,
            work_id,
            assertion,
            data,
            kind,
            refs,
            gaps,
            &[],
        )
    }

    /// Mint and emit a record that cites additional retained events as
    /// supporting fragments — the recorded facts a derived binding
    /// rests on, per the contract's supporting-evidence rule.
    #[allow(clippy::too_many_arguments)]
    fn emit_supported(
        &mut self,
        items: &mut Vec<StreamItem>,
        snap: &HistorySnapshot,
        event: &HistoryEvent,
        source_id: &str,
        work_id: &str,
        assertion: Assertion,
        data: RecordData,
        kind: RecordKind,
        refs: Vec<RevisionRef>,
        gaps: Vec<Gap>,
        support_seqs: &[u64],
    ) -> RevisionRef {
        let (revision, revision_seq) = self.mint(source_id, event.seq);
        self.emit_named(
            items,
            snap,
            event,
            source_id,
            work_id,
            assertion,
            data,
            kind,
            refs,
            gaps,
            revision.clone(),
            revision_seq,
            support_seqs,
        );
        rref(source_id, &revision)
    }

    /// Emit a record under a previously minted revision.
    #[allow(clippy::too_many_arguments)]
    fn emit_named(
        &mut self,
        items: &mut Vec<StreamItem>,
        snap: &HistorySnapshot,
        event: &HistoryEvent,
        source_id: &str,
        work_id: &str,
        assertion: Assertion,
        data: RecordData,
        kind: RecordKind,
        refs: Vec<RevisionRef>,
        gaps: Vec<Gap>,
        revision: String,
        revision_seq: u64,
        support_seqs: &[u64],
    ) {
        let payload_sha = sha256(event.payload.as_bytes());
        let mut source_refs = vec![SourceRef {
            artifact_id: artifact_id(event.seq),
            sha256: payload_sha,
            locator: Locator::JsonPointer {
                pointer: String::new(),
            },
        }];
        let mut artifacts = vec![descriptor(
            &snap.installation_id,
            &snap.store_epoch,
            event.seq,
            event.payload.as_bytes(),
        )];
        let mut cited = BTreeSet::from([event.seq]);
        for &seq in support_seqs {
            if cited.insert(seq)
                && let Some((source_ref, artifact)) = supporting_ref(snap, seq)
            {
                source_refs.push(source_ref);
                artifacts.push(artifact);
            }
        }
        let mut gaps = gaps;
        for art in &artifacts {
            if art.availability != Availability::Available {
                gaps.push(Gap {
                    code: GapCode::SourceUnavailable,
                    scope: GapScope::Artifact {
                        artifact_id: art.artifact_id.clone(),
                        sha256: art.sha256.clone(),
                    },
                    evidence_class: EvidenceClass::Artifact,
                    reason: "the retained payload exceeds the artifact byte limit".into(),
                });
            }
        }
        let body = RecordBody {
            record_kind: kind,
            source_id: source_id.into(),
            revision: revision.clone(),
            revision_seq,
            work_id: work_id.into(),
            assertion,
            source_refs,
            data,
        };
        let body_json = serde_json::to_string(&body).expect("record body serializes");
        items.push(StreamItem {
            record: Record {
                source_id: source_id.into(),
                revision: revision.clone(),
                revision_seq,
                change_seq: event.seq,
                body_sha256: sha256(body_json.as_bytes()),
                body_json,
            },
            kind,
            refs,
            artifacts,
            gaps,
        });
        self.emitted.insert((source_id.into(), revision.clone()));
        self.latest.insert(source_id.into(), revision);
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_link(
        &mut self,
        items: &mut Vec<StreamItem>,
        snap: &HistorySnapshot,
        event: &HistoryEvent,
        work_id: &str,
        relation: Relation,
        from: RevisionRef,
        to: RevisionRef,
        assertion: Assertion,
    ) {
        self.emit_link_supported(
            items,
            snap,
            event,
            work_id,
            relation,
            from,
            to,
            assertion,
            &[],
        );
    }

    /// Emit a link whose assertion rests on recorded fragments beyond
    /// the minting event; `support_seqs` names those events so the
    /// link's source references cite them.
    #[allow(clippy::too_many_arguments)]
    fn emit_link_supported(
        &mut self,
        items: &mut Vec<StreamItem>,
        snap: &HistorySnapshot,
        event: &HistoryEvent,
        work_id: &str,
        relation: Relation,
        from: RevisionRef,
        to: RevisionRef,
        assertion: Assertion,
        support_seqs: &[u64],
    ) {
        let relation_tag = serde_json::to_value(relation)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let from_key = format!("{}:{}", from.source_id, from.revision);
        let to_key = format!("{}:{}", to.source_id, to.revision);
        let source_id = format!("link:{relation_tag}:{from_key}:{to_key}");
        if !self.minted_links.insert(source_id.clone()) {
            return;
        }
        let refs = vec![from.clone(), to.clone()];
        self.emit_supported(
            items,
            snap,
            event,
            &source_id,
            work_id,
            assertion,
            RecordData::Link { relation, from, to },
            RecordKind::Link,
            refs,
            vec![],
            support_seqs,
        );
    }

    fn latest_ref(&self, source_id: &str) -> Option<RevisionRef> {
        self.latest.get(source_id).map(|rev| rref(source_id, rev))
    }

    /// The latest revision an entity carries, whether or not its record
    /// was emitted — used to name skipped endpoints exactly.
    fn any_ref(&self, source_id: &str) -> Option<RevisionRef> {
        self.produced
            .iter()
            .filter(|((sid, _), _)| sid == source_id)
            .max_by_key(|((_, rev), seq)| (*seq, rev.clone()))
            .map(|((sid, rev), _)| rref(sid, rev))
    }

    /// The recorded execution behind a produced head: a review round
    /// names no execution, so the binding is derived from the durable
    /// artifact-handoff record whose `produced` matches the reviewed
    /// head — the only recorded fact that binds the review's target to
    /// the execution that produced it. Returns the handoff event's seq
    /// alongside the execution id so derived records can cite the
    /// supporting fragment.
    fn execution_for_head(&self, head: &str, before_seq: u64) -> Option<(u64, String)> {
        self.produced_heads
            .get(head)?
            .iter()
            .filter(|(seq, _, _)| *seq <= before_seq)
            .max_by_key(|(seq, _, _)| *seq)
            .map(|(seq, _, exec)| (*seq, exec.clone()))
    }

    /// The judgment that authorized a fix dispatch: the latest judgment
    /// on the work's latest sealed round, recorded before the fix
    /// request, that selected `fix` for at least one occurrence — the
    /// exact record the admission gate consulted.
    fn fix_authorizer(&self, work_id: &str, request_seq: u64) -> Option<(String, u64)> {
        let round_id = self
            .work_rounds
            .get(work_id)?
            .iter()
            .filter(|(seq, _)| *seq <= request_seq)
            .max_by_key(|(seq, _)| *seq)
            .map(|(_, id)| id.clone())?;
        self.judgments
            .get(&round_id)?
            .iter()
            .rev()
            .find(|(seq, j)| {
                *seq <= request_seq && j.decisions.iter().any(|d| d.choice == FindingChoice::Fix)
            })
            .map(|(seq, _)| (round_id, *seq))
    }

    /// The finding revision a judgment minted for one occurrence — the
    /// disposition revision recorded at or before `seq`.
    fn finding_rev_at(&self, occurrence_id: &str, seq: u64) -> Option<RevisionRef> {
        self.finding_revs
            .get(occurrence_id)?
            .iter()
            .rev()
            .find_map(|(s, rev)| {
                if *s <= seq {
                    Some(rref(&finding_sid(occurrence_id), rev))
                } else {
                    None
                }
            })
    }

    fn handoff_work(&self, artifact_id: &str) -> Option<String> {
        self.handoffs.get(artifact_id).map(|(w, _)| w.clone())
    }

    fn repair_head(&self, artifact_id: &str) -> Option<String> {
        self.handoffs.get(artifact_id).and_then(|(_, h)| h.clone())
    }
}

/// The requirement/invariant entities a spec revision declares. They
/// are minted once per recorded spec revision — the spec payload is
/// immutable — and each work revision references the revisions its spec
/// recorded.
fn spec_entities(
    rep: &mut Replay,
    items: &mut Vec<StreamItem>,
    snap: &HistorySnapshot,
    event: &HistoryEvent,
    spec: &SpecSnapshot,
) -> (Vec<RevisionRef>, Vec<RevisionRef>) {
    if rep
        .spec_entities
        .insert((spec.work_id.0.clone(), spec.revision))
    {
        let work_ref = rep.latest_ref(&work_sid(&spec.work_id.0));
        let mut emit_one =
            |rep: &mut Replay, source_id: String, text: String, is_requirement: bool| {
                let data = if is_requirement {
                    RecordData::Entity {
                        entity: EntityPayload::Requirement {
                            requirement_id: source_id.clone(),
                            text: Some(text),
                            category: None,
                            provenance: Provenance::Dispatch,
                            source_revision: work_ref.clone(),
                        },
                    }
                } else {
                    RecordData::Entity {
                        entity: EntityPayload::Invariant {
                            invariant_id: source_id.clone(),
                            text: Some(text),
                            category: None,
                            provenance: Provenance::Dispatch,
                            source_revision: work_ref.clone(),
                        },
                    }
                };
                let refs: Vec<_> = work_ref.iter().cloned().collect();
                rep.emit(
                    items,
                    snap,
                    event,
                    &source_id,
                    &spec.work_id.0,
                    Assertion::Observed,
                    data,
                    RecordKind::Entity,
                    refs,
                    vec![],
                );
            };
        for (i, argv) in spec.payload.verification_argv.iter().enumerate() {
            emit_one(
                rep,
                format!("req:{}:{}:v{i}", spec.work_id.0, spec.revision),
                argv.join(" "),
                true,
            );
        }
        for (i, check) in spec.payload.required_checks.iter().enumerate() {
            emit_one(
                rep,
                format!("req:{}:{}:c{i}", spec.work_id.0, spec.revision),
                check.clone(),
                true,
            );
        }
        for (i, text) in spec.payload.constraints.iter().enumerate() {
            emit_one(
                rep,
                format!("inv:{}:{}:{i}", spec.work_id.0, spec.revision),
                text.clone(),
                false,
            );
        }
    }
    let reqs = (0..spec.payload.verification_argv.len())
        .map(|i| format!("req:{}:{}:v{i}", spec.work_id.0, spec.revision))
        .chain(
            (0..spec.payload.required_checks.len())
                .map(|i| format!("req:{}:{}:c{i}", spec.work_id.0, spec.revision)),
        )
        .map(|sid| rep.any_ref(&sid).unwrap_or_else(|| rref(&sid, "r1")))
        .collect();
    let invs = (0..spec.payload.constraints.len())
        .map(|i| format!("inv:{}:{}:{i}", spec.work_id.0, spec.revision))
        .map(|sid| rep.any_ref(&sid).unwrap_or_else(|| rref(&sid, "r1")))
        .collect();
    (reqs, invs)
}

/// Emit one work revision plus its requirement/invariant membership
/// links. A work whose spec was never recorded exports `intent: null`
/// and the intent-class gap the contract requires — never invented
/// text.
fn emit_work(
    rep: &mut Replay,
    items: &mut Vec<StreamItem>,
    snap: &HistorySnapshot,
    event: &HistoryEvent,
    work: &Work,
) {
    let sid = work_sid(&work.id.0);
    let spec = rep.specs.get(&(work.id.0.clone(), work.spec)).cloned();
    let (requirements, constraints, intent) = match &spec {
        Some(spec) => {
            let (r, c) = spec_entities(rep, items, snap, event, spec);
            (r, c, Some(spec.payload.intent.clone()))
        }
        None => (vec![], vec![], None),
    };
    let mut gaps = vec![];
    let refs: Vec<_> = requirements.iter().chain(&constraints).cloned().collect();
    if intent.is_none() {
        // The gap names the revision about to be minted (r<next>).
        let n = *rep.next_seq.get(&sid).unwrap_or(&1);
        gaps.push(Gap {
            code: GapCode::NotRecorded,
            scope: GapScope::Revision {
                record: rref(&sid, &format!("r{n}")),
            },
            evidence_class: EvidenceClass::Intent,
            reason: "the dispatch-time spec snapshot was not recorded".into(),
        });
    }
    let work_rev = rep.emit(
        items,
        snap,
        event,
        &sid,
        &work.id.0,
        Assertion::Observed,
        RecordData::Entity {
            entity: EntityPayload::Work {
                work_id: work.id.0.clone(),
                version: work.version,
                intent,
                requirements: requirements.clone(),
                constraints: constraints.clone(),
                dispatch_source: None,
            },
        },
        RecordKind::Entity,
        refs,
        gaps,
    );
    for req in requirements {
        rep.emit_link(
            items,
            snap,
            event,
            &work.id.0,
            Relation::WorkHasRequirement,
            work_rev.clone(),
            req,
            Assertion::Observed,
        );
    }
    for inv in constraints {
        rep.emit_link(
            items,
            snap,
            event,
            &work.id.0,
            Relation::WorkHasInvariant,
            work_rev.clone(),
            inv,
            Assertion::Observed,
        );
    }
}

/// Emit one execution revision. When a new terminal outcome is recorded
/// the outcome entity is emitted first, bound to the already-minted
/// execution revision, so the execution body can name it exactly.
fn emit_execution(
    rep: &mut Replay,
    items: &mut Vec<StreamItem>,
    snap: &HistorySnapshot,
    event: &HistoryEvent,
    execution: &Execution,
) {
    let sid = exec_sid(&execution.id.0);
    let wsid = work_sid(&execution.work_id.0);
    let first = !rep.latest.contains_key(&sid);
    let (rev, rev_seq) = rep.mint(&sid, event.seq);
    let exec_ref = rref(&sid, &rev);
    let outcome_sid = format!("outcome:{}", execution.id.0);
    let mut outcome_ref = None;
    if let Some(o) = &execution.outcome {
        if rep.outcomes.get(&execution.id.0) == Some(&o.recorded_seq) {
            outcome_ref = rep.latest_ref(&outcome_sid);
        } else {
            rep.outcomes.insert(execution.id.0.clone(), o.recorded_seq);
            outcome_ref = Some(rep.emit(
                items,
                snap,
                event,
                &outcome_sid,
                &execution.work_id.0,
                Assertion::Observed,
                RecordData::Entity {
                    entity: EntityPayload::Outcome {
                        outcome_id: outcome_sid.clone(),
                        execution: exec_ref.clone(),
                        scope: OutcomeScope::Execution,
                        state: exec_state(&o.state),
                        head: None,
                    },
                },
                RecordKind::Entity,
                vec![exec_ref.clone()],
                vec![],
            ));
        }
    }
    let work_ref = rep.latest_ref(&wsid).unwrap_or_else(|| rref(&wsid, "r1"));
    let refs: Vec<_> = [work_ref.clone()]
        .into_iter()
        .chain(outcome_link_iter(&outcome_ref))
        .collect();
    rep.emit_named(
        items,
        snap,
        event,
        &sid,
        &execution.work_id.0,
        Assertion::Observed,
        RecordData::Entity {
            entity: EntityPayload::Execution {
                execution_id: execution.id.0.clone(),
                work: work_ref,
                state: exec_state(&execution.state),
                outcome: outcome_ref.clone(),
                base_head: None,
                result_head: None,
                started_at: None,
                finished_at: None,
            },
        },
        RecordKind::Entity,
        refs,
        vec![],
        rev.clone(),
        rev_seq,
        &[],
    );
    if first {
        rep.emit_link(
            items,
            snap,
            event,
            &execution.work_id.0,
            Relation::WorkHasExecution,
            rep.latest_ref(&wsid).unwrap_or_else(|| rref(&wsid, "r1")),
            exec_ref.clone(),
            Assertion::Observed,
        );
    }
    if let Some(o_ref) = outcome_ref {
        rep.emit_link(
            items,
            snap,
            event,
            &execution.work_id.0,
            Relation::ExecutionHasOutcome,
            exec_ref,
            o_ref,
            Assertion::Observed,
        );
    }
}

fn outcome_link_iter(o: &Option<RevisionRef>) -> impl Iterator<Item = RevisionRef> + '_ {
    o.iter().cloned()
}

/// Emit one invocation revision; the first sighting mints the
/// execution-membership link.
fn emit_invocation(
    rep: &mut Replay,
    items: &mut Vec<StreamItem>,
    snap: &HistorySnapshot,
    event: &HistoryEvent,
    invocation: &Invocation,
) {
    let sid = invocation_sid(&invocation.id.0);
    let esid = exec_sid(&invocation.execution_id.0);
    let Some(work_id) = rep.exec_work.get(&invocation.execution_id.0).cloned() else {
        // No recorded execution binds this invocation's Work: name the
        // skipped revision rather than emit a record with no work.
        let (rev, _) = rep.mint(&sid, event.seq);
        rep.orphan_gaps.push((
            event.seq,
            Gap {
                code: GapCode::NotRecorded,
                scope: GapScope::Revision {
                    record: rref(&sid, &rev),
                },
                evidence_class: EvidenceClass::Body,
                reason: "the invocation's execution binding is not recorded".into(),
            },
        ));
        return;
    };
    let first = !rep.latest.contains_key(&sid);
    let exec_ref = rep.latest_ref(&esid).unwrap_or_else(|| rref(&esid, "r1"));
    let inv_ref = rep.emit(
        items,
        snap,
        event,
        &sid,
        &work_id,
        Assertion::Observed,
        RecordData::Entity {
            entity: EntityPayload::Invocation {
                invocation_id: invocation.id.0.clone(),
                execution: exec_ref.clone(),
                process_attempt_id: invocation.request_id.clone(),
                state: invocation_state(&invocation.state),
            },
        },
        RecordKind::Entity,
        vec![exec_ref.clone()],
        vec![],
    );
    if first {
        rep.emit_link(
            items,
            snap,
            event,
            &work_id,
            Relation::ExecutionHasInvocation,
            exec_ref,
            inv_ref,
            Assertion::Observed,
        );
    }
}

/// Emit a sealed review round and its occurrence set. The recorded
/// facts are the round and its occurrences; the execution binding is
/// derived from the produced-head handoff record, so records carrying
/// it are asserted as model interpretation rather than observed truth.
/// A round whose binding cannot be derived at all is itself a gap —
/// naming its revision — never a fabricated execution reference.
fn emit_round(
    rep: &mut Replay,
    items: &mut Vec<StreamItem>,
    snap: &HistorySnapshot,
    event: &HistoryEvent,
    round: &ReviewRound,
    occurrences: &[FindingOccurrence],
) {
    let sid = round_sid(&round.id);
    let head = round.target.head.object.clone();
    let exec_binding = rep.execution_for_head(&head, event.seq);
    let exec_ref = exec_binding
        .as_ref()
        .and_then(|(_, eid)| rep.latest_ref(&exec_sid(eid)));
    // The handoff event that produced the reviewed head is the
    // recorded fragment the derived execution binding rests on.
    let support: &[u64] = match &exec_binding {
        Some((seq, _)) => std::slice::from_ref(seq),
        None => &[],
    };
    let Some(exec_ref) = exec_ref else {
        // No recorded binding exists: name the round's own revision as
        // missing evidence rather than invent an endpoint.
        let (rev, _) = rep.mint(&sid, event.seq);
        rep.orphan_gaps.push((
            event.seq,
            Gap {
                code: GapCode::NotRecorded,
                scope: GapScope::Revision {
                    record: rref(&sid, &rev),
                },
                evidence_class: EvidenceClass::Body,
                reason: "the round's execution binding is not recorded".into(),
            },
        ));
        // Occurrences still export under the (absent) round reference;
        // the assembly pass emits their missing-endpoint gaps.
        for occ in occurrences {
            emit_finding(rep, items, snap, event, round, occ, None);
        }
        return;
    };
    let assertion = derived(
        "the round's execution binding is derived from the recorded \
         handoff whose produced head matches the reviewed head",
    );
    let round_ref = rep.emit_supported(
        items,
        snap,
        event,
        &sid,
        &round.work_id.0,
        assertion.clone(),
        RecordData::Entity {
            entity: EntityPayload::ReviewRound {
                round_id: round.id.clone(),
                execution: exec_ref.clone(),
                change_id: format!(
                    "{}@{}",
                    round.target.change.repository.repository.as_str(),
                    round.target.change.change_request.as_str()
                ),
                reviewed_head: Some(head.clone()),
                completeness: completeness(&round.completeness),
                observation_id: round.source_revision.clone(),
            },
        },
        RecordKind::Entity,
        vec![exec_ref.clone()],
        vec![],
        support,
    );
    rep.emit_link_supported(
        items,
        snap,
        event,
        &round.work_id.0,
        Relation::ExecutionHasRound,
        exec_ref,
        round_ref.clone(),
        assertion,
        support,
    );
    for occ in occurrences {
        emit_finding(rep, items, snap, event, round, occ, Some(&round_ref));
    }
}

/// Emit one finding occurrence revision. `round_ref` is the round
/// revision when emitted or `r1` when the round itself was skipped.
/// The recorded facts are the occurrence identity, its source key, and
/// the sealing round — the finding body text is never retained, so the
/// record carries `body: null` and the body-class gap the contract
/// requires.
fn emit_finding(
    rep: &mut Replay,
    items: &mut Vec<StreamItem>,
    snap: &HistorySnapshot,
    event: &HistoryEvent,
    round: &ReviewRound,
    occ: &FindingOccurrence,
    round_ref: Option<&RevisionRef>,
) {
    let sid = finding_sid(&occ.id);
    let n = *rep.next_seq.get(&sid).unwrap_or(&1);
    let pending_rev = format!("r{n}");
    let body_gap = Gap {
        code: GapCode::NotRecorded,
        scope: GapScope::Revision {
            record: rref(&sid, &pending_rev),
        },
        evidence_class: EvidenceClass::Body,
        reason: "finding body text is not retained; the occurrence \
                 identity and recorded disposition carry the evidence"
            .into(),
    };
    let round_ref = round_ref.cloned().unwrap_or_else(|| {
        rep.any_ref(&round_sid(&occ.round_id))
            .unwrap_or_else(|| rref(&round_sid(&occ.round_id), "r1"))
    });
    emit_finding_body(
        rep,
        items,
        snap,
        event,
        round,
        occ,
        round_ref,
        Disposition::Unknown,
        body_gap,
    );
}

#[allow(clippy::too_many_arguments)]
fn emit_finding_body(
    rep: &mut Replay,
    items: &mut Vec<StreamItem>,
    snap: &HistorySnapshot,
    event: &HistoryEvent,
    round: &ReviewRound,
    occ: &FindingOccurrence,
    round_ref: RevisionRef,
    disposition: Disposition,
    body_gap: Gap,
) {
    let sid = finding_sid(&occ.id);
    let finding_ref = rep.emit(
        items,
        snap,
        event,
        &sid,
        &round.work_id.0,
        Assertion::Observed,
        RecordData::Entity {
            entity: EntityPayload::FindingOccurrence {
                occurrence_id: occ.id.clone(),
                source_finding_key: occ.source_key.clone(),
                round: round_ref.clone(),
                reviewed_head: Some(round.target.head.object.clone()),
                body: None,
                disposition,
                category: None,
                invariants: vec![],
            },
        },
        RecordKind::Entity,
        vec![round_ref.clone()],
        vec![body_gap],
    );
    rep.finding_revs
        .entry(occ.id.clone())
        .or_default()
        .push((event.seq, finding_ref.revision.clone()));
    rep.emit_link(
        items,
        snap,
        event,
        &round.work_id.0,
        Relation::RoundHasFinding,
        round_ref,
        finding_ref,
        Assertion::Observed,
    );
}

/// Emit one finding revision per recorded judgment decision, then the
/// review-scope outcome. Every decision mints a new immutable
/// occurrence revision — a correction is never an in-place rewrite and
/// an omitted occurrence keeps its prior disposition.
fn emit_judgment(
    rep: &mut Replay,
    items: &mut Vec<StreamItem>,
    snap: &HistorySnapshot,
    event: &HistoryEvent,
    judgment: &RoundJudgment,
    occurrences: &[FindingOccurrence],
    round: &ReviewRound,
) {
    let by_id: BTreeMap<&str, &FindingOccurrence> =
        occurrences.iter().map(|o| (o.id.as_str(), o)).collect();
    let round_ref = rep
        .any_ref(&round_sid(&judgment.round_id))
        .unwrap_or_else(|| rref(&round_sid(&judgment.round_id), "r1"));
    for d in &judgment.decisions {
        let disposition = match d.choice {
            FindingChoice::Fix | FindingChoice::Wait => Disposition::Open,
            FindingChoice::Accept => Disposition::Accepted,
            FindingChoice::Dismiss => Disposition::Dismissed,
        };
        let Some(occ) = by_id.get(d.occurrence_id.as_str()) else {
            continue;
        };
        let sid = finding_sid(&occ.id);
        let n = *rep.next_seq.get(&sid).unwrap_or(&1);
        let body_gap = Gap {
            code: GapCode::NotRecorded,
            scope: GapScope::Revision {
                record: rref(&sid, &format!("r{n}")),
            },
            evidence_class: EvidenceClass::Body,
            reason: "finding body text is not retained".into(),
        };
        let finding_ref = rep.emit(
            items,
            snap,
            event,
            &sid,
            &round.work_id.0,
            Assertion::Observed,
            RecordData::Entity {
                entity: EntityPayload::FindingOccurrence {
                    occurrence_id: occ.id.clone(),
                    source_finding_key: occ.source_key.clone(),
                    round: round_ref.clone(),
                    reviewed_head: Some(round.target.head.object.clone()),
                    body: None,
                    disposition,
                    category: None,
                    invariants: vec![],
                },
            },
            RecordKind::Entity,
            vec![round_ref.clone()],
            vec![body_gap],
        );
        rep.finding_revs
            .entry(occ.id.clone())
            .or_default()
            .push((event.seq, finding_ref.revision));
    }
    // The review-scope outcome records the judgment itself. Its
    // execution binding derives the same way the round's does.
    let outcome_sid = format!("outcome:review:{}", judgment.id);
    let exec_binding = rep.execution_for_head(&round.target.head.object, event.seq);
    let exec_ref = exec_binding
        .as_ref()
        .and_then(|(_, eid)| rep.latest_ref(&exec_sid(eid)));
    let support: &[u64] = match &exec_binding {
        Some((seq, _)) => std::slice::from_ref(seq),
        None => &[],
    };
    match exec_ref {
        Some(exec_ref) => {
            rep.emit_supported(
                items,
                snap,
                event,
                &outcome_sid,
                &judgment.work_id.0,
                derived(
                    "the judgment outcome's execution binding is derived \
                     from the recorded handoff for the reviewed head",
                ),
                RecordData::Entity {
                    entity: EntityPayload::Outcome {
                        outcome_id: outcome_sid.clone(),
                        execution: exec_ref.clone(),
                        scope: OutcomeScope::Review,
                        state: State::Passed,
                        head: Some(round.target.head.object.clone()),
                    },
                },
                RecordKind::Entity,
                vec![exec_ref],
                vec![],
                support,
            );
        }
        None => {
            let (rev, _) = rep.mint(&outcome_sid, event.seq);
            rep.orphan_gaps.push((
                event.seq,
                Gap {
                    code: GapCode::NotRecorded,
                    scope: GapScope::Revision {
                        record: rref(&outcome_sid, &rev),
                    },
                    evidence_class: EvidenceClass::Body,
                    reason: "the judgment's execution binding is not recorded".into(),
                },
            ));
        }
    }
}

/// Emit a repair-attempt revision for one fix-artifact handoff
/// transition. `execution` is recorded on the handoff; `round` and the
/// addressed-finding set are derived from the judgment that authorized
/// the fix dispatch, so derived bindings are asserted as model
/// interpretation.
fn emit_repair(
    rep: &mut Replay,
    items: &mut Vec<StreamItem>,
    snap: &HistorySnapshot,
    event: &HistoryEvent,
    handoff: &ArtifactHandoff,
) {
    let Some((request_seq, request)) = rep.requests.get(&handoff.request_id).cloned() else {
        return;
    };
    if request.role != InvocationRole::Fix {
        return;
    }
    let sid = repair_sid(&handoff.artifact_id);
    let esid = exec_sid(&handoff.execution_id.0);
    let work_id = handoff.work_id.0.clone();
    let first = !rep.latest.contains_key(&sid);
    let exec_ref = rep.latest_ref(&esid).unwrap_or_else(|| rref(&esid, "r1"));
    let authorizer = rep.fix_authorizer(&work_id, request_seq);
    let (round_ref, addressed, assertion) = match &authorizer {
        Some((round_id, judgment_seq)) => {
            let rsid = round_sid(round_id);
            let r_ref = rep.any_ref(&rsid).unwrap_or_else(|| rref(&rsid, "r1"));
            let fixes = rep
                .judgments
                .get(round_id)
                .and_then(|js| js.iter().find(|(s, _)| s == judgment_seq))
                .map(|(_, j)| {
                    j.decisions
                        .iter()
                        .filter(|d| d.choice == FindingChoice::Fix)
                        .map(|d| d.occurrence_id.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            (
                Some(r_ref),
                fixes
                    .iter()
                    .filter_map(|occ| rep.finding_rev_at(occ, *judgment_seq))
                    .collect::<Vec<_>>(),
                derived(
                    "the repair's round and addressed findings are derived \
                     from the recorded judgment that authorized the fix dispatch",
                ),
            )
        }
        None => (None, vec![], Assertion::Observed),
    };
    // The judgment event that authorized the fix dispatch is the
    // recorded fragment the derived round/finding bindings rest on.
    let support: &[u64] = match &authorizer {
        Some((_, judgment_seq)) => std::slice::from_ref(judgment_seq),
        None => &[],
    };
    let state = match handoff.state {
        HandoffState::Acknowledged => State::Passed,
        HandoffState::Invalid => State::Failed,
        _ => State::Running,
    };
    let sequence = rep.fix_requests.get(&work_id).and_then(|reqs| {
        reqs.iter()
            .position(|(_, id)| id == &handoff.request_id)
            .map(|i| (i + 1) as u64)
    });
    let mut refs = vec![exec_ref.clone()];
    refs.extend(round_ref.iter().cloned());
    let repair_ref = rep.emit_supported(
        items,
        snap,
        event,
        &sid,
        &work_id,
        assertion.clone(),
        RecordData::Entity {
            entity: EntityPayload::RepairAttempt {
                attempt_id: handoff.artifact_id.clone(),
                execution: exec_ref.clone(),
                round: round_ref.clone(),
                target_head: Some(handoff.base.clone()),
                result_head: handoff.produced.clone(),
                state,
                sequence,
            },
        },
        RecordKind::Entity,
        refs,
        vec![],
        support,
    );
    if first {
        rep.emit_link(
            items,
            snap,
            event,
            &work_id,
            Relation::ExecutionHasRepair,
            exec_ref,
            repair_ref.clone(),
            Assertion::Observed,
        );
        if let Some(r_ref) = round_ref {
            rep.emit_link_supported(
                items,
                snap,
                event,
                &work_id,
                Relation::RoundHasRepair,
                r_ref,
                repair_ref.clone(),
                assertion.clone(),
                support,
            );
        }
        for finding_ref in addressed {
            rep.emit_link_supported(
                items,
                snap,
                event,
                &work_id,
                Relation::RepairAddressesFinding,
                repair_ref.clone(),
                finding_ref,
                assertion.clone(),
                support,
            );
        }
    }
}

/// Emit one verification record. A `verification_tests_repair` link is
/// minted only when the tested artifact is a recorded fix handoff whose
/// produced head matches the tested head — the wire-level binding the
/// consumer contract checks.
fn emit_verification(
    rep: &mut Replay,
    items: &mut Vec<StreamItem>,
    snap: &HistorySnapshot,
    event: &HistoryEvent,
    result: &VerificationResult,
) {
    let sid = verification_sid(&result.id);
    let rsid = repair_sid(&result.artifact_id);
    let state = match result.result {
        VerificationOutcome::Pass => State::Passed,
        VerificationOutcome::Fail => State::Failed,
        VerificationOutcome::Inconclusive => State::Unknown,
    };
    let Some(work_id) = rep.handoff_work(&result.artifact_id) else {
        // The tested artifact names no recorded handoff: the Work a
        // verification record must carry is not derivable.
        let (rev, _) = rep.mint(&sid, event.seq);
        rep.orphan_gaps.push((
            event.seq,
            Gap {
                code: GapCode::NotRecorded,
                scope: GapScope::Revision {
                    record: rref(&sid, &rev),
                },
                evidence_class: EvidenceClass::Body,
                reason: "the verification's work binding is not recorded".into(),
            },
        ));
        return;
    };
    let v_ref = rep.emit(
        items,
        snap,
        event,
        &sid,
        &work_id,
        Assertion::Observed,
        RecordData::Entity {
            entity: EntityPayload::Verification {
                check_id: result.id.clone(),
                name: result.requirement.clone(),
                tested_head: Some(result.produced.clone()),
                state,
                scope: VerificationScope::Targeted,
                exit_code: None,
            },
        },
        RecordKind::Entity,
        vec![],
        vec![],
    );
    // Link only when the tested head is the repair's produced head —
    // the contract requires tested_head == result_head on this
    // relation, so a link would otherwise be malformed.
    if let Some(repair_ref) = rep.latest_ref(&rsid)
        && rep
            .repair_head(&result.artifact_id)
            .is_some_and(|h| h == result.produced)
    {
        rep.emit_link(
            items,
            snap,
            event,
            &work_id,
            Relation::VerificationTestsRepair,
            v_ref,
            repair_ref,
            Assertion::Observed,
        );
    }
}

/// Project one recorded effect into the stream. Recorded effects the
/// wire contract has no vocabulary for (reservations, slots, intake,
/// host-action intents, recovery bookkeeping) export nothing; their raw
/// payloads remain reachable through the `event:<seq>` artifacts the
/// exported records cite.
fn project(
    rep: &mut Replay,
    items: &mut Vec<StreamItem>,
    snap: &HistorySnapshot,
    event: &HistoryEvent,
    effect: &Effect,
) {
    match effect {
        Effect::InsertWork { work, spec, .. } => {
            rep.specs
                .insert((work.id.0.clone(), spec.revision), spec.clone());
            emit_work(rep, items, snap, event, work);
        }
        Effect::UpdateWork { work, .. } => {
            emit_work(rep, items, snap, event, work);
        }
        Effect::InsertSpec { spec } => {
            rep.specs
                .insert((spec.work_id.0.clone(), spec.revision), spec.clone());
            spec_entities(rep, items, snap, event, spec);
        }
        Effect::InsertExecution { execution } | Effect::UpdateExecution { execution, .. } => {
            rep.exec_work
                .insert(execution.id.0.clone(), execution.work_id.0.clone());
            emit_execution(rep, items, snap, event, execution);
        }
        Effect::InsertRequest { request } => {
            if request.role == InvocationRole::Fix {
                rep.fix_requests
                    .entry(request.work_id.0.clone())
                    .or_default()
                    .push((event.seq, request.request_id.clone()));
            }
            rep.requests
                .insert(request.request_id.clone(), (event.seq, request.clone()));
        }
        Effect::InsertInvocation { invocation } | Effect::UpdateInvocation { invocation, .. } => {
            emit_invocation(rep, items, snap, event, invocation);
        }
        Effect::InsertReviewRound { round, occurrences } => {
            rep.work_rounds
                .entry(round.work_id.0.clone())
                .or_default()
                .push((event.seq, round.id.clone()));
            rep.rounds
                .insert(round.id.clone(), (round.clone(), occurrences.clone()));
            emit_round(rep, items, snap, event, round, occurrences);
        }
        Effect::InsertRoundJudgment { judgment } => {
            rep.judgments
                .entry(judgment.round_id.clone())
                .or_default()
                .push((event.seq, judgment.clone()));
            let Some((round, occurrences)) = rep.rounds.get(&judgment.round_id).cloned() else {
                // A judgment with no sealed round in retained history:
                // its decisions cannot bind an occurrence set.
                return;
            };
            emit_judgment(rep, items, snap, event, judgment, &occurrences, &round);
        }
        Effect::UpsertHandoff { handoff, .. } => {
            rep.handoffs.insert(
                handoff.artifact_id.clone(),
                (handoff.work_id.0.clone(), handoff.produced.clone()),
            );
            if let Some(produced) = &handoff.produced {
                rep.produced_heads
                    .entry(produced.clone())
                    .or_default()
                    .push((
                        event.seq,
                        handoff.artifact_id.clone(),
                        handoff.execution_id.0.clone(),
                    ));
            }
            emit_repair(rep, items, snap, event, handoff);
        }
        Effect::InsertVerificationResult { result } => {
            emit_verification(rep, items, snap, event, result);
        }
        _ => {}
    }
}

/// Replay the recorded effects at or below `watermark` into the merged
/// record stream. Items keep only revisions with `change_seq > after_seq`
/// (the consumer already committed everything at or below the lower
/// checkpoint); derivation state is replayed from the retained boundary
/// so revision numbering and endpoint bindings stay continuous.
fn materialize(
    snap: &HistorySnapshot,
    watermark: u64,
    after_seq: Option<u64>,
) -> Result<Materialized, ExportError> {
    let mut rep = Replay::default();
    let mut items = Vec::new();
    for event in &snap.events {
        if event.seq > watermark {
            break;
        }
        let effect: Effect = serde_json::from_str(&event.payload).map_err(|e| {
            ExportError::new(
                ErrorCode::Store,
                format!("recorded effect at seq {} is unreadable: {e}", event.seq),
            )
        })?;
        let mut event_items = Vec::new();
        project(&mut rep, &mut event_items, snap, event, &effect);
        if after_seq.is_none_or(|after| event.seq > after) {
            items.append(&mut event_items);
        }
    }
    // Merged order: (change_seq, record_kind, source_id, revision).
    items.sort_by(|a, b| {
        (
            a.record.change_seq,
            a.kind,
            &a.record.source_id,
            &a.record.revision,
        )
            .cmp(&(
                b.record.change_seq,
                b.kind,
                &b.record.source_id,
                &b.record.revision,
            ))
    });
    let boundary = after_seq.unwrap_or(0);
    let gaps = rep
        .orphan_gaps
        .into_iter()
        .filter(|(seq, _)| *seq > boundary)
        .collect();
    Ok(Materialized {
        items,
        gaps,
        produced: rep.produced,
        emitted: rep.emitted,
    })
}

// ------------------------------------------------------------------
// Page assembly and the export driver
// ------------------------------------------------------------------

/// Headroom reserved for the `next_cursor`/`checkpoint` token fields
/// when checking a candidate page against `MAX_PAGE_BYTES`.
const PAGE_TOKEN_MARGIN: usize = 2048;
const MAX_PAGE_GAPS: usize = 2000;
const MAX_PAGE_ARTIFACTS: usize = 2000;

struct PageCtx {
    installation_id: String,
    store_epoch: String,
    snapshot_id: String,
    watermark: u64,
    retained_from_seq: u64,
    after_seq: Option<u64>,
}

fn page_fits(
    ctx: &PageCtx,
    entities: &[Record],
    links: &[Record],
    artifacts: &[ArtifactDescriptor],
    gaps: &[Gap],
) -> bool {
    let page = ExportPage {
        protocol_version: ProtocolVersion::V1,
        installation_id: ctx.installation_id.clone(),
        store_epoch: ctx.store_epoch.clone(),
        snapshot_id: ctx.snapshot_id.clone(),
        snapshot_seq: ctx.watermark,
        retained_from_seq: ctx.retained_from_seq,
        after_seq: ctx.after_seq,
        entities: entities.to_vec(),
        links: links.to_vec(),
        artifacts: artifacts.to_vec(),
        gaps: gaps.to_vec(),
        next_cursor: None,
        checkpoint: None,
    };
    // The CLI emits the envelope with to_string_pretty and the consumer
    // bounds the raw `result` bytes, so measure that wire form: the
    // page sits one nesting level deep, adding two indent bytes per
    // line on top of the standalone pretty serialization.
    serde_json::to_string_pretty(&page)
        .map(|wire| {
            let nesting = wire.bytes().filter(|&b| b == b'\n').count() * 2;
            wire.len() + nesting + PAGE_TOKEN_MARGIN <= MAX_PAGE_BYTES
        })
        .unwrap_or(false)
}

struct Assembly {
    entities: Vec<Record>,
    links: Vec<Record>,
    artifacts: BTreeMap<String, ArtifactDescriptor>,
    gaps: Vec<Gap>,
    gap_scopes: BTreeSet<(String, String)>,
    artifact_gaps: BTreeSet<(String, String)>,
}

impl Assembly {
    fn new() -> Self {
        Self {
            entities: vec![],
            links: vec![],
            artifacts: BTreeMap::new(),
            gaps: vec![],
            gap_scopes: BTreeSet::new(),
            artifact_gaps: BTreeSet::new(),
        }
    }

    fn artifacts_vec(&self) -> Vec<ArtifactDescriptor> {
        self.artifacts.values().cloned().collect()
    }
}

/// The endpoint-gap rule: a reference resolves when its revision is in
/// this sequence or in the consumer's retained history (any revision
/// minted at or below `after_seq`); otherwise the page names it with a
/// relation-class gap.
fn endpoint_gap(mat: &Materialized, after_seq: Option<u64>, r: &RevisionRef) -> Option<Gap> {
    let key = (r.source_id.clone(), r.revision.clone());
    if mat.emitted.contains(&key) {
        return None;
    }
    let produced_seq = mat.produced.get(&key).copied().unwrap_or(0);
    if after_seq.is_some_and(|after| produced_seq <= after && produced_seq > 0) {
        return None;
    }
    Some(Gap {
        code: GapCode::NotRecorded,
        scope: GapScope::Revision { record: r.clone() },
        evidence_class: EvidenceClass::Relation,
        reason: "the referenced endpoint revision is not retained \
                 in this export's history"
            .into(),
    })
}

/// Produce one export page for `request`. Token binding, watermark
/// pinning and page bounds are the whole contract surface here; the
/// projection itself is the deterministic `materialize` replay.
pub fn export_page(
    request: &ExportRequest,
    snap: &HistorySnapshot,
    now_unix_seconds: i64,
) -> Result<ExportPage, ExportError> {
    validate_request(request).map_err(|e| {
        ExportError::new(
            ErrorCode::InvalidRequest,
            format!("export request failed boundary validation ({e})"),
        )
    })?;
    // Every export sequence mints a fresh snapshot id: the contract
    // requires ids unseen anywhere earlier in the validated chain, so
    // two polls at the same watermark must not collide — only pages of
    // one sequence (via the cursor token) reuse it.
    let snap_nonce = new_id("snap");
    let (after_seq, watermark, retained, snap_id, mut position, mut gap_pos, minted) =
        match (&request.after, &request.cursor) {
            (Some(a), _) => {
                let t = ExportToken::decode(a)?;
                t.check_binding(
                    TokenKind::Checkpoint,
                    &snap.installation_id,
                    &snap.store_epoch,
                    snap.retained_from_seq,
                    snap.watermark,
                    now_unix_seconds,
                )?;
                let after = t.watermark;
                (
                    Some(after),
                    snap.watermark,
                    snap.retained_from_seq,
                    snapshot_id(
                        &snap.installation_id,
                        &snap.store_epoch,
                        Some(after),
                        snap.watermark,
                        &snap_nonce,
                    ),
                    0u64,
                    0u64,
                    now_unix_seconds,
                )
            }
            (None, Some(c)) => {
                let t = ExportToken::decode(c)?;
                t.check_binding(
                    TokenKind::Cursor,
                    &snap.installation_id,
                    &snap.store_epoch,
                    snap.retained_from_seq,
                    snap.watermark,
                    now_unix_seconds,
                )?;
                (
                    t.after_seq,
                    t.watermark,
                    t.retained_from_seq,
                    t.snapshot_id,
                    t.position,
                    t.gap_position,
                    t.minted_unix_seconds,
                )
            }
            (None, None) => (
                None,
                snap.watermark,
                snap.retained_from_seq,
                snapshot_id(
                    &snap.installation_id,
                    &snap.store_epoch,
                    None,
                    snap.watermark,
                    &snap_nonce,
                ),
                0,
                0,
                now_unix_seconds,
            ),
        };
    let ctx = PageCtx {
        installation_id: snap.installation_id.clone(),
        store_epoch: snap.store_epoch.clone(),
        snapshot_id: snap_id,
        watermark,
        retained_from_seq: retained,
        after_seq,
    };
    let mat = materialize(snap, watermark, after_seq)?;
    let limit = usize::from(request.limit);
    let mut a = Assembly::new();
    if after_seq.is_none() && retained > 1 && position == 0 && gap_pos == 0 {
        a.gaps.push(Gap {
            code: GapCode::RetentionExpired,
            scope: GapScope::History {
                before_seq: retained,
            },
            evidence_class: EvidenceClass::History,
            reason: "earlier recorded history is not retained".into(),
        });
    }
    loop {
        if let Some(item) = mat.items.get(position as usize) {
            if a.entities.len() + a.links.len() + 1 > limit {
                break;
            }
            let mut endpoint_gaps: Vec<Gap> = item
                .refs
                .iter()
                .filter_map(|r| endpoint_gap(&mat, after_seq, r))
                .filter(|g| {
                    if let GapScope::Revision { record } = &g.scope {
                        a.gap_scopes
                            .insert((record.source_id.clone(), record.revision.clone()))
                    } else {
                        true
                    }
                })
                .collect();
            let mut candidate_gaps = a.gaps.clone();
            let mut drain = 0usize;
            while let Some((seq, gap)) = mat.gaps.get(gap_pos as usize + drain)
                && *seq <= item.record.change_seq
            {
                candidate_gaps.push(gap.clone());
                drain += 1;
            }
            candidate_gaps.append(&mut endpoint_gaps);
            candidate_gaps.extend(
                item.gaps
                    .iter()
                    .filter(|g| match &g.scope {
                        GapScope::Artifact {
                            artifact_id,
                            sha256,
                        } if g.evidence_class == EvidenceClass::Artifact => a
                            .artifact_gaps
                            .insert((artifact_id.clone(), sha256.clone())),
                        _ => true,
                    })
                    .cloned(),
            );
            let mut candidate_arts = a.artifacts.clone();
            for art in &item.artifacts {
                candidate_arts.insert(art.artifact_id.clone(), art.clone());
            }
            let fits = page_fits(
                &ctx,
                &a.entities
                    .iter()
                    .cloned()
                    .chain((item.kind == RecordKind::Entity).then(|| item.record.clone()))
                    .collect::<Vec<_>>(),
                &a.links
                    .iter()
                    .cloned()
                    .chain((item.kind == RecordKind::Link).then(|| item.record.clone()))
                    .collect::<Vec<_>>(),
                &candidate_arts.values().cloned().collect::<Vec<_>>(),
                &candidate_gaps,
            ) && candidate_gaps.len() <= MAX_PAGE_GAPS
                && candidate_arts.len() <= MAX_PAGE_ARTIFACTS;
            if !fits {
                if a.entities.is_empty() && a.links.is_empty() {
                    return Err(ExportError::new(
                        ErrorCode::RecordTooLarge,
                        "the next record cannot fit in a page",
                    ));
                }
                break;
            }
            match item.kind {
                RecordKind::Entity => a.entities.push(item.record.clone()),
                RecordKind::Link => a.links.push(item.record.clone()),
            }
            a.artifacts = candidate_arts;
            a.gaps = candidate_gaps;
            gap_pos += drain as u64;
            position += 1;
        } else {
            // Record stream exhausted: drain the orphan-gap tail onto
            // the final page.
            while let Some((_, gap)) = mat.gaps.get(gap_pos as usize) {
                let mut candidate = a.gaps.clone();
                candidate.push(gap.clone());
                let candidate_arts = a.artifacts_vec();
                if candidate.len() > MAX_PAGE_GAPS
                    || !page_fits(&ctx, &a.entities, &a.links, &candidate_arts, &candidate)
                {
                    return Err(ExportError::new(
                        ErrorCode::RecordTooLarge,
                        "the page's evidence gaps cannot fit in a page",
                    ));
                }
                a.gaps = candidate;
                gap_pos += 1;
            }
            break;
        }
    }
    let finished = position as usize >= mat.items.len() && gap_pos as usize >= mat.gaps.len();
    let token = |kind: TokenKind| {
        ExportToken {
            kind,
            installation_id: ctx.installation_id.clone(),
            store_epoch: ctx.store_epoch.clone(),
            watermark,
            retained_from_seq: retained,
            after_seq,
            snapshot_id: ctx.snapshot_id.clone(),
            position,
            gap_position: gap_pos,
            minted_unix_seconds: minted,
        }
        .encode()
    };
    let (next_cursor, checkpoint) = if finished {
        (None, Some(token(TokenKind::Checkpoint)))
    } else {
        (Some(token(TokenKind::Cursor)), None)
    };
    Ok(ExportPage {
        protocol_version: ProtocolVersion::V1,
        installation_id: ctx.installation_id,
        store_epoch: ctx.store_epoch,
        snapshot_id: ctx.snapshot_id,
        snapshot_seq: watermark,
        retained_from_seq: retained,
        after_seq,
        entities: a.entities,
        links: a.links,
        artifacts: a.artifacts.values().cloned().collect(),
        gaps: a.gaps,
        next_cursor,
        checkpoint,
    })
}
