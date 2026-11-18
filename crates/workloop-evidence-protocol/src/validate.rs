use crate::*;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationError {
    UnsupportedProtocol,
    InvalidJson,
    InvalidRequest,
    InvalidField,
    PageSize,
    PageTokens,
    SnapshotMetadata,
    Watermark,
    Ordering,
    DuplicateRevision,
    RevisionSequence,
    BodyDigest,
    BodyIdentity,
    RecordKindMismatch,
    MissingEndpoint,
    MissingEvidenceGap,
    EndpointType,
    HeadMismatch,
    ArtifactIdentityMismatch,
    ArtifactUnavailable,
    ArtifactTooLarge,
    ArtifactLength,
    ArtifactDigest,
    InvalidLocator,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ValidationError {}
use ValidationError::*;

fn require(ok: bool, error: ValidationError) -> Result<(), ValidationError> {
    if ok { Ok(()) } else { Err(error) }
}
fn token(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= 1024
}
fn digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn reference(r: &RevisionRef) -> bool {
    token(&r.source_id) && token(&r.revision)
}

pub fn decode_page(raw: &[u8]) -> Result<ExportPage, ValidationError> {
    require(raw.len() <= MAX_PAGE_BYTES, PageSize)?;
    #[derive(Deserialize)]
    struct Header {
        protocol_version: String,
    }
    let header: Header = serde_json::from_slice(raw).map_err(|_| InvalidJson)?;
    require(header.protocol_version == PROTOCOL, UnsupportedProtocol)?;
    serde_json::from_slice(raw).map_err(|_| InvalidJson)
}

pub fn validate_request(request: &ExportRequest) -> Result<(), ValidationError> {
    require(
        !(request.after.is_some() && request.cursor.is_some())
            && request.after.as_deref().is_none_or(token)
            && request.cursor.as_deref().is_none_or(token)
            && (1..=MAX_RECORDS).contains(&usize::from(request.limit)),
        InvalidRequest,
    )
}

fn record_key(record: &Record, kind: RecordKind) -> (u64, RecordKind, &str, &str) {
    (record.change_seq, kind, &record.source_id, &record.revision)
}

pub fn validate_page(request: &ExportRequest, page: &ExportPage) -> Result<(), ValidationError> {
    validate_request(request)?;
    require(
        page.entities.len() + page.links.len() <= usize::from(request.limit)
            && page.artifacts.len() <= 2000
            && page.gaps.len() <= 2000,
        PageSize,
    )?;
    require(
        page.next_cursor.is_some() != page.checkpoint.is_some()
            && page.next_cursor.as_deref().is_none_or(token)
            && page.checkpoint.as_deref().is_none_or(token)
            && (page.next_cursor.is_none() || !page.entities.is_empty() || !page.links.is_empty()),
        PageTokens,
    )?;
    require(
        [&page.installation_id, &page.store_epoch, &page.snapshot_id]
            .into_iter()
            .all(|s| token(s)),
        InvalidField,
    )?;
    require(
        page.retained_from_seq <= page.snapshot_seq
            && page.after_seq.is_none_or(|seq| {
                seq <= page.snapshot_seq && seq.saturating_add(1) >= page.retained_from_seq
            }),
        Watermark,
    )?;
    require(
        request.cursor.is_some() || request.after.is_some() == page.after_seq.is_some(),
        InvalidRequest,
    )?;
    for (kind, records) in [
        (RecordKind::Entity, &page.entities),
        (RecordKind::Link, &page.links),
    ] {
        require(
            records
                .windows(2)
                .all(|pair| record_key(&pair[0], kind) <= record_key(&pair[1], kind)),
            Ordering,
        )?;
        for record in records {
            require(
                record.change_seq >= page.retained_from_seq
                    && record.change_seq <= page.snapshot_seq
                    && page.after_seq.is_none_or(|seq| record.change_seq > seq),
                Watermark,
            )?;
            parse_record(record, kind)?;
        }
    }
    for a in &page.artifacts {
        require(
            a.identity.installation_id == page.installation_id
                && a.identity.store_epoch == page.store_epoch,
            ArtifactIdentityMismatch,
        )?;
        validate_descriptor(a)?;
    }
    for gap in &page.gaps {
        require(
            token(&gap.reason)
                && match &gap.scope {
                    GapScope::Revision { record } => reference(record),
                    GapScope::Artifact {
                        artifact_id,
                        sha256,
                    } => token(artifact_id) && digest(sha256),
                    GapScope::History { before_seq } => {
                        *before_seq == page.retained_from_seq
                            && gap.code == GapCode::RetentionExpired
                            && gap.evidence_class == EvidenceClass::History
                    }
                },
            InvalidField,
        )?;
    }
    Ok(())
}

pub fn parse_record(r: &Record, kind: RecordKind) -> Result<RecordBody, ValidationError> {
    require(
        token(&r.source_id) && token(&r.revision) && r.revision_seq > 0,
        InvalidField,
    )?;
    require(
        digest(&r.body_sha256) && sha256(r.body_json.as_bytes()) == r.body_sha256,
        BodyDigest,
    )?;
    let body: RecordBody = serde_json::from_str(&r.body_json).map_err(|_| InvalidJson)?;
    require(
        (
            body.source_id.as_str(),
            body.revision.as_str(),
            body.revision_seq,
        ) == (r.source_id.as_str(), r.revision.as_str(), r.revision_seq),
        BodyIdentity,
    )?;
    let data_kind = match &body.data {
        RecordData::Entity { .. } | RecordData::EntityTombstone { .. } => RecordKind::Entity,
        _ => RecordKind::Link,
    };
    require(
        body.record_kind == kind && data_kind == kind,
        RecordKindMismatch,
    )?;
    require(
        token(&body.work_id) && body.source_refs.len() <= 32,
        InvalidField,
    )?;
    match &body.assertion {
        Assertion::HumanInference {
            author,
            tool,
            version,
            rationale,
        }
        | Assertion::ModelInterpretation {
            author,
            tool,
            version,
            rationale,
        } => require(
            [author, tool, version, rationale]
                .into_iter()
                .all(|s| token(s)),
            InvalidField,
        )?,
        _ => {}
    }
    match &body.data {
        RecordData::EntityTombstone { reason, .. } | RecordData::LinkTombstone { reason, .. } => {
            require(token(reason) && !body.source_refs.is_empty(), InvalidField)?
        }
        RecordData::Link { .. } => require(!body.source_refs.is_empty(), InvalidField)?,
        _ => {}
    }
    require(
        body.source_refs
            .iter()
            .all(|r| token(&r.artifact_id) && digest(&r.sha256)),
        InvalidField,
    )?;
    validate_payload(&body)?;
    Ok(body)
}

fn validate_payload(body: &RecordBody) -> Result<(), ValidationError> {
    use EntityPayload::*;
    let RecordData::Entity { entity } = &body.data else {
        return Ok(());
    };
    let valid = match entity {
        Work {
            work_id, version, ..
        } => work_id == &body.work_id && *version > 0,
        Execution {
            execution_id,
            base_head,
            result_head,
            started_at,
            finished_at,
            ..
        } => {
            token(execution_id)
                && [base_head, result_head, started_at, finished_at]
                    .into_iter()
                    .all(|v| v.as_deref().is_none_or(token))
        }
        Invocation {
            invocation_id,
            process_attempt_id,
            ..
        } => token(invocation_id) && token(process_attempt_id),
        ReviewRound {
            round_id,
            change_id,
            reviewed_head,
            observation_id,
            ..
        } => {
            token(round_id)
                && token(change_id)
                && [reviewed_head, observation_id]
                    .into_iter()
                    .all(|v| v.as_deref().is_none_or(token))
        }
        FindingOccurrence {
            occurrence_id,
            source_finding_key,
            reviewed_head,
            category,
            ..
        } => {
            token(occurrence_id)
                && token(source_finding_key)
                && [reviewed_head, category]
                    .into_iter()
                    .all(|v| v.as_deref().is_none_or(token))
        }
        RepairAttempt {
            attempt_id,
            target_head,
            result_head,
            sequence,
            ..
        } => {
            token(attempt_id)
                && sequence.is_none_or(|s| s > 0)
                && [target_head, result_head]
                    .into_iter()
                    .all(|v| v.as_deref().is_none_or(token))
        }
        Verification {
            check_id,
            name,
            tested_head,
            ..
        } => token(check_id) && token(name) && tested_head.as_deref().is_none_or(token),
        Outcome {
            outcome_id, head, ..
        } => token(outcome_id) && head.as_deref().is_none_or(token),
        Requirement {
            requirement_id,
            category,
            ..
        } => token(requirement_id) && category.as_deref().is_none_or(token),
        Invariant {
            invariant_id,
            category,
            ..
        } => token(invariant_id) && category.as_deref().is_none_or(token),
    };
    require(valid, InvalidField)
}

pub fn validate_descriptor(a: &ArtifactDescriptor) -> Result<(), ValidationError> {
    let i = &a.identity;
    require(
        [
            &a.artifact_id,
            &a.media_type,
            &i.installation_id,
            &i.store_epoch,
            &i.source_id,
            &i.revision,
        ]
        .into_iter()
        .all(|s| token(s))
            && i.revision_seq > 0
            && digest(&a.sha256),
        ArtifactIdentityMismatch,
    )?;
    require(
        a.availability != Availability::Available || a.byte_length <= MAX_ARTIFACT_BYTES as u64,
        ArtifactTooLarge,
    )
}
pub fn validate_artifact(a: &ArtifactDescriptor, bytes: &[u8]) -> Result<(), ValidationError> {
    validate_descriptor(a)?;
    require(
        a.availability == Availability::Available,
        ArtifactUnavailable,
    )?;
    require(bytes.len() <= MAX_ARTIFACT_BYTES, ArtifactTooLarge)?;
    require(bytes.len() as u64 == a.byte_length, ArtifactLength)?;
    require(sha256(bytes) == a.sha256, ArtifactDigest)
}
pub fn validate_artifact_request(r: &ArtifactRequest) -> Result<(), ValidationError> {
    require(token(&r.artifact_id) && digest(&r.sha256), InvalidRequest)
}
pub fn validate_artifact_result(
    r: &ArtifactRequest,
    expected: &ArtifactDescriptor,
    result: &ArtifactResult,
    bytes: &[u8],
) -> Result<(), ValidationError> {
    validate_artifact_request(r)?;
    validate_descriptor(expected)?;
    require(
        expected.availability == Availability::Available,
        ArtifactUnavailable,
    )?;
    require(
        same_artifact_binding(expected, &result.descriptor),
        ArtifactIdentityMismatch,
    )?;
    require(
        r.artifact_id == result.descriptor.artifact_id,
        ArtifactIdentityMismatch,
    )?;
    require(r.sha256 == result.descriptor.sha256, ArtifactDigest)?;
    require(
        result.bytes_written == result.descriptor.byte_length,
        ArtifactLength,
    )?;
    validate_artifact(&result.descriptor, bytes)
}

fn validate_locator(
    r: &SourceRef,
    a: &ArtifactDescriptor,
    bytes: Option<&[u8]>,
) -> Result<(), ValidationError> {
    require(r.sha256 == a.sha256, ArtifactDigest)?;
    match &r.locator {
        Locator::ByteRange { start, end } => {
            require(start < end && *end <= a.byte_length, InvalidLocator)
        }
        Locator::JsonPointer { pointer } => {
            let valid = pointer.is_empty()
                || (pointer.starts_with('/')
                    && pointer.as_bytes().iter().enumerate().all(|(i, b)| {
                        *b != b'~' || matches!(pointer.as_bytes().get(i + 1), Some(b'0' | b'1'))
                    }));
            require(
                valid
                    && pointer.len() <= 1024
                    && (a.media_type == "application/json" || a.media_type.ends_with("+json")),
                InvalidLocator,
            )?;
            let Some(bytes) = bytes else {
                return Ok(());
            };
            let json: serde_json::Value =
                serde_json::from_slice(bytes).map_err(|_| InvalidLocator)?;
            require(json.pointer(pointer).is_some(), InvalidLocator)
        }
    }
}

#[derive(Debug, Clone)]
pub struct ValidatedSnapshot {
    snapshot_ids: BTreeSet<String>,
    checkpoints: BTreeMap<String, u64>,
    unresolved: BTreeSet<RevisionRef>,
    revision_gaps: BTreeSet<(RevisionRef, EvidenceClass)>,
    installation: String,
    epoch: String,
    watermark: u64,
    retained_from: u64,
    checkpoint: String,
    records: BTreeMap<RevisionRef, RecordBody>,
    artifacts: BTreeMap<String, ArtifactDescriptor>,
    unsupported: BTreeSet<RevisionRef>,
}
impl ValidatedSnapshot {
    pub fn checkpoint(&self) -> &str {
        &self.checkpoint
    }
    pub fn record(&self, id: &RevisionRef) -> Option<&RecordBody> {
        self.records.get(id)
    }
    pub fn unsupported_links(&self) -> &BTreeSet<RevisionRef> {
        &self.unsupported
    }
}

type ArtifactBinding = (String, u64, String);

#[derive(Default)]
struct RevisionBindings {
    by_revision: BTreeMap<(String, String), u64>,
    artifact_contents: BTreeMap<(String, String), ArtifactBinding>,
    by_sequence: BTreeMap<(String, u64), String>,
}
impl RevisionBindings {
    fn check_revision(
        &mut self,
        source: &str,
        revision: &str,
        seq: u64,
    ) -> Result<(), ValidationError> {
        let mapped_seq = self
            .by_revision
            .entry((source.into(), revision.into()))
            .or_insert(seq);
        let mapped_rev = self
            .by_sequence
            .entry((source.into(), seq))
            .or_insert_with(|| revision.into());
        require(
            *mapped_seq == seq && mapped_rev == revision,
            ArtifactIdentityMismatch,
        )
    }

    fn check_artifact(&mut self, artifact: &ArtifactDescriptor) -> Result<(), ValidationError> {
        let id = &artifact.identity;
        self.check_revision(&id.source_id, &id.revision, id.revision_seq)?;
        let contents = (
            artifact.sha256.clone(),
            artifact.byte_length,
            artifact.media_type.clone(),
        );
        let previous = self
            .artifact_contents
            .entry((id.source_id.clone(), id.revision.clone()))
            .or_insert_with(|| contents.clone());
        require(*previous == contents, ArtifactIdentityMismatch)
    }
}

fn same_artifact_binding(a: &ArtifactDescriptor, b: &ArtifactDescriptor) -> bool {
    a.artifact_id == b.artifact_id
        && a.identity == b.identity
        && a.sha256 == b.sha256
        && a.byte_length == b.byte_length
        && a.media_type == b.media_type
}

pub fn validate_snapshot(
    pages: &[(ExportRequest, ExportPage)],
    history: Option<&ValidatedSnapshot>,
    artifact_bytes: &BTreeMap<String, Vec<u8>>,
) -> Result<ValidatedSnapshot, ValidationError> {
    let (_, first) = pages.first().ok_or(PageTokens)?;
    let mut records = BTreeMap::new();
    let mut artifacts = BTreeMap::new();
    let mut unsupported = BTreeSet::new();
    let mut snapshot_ids = BTreeSet::new();
    let mut checkpoints = BTreeMap::new();
    let mut unresolved = BTreeSet::new();
    let mut gaps = BTreeSet::new();
    let checkpoint = pages
        .last()
        .and_then(|p| p.1.checkpoint.clone())
        .ok_or(PageTokens)?;
    if let Some(h) = history {
        require(
            h.installation == first.installation_id
                && h.epoch == first.store_epoch
                && Some(h.watermark) == first.after_seq
                && first.retained_from_seq >= h.retained_from
                && pages[0].0.after.as_deref() == Some(h.checkpoint()),
            SnapshotMetadata,
        )?;
        records.clone_from(&h.records);
        artifacts.clone_from(&h.artifacts);
        unsupported.clone_from(&h.unsupported);
        snapshot_ids.clone_from(&h.snapshot_ids);
        checkpoints.clone_from(&h.checkpoints);
        unresolved.clone_from(&h.unresolved);
        gaps.clone_from(&h.revision_gaps);
    } else {
        require(first.after_seq.is_none(), SnapshotMetadata)?;
    }
    require(
        snapshot_ids.insert(first.snapshot_id.clone()),
        SnapshotMetadata,
    )?;
    if let Some(old) = checkpoints.insert(checkpoint.clone(), first.snapshot_seq) {
        require(old == first.snapshot_seq, SnapshotMetadata)?;
    }
    let mut revisions = RevisionBindings::default();
    for artifact in artifacts.values() {
        revisions.check_artifact(artifact)?;
    }
    let mut cursor = None;
    let mut seen_cursors = BTreeSet::new();
    let mut previous_key = None;
    let mut latest = BTreeMap::new();
    let mut new_records = Vec::new();
    let mut current_gaps = BTreeSet::new();
    let mut seen_descriptors = BTreeSet::new();
    let mut artifact_gaps = BTreeSet::new();
    let mut has_history_gap = false;
    let mut checked_artifacts = BTreeSet::new();
    for body in records.values() {
        revisions.check_revision(&body.source_id, &body.revision, body.revision_seq)?;
        update_latest(&mut latest, body);
    }
    for (index, (request, page)) in pages.iter().enumerate() {
        validate_page(request, page)?;
        require(
            request.cursor.as_ref() == cursor
                && (index == 0 || request.after.is_none())
                && page.checkpoint.is_some() == (index + 1 == pages.len()),
            PageTokens,
        )?;
        cursor = page.next_cursor.as_ref();
        if let Some(c) = cursor {
            require(seen_cursors.insert(c), PageTokens)?;
        }
        require(
            (
                &page.installation_id,
                &page.store_epoch,
                &page.snapshot_id,
                page.snapshot_seq,
                page.retained_from_seq,
                page.after_seq,
            ) == (
                &first.installation_id,
                &first.store_epoch,
                &first.snapshot_id,
                first.snapshot_seq,
                first.retained_from_seq,
                first.after_seq,
            ),
            SnapshotMetadata,
        )?;
        for a in &page.artifacts {
            revisions.check_artifact(a)?;
            let first_in_snapshot = seen_descriptors.insert(&a.artifact_id);
            if let Some(old) = artifacts.insert(a.artifact_id.clone(), a.clone()) {
                require(
                    if first_in_snapshot {
                        same_artifact_binding(&old, a)
                    } else {
                        old == *a
                    },
                    ArtifactIdentityMismatch,
                )?;
            }
            if a.availability == Availability::Available
                && checked_artifacts.insert(a.artifact_id.clone())
            {
                validate_artifact(
                    a,
                    artifact_bytes
                        .get(&a.artifact_id)
                        .ok_or(ArtifactUnavailable)?,
                )?;
            }
        }
        for gap in &page.gaps {
            match &gap.scope {
                GapScope::Revision { record } => {
                    current_gaps.insert((record.clone(), gap.evidence_class));
                }
                GapScope::Artifact {
                    artifact_id,
                    sha256,
                } if gap.evidence_class == EvidenceClass::Artifact => {
                    artifact_gaps.insert((artifact_id.clone(), sha256.clone()));
                }
                GapScope::History { .. } => has_history_gap = true,
                _ => {}
            }
        }
        let mut merged: Vec<_> = page
            .entities
            .iter()
            .map(|r| (r, RecordKind::Entity))
            .chain(page.links.iter().map(|r| (r, RecordKind::Link)))
            .collect();
        merged.sort_unstable_by_key(|(r, k)| record_key(r, *k));
        for (r, k) in merged {
            let id = RevisionRef {
                source_id: r.source_id.clone(),
                revision: r.revision.clone(),
            };
            require(!records.contains_key(&id), DuplicateRevision)?;
            let key = record_key(r, k);
            require(previous_key.is_none_or(|previous| previous < key), Ordering)?;
            previous_key = Some(key);
            let body = parse_record(r, k)?;
            if let Some((seq, old_kind, old_type)) = latest.get(&body.source_id) {
                require(*seq < body.revision_seq, RevisionSequence)?;
                require(
                    *old_kind == body.record_kind && *old_type == logical_type(&body),
                    RecordKindMismatch,
                )?;
            }
            revisions.check_revision(&body.source_id, &body.revision, body.revision_seq)?;
            update_latest(&mut latest, &body);
            new_records.push(id.clone());
            records.insert(id, body);
        }
    }
    require(
        first.after_seq.is_some() || first.retained_from_seq <= 1 || has_history_gap,
        Watermark,
    )?;
    gaps.extend(
        current_gaps
            .iter()
            .filter(|(_, class)| *class == EvidenceClass::Relation)
            .cloned(),
    );
    if !new_records.is_empty() {
        for id in std::mem::take(&mut unresolved) {
            if validate_endpoints(&records[&id], &records, &gaps)? {
                unresolved.insert(id);
            }
        }
    }
    for id in new_records {
        let body = &records[&id];
        let missing_class = match &body.data {
            RecordData::Entity {
                entity: EntityPayload::FindingOccurrence { body: None, .. },
            } => Some(EvidenceClass::Body),
            RecordData::Entity {
                entity: EntityPayload::Work { intent: None, .. },
            } => Some(EvidenceClass::Intent),
            _ => None,
        };
        if let Some(class) = missing_class {
            require(
                current_gaps.contains(&(id.clone(), class)),
                MissingEvidenceGap,
            )?;
        }
        for r in &body.source_refs {
            let a = artifacts
                .get(&r.artifact_id)
                .ok_or(ArtifactIdentityMismatch)?;
            if a.availability != Availability::Available {
                validate_locator(r, a, None)?;
                require(
                    artifact_gaps.contains(&(r.artifact_id.clone(), r.sha256.clone())),
                    ArtifactUnavailable,
                )?;
                continue;
            }
            let bytes = artifact_bytes
                .get(&r.artifact_id)
                .ok_or(ArtifactUnavailable)?;
            if checked_artifacts.insert(r.artifact_id.clone()) {
                validate_artifact(a, bytes)?;
            }
            validate_locator(r, a, Some(bytes))?;
        }
        if validate_endpoints(body, &records, &gaps)? {
            unresolved.insert(id.clone());
            if body.record_kind == RecordKind::Link {
                unsupported.insert(id);
            }
        }
    }
    Ok(ValidatedSnapshot {
        snapshot_ids,
        checkpoints,
        unresolved,
        revision_gaps: gaps,
        installation: first.installation_id.clone(),
        epoch: first.store_epoch.clone(),
        watermark: first.snapshot_seq,
        retained_from: first.retained_from_seq,
        checkpoint,
        records,
        artifacts,
        unsupported,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LogicalType {
    Entity(EntityType),
    Link(Relation),
}
fn logical_type(body: &RecordBody) -> LogicalType {
    match &body.data {
        RecordData::Entity { entity } => LogicalType::Entity(entity.entity_type()),
        RecordData::EntityTombstone { entity_type, .. } => LogicalType::Entity(*entity_type),
        RecordData::Link { relation, .. } | RecordData::LinkTombstone { relation, .. } => {
            LogicalType::Link(*relation)
        }
    }
}
fn update_latest(latest: &mut BTreeMap<String, (u64, RecordKind, LogicalType)>, body: &RecordBody) {
    if latest
        .get(&body.source_id)
        .is_none_or(|entry| entry.0 < body.revision_seq)
    {
        latest.insert(
            body.source_id.clone(),
            (body.revision_seq, body.record_kind, logical_type(body)),
        );
    }
}

fn validate_endpoints(
    body: &RecordBody,
    records: &BTreeMap<RevisionRef, RecordBody>,
    gaps: &BTreeSet<(RevisionRef, EvidenceClass)>,
) -> Result<bool, ValidationError> {
    use EntityType as T;
    let mut missing = false;
    let mut resolve = |id: &RevisionRef, expected: Option<T>| {
        require(reference(id), InvalidField)?;
        match records.get(id) {
            Some(endpoint) => {
                require(endpoint.work_id == body.work_id, EndpointType)?;
                if let Some(ty) = expected {
                    require(
                        matches!(&endpoint.data, RecordData::Entity { entity } if entity.entity_type() == ty),
                        EndpointType,
                    )?;
                }
                Ok(Some(endpoint))
            }
            None => {
                require(
                    gaps.contains(&(id.clone(), EvidenceClass::Relation)),
                    MissingEndpoint,
                )?;
                missing = true;
                Ok(None)
            }
        }
    };
    match &body.data {
        RecordData::Link { relation, from, to }
        | RecordData::LinkTombstone {
            relation, from, to, ..
        } => {
            use Relation::*;
            let (from_type, to_type) = match relation {
                WorkHasExecution => (T::Work, T::Execution),
                ExecutionHasInvocation => (T::Execution, T::Invocation),
                ExecutionHasRound => (T::Execution, T::ReviewRound),
                RoundHasFinding => (T::ReviewRound, T::FindingOccurrence),
                WorkHasRequirement => (T::Work, T::Requirement),
                WorkHasInvariant => (T::Work, T::Invariant),
                ExecutionHasRepair => (T::Execution, T::RepairAttempt),
                FindingViolatesInvariant => (T::FindingOccurrence, T::Invariant),
                RepairAddressesFinding => (T::RepairAttempt, T::FindingOccurrence),
                VerificationTestsRepair => (T::Verification, T::RepairAttempt),
                ExecutionHasOutcome => (T::Execution, T::Outcome),
                RoundHasRepair => (T::ReviewRound, T::RepairAttempt),
            };
            let from = resolve(from, Some(from_type))?;
            let to = resolve(to, Some(to_type))?;
            if *relation == VerificationTestsRepair
                && matches!(body.data, RecordData::Link { .. })
                && let (Some(from), Some(to)) = (from, to)
                && let (
                    RecordData::Entity {
                        entity: EntityPayload::Verification { tested_head, .. },
                    },
                    RecordData::Entity {
                        entity: EntityPayload::RepairAttempt { result_head, .. },
                    },
                ) = (&from.data, &to.data)
            {
                require(
                    tested_head.is_some() && tested_head == result_head,
                    HeadMismatch,
                )?;
            }
            Ok(missing)
        }
        RecordData::Entity { entity } => {
            use EntityPayload::*;
            let mut refs = Vec::new();
            match entity {
                Work {
                    requirements,
                    constraints,
                    dispatch_source,
                    ..
                } => {
                    refs.extend(requirements.iter().map(|r| (r, Some(T::Requirement))));
                    refs.extend(constraints.iter().map(|r| (r, Some(T::Invariant))));
                    refs.extend(dispatch_source.iter().map(|r| (r, None)));
                }
                Execution { work, outcome, .. } => {
                    refs.push((work, Some(T::Work)));
                    refs.extend(outcome.iter().map(|r| (r, Some(T::Outcome))));
                }
                Invocation { execution, .. }
                | ReviewRound { execution, .. }
                | Outcome { execution, .. } => refs.push((execution, Some(T::Execution))),
                FindingOccurrence {
                    round, invariants, ..
                } => {
                    refs.push((round, Some(T::ReviewRound)));
                    refs.extend(invariants.iter().map(|r| (r, Some(T::Invariant))));
                }
                RepairAttempt {
                    execution, round, ..
                } => {
                    refs.push((execution, Some(T::Execution)));
                    refs.extend(round.iter().map(|r| (r, Some(T::ReviewRound))));
                }
                Requirement {
                    source_revision, ..
                }
                | Invariant {
                    source_revision, ..
                } => refs.extend(source_revision.iter().map(|r| (r, None))),
                Verification { .. } => {}
            }
            require(refs.len() <= 500, InvalidField)?;
            for (r, t) in refs {
                resolve(r, t)?;
            }
            Ok(missing)
        }
        RecordData::EntityTombstone { .. } => Ok(false),
    }
}

impl<T> QueryEnvelope<T> {
    fn validate_query(
        &self,
        kind: CommandKind,
        watermark: Option<u64>,
    ) -> Result<(), ValidationError> {
        let identity = |c: &QueryCommand| {
            c.kind == kind
                && token(&c.actor)
                && c.id.as_deref().is_none_or(token)
                && c.idempotency_key.is_none()
        };
        require(
            self.work_id.is_none()
                && self.execution_id.is_none()
                && self.evidence_refs.len() <= 16
                && self.evidence_refs.iter().all(|s| s.chars().count() <= 240)
                && self.sources.as_ref().is_none_or(|sources| {
                    sources.len() <= 2000 && sources.iter().all(|s| token(&s.source))
                }),
            InvalidField,
        )?;
        match self.status {
            QueryStatus::Accepted => require(
                self.result.is_some()
                    && self.reason.is_none()
                    && self.command.as_ref().is_some_and(identity)
                    && self.as_of == watermark
                    && self.sources.is_some() == watermark.is_some(),
                InvalidField,
            ),
            QueryStatus::Error => require(
                self.result.is_none()
                    && self.as_of.is_none()
                    && self.sources.is_none()
                    && self.command.as_ref().is_none_or(identity)
                    && self.reason.as_ref().is_some_and(|r| {
                        !r.message.trim().is_empty() && r.message.chars().count() <= 240
                    }),
                InvalidField,
            ),
        }
    }
}

impl QueryEnvelope<ExportPage> {
    pub fn validate_export(&self) -> Result<(), ValidationError> {
        self.validate_query(
            CommandKind::EvidenceExport,
            self.result.as_ref().map(|p| p.snapshot_seq),
        )
    }
}
impl QueryEnvelope<ArtifactResult> {
    pub fn validate_artifact_receipt(&self) -> Result<(), ValidationError> {
        self.validate_query(CommandKind::EvidenceArtifact, None)?;
        if let Some(result) = &self.result {
            validate_descriptor(&result.descriptor)?;
            require(
                result.descriptor.availability == Availability::Available,
                ArtifactUnavailable,
            )?;
            require(
                result.bytes_written == result.descriptor.byte_length,
                ArtifactLength,
            )?;
        }
        Ok(())
    }
}

pub fn decode_export_envelope(raw: &[u8]) -> Result<QueryEnvelope<ExportPage>, ValidationError> {
    let wire: QueryEnvelope<&serde_json::value::RawValue> =
        serde_json::from_slice(raw).map_err(|_| InvalidJson)?;
    let envelope = QueryEnvelope {
        schema_version: wire.schema_version,
        status: wire.status,
        command: wire.command,
        work_id: wire.work_id,
        execution_id: wire.execution_id,
        result: wire
            .result
            .map(|body| decode_page(body.get().as_bytes()))
            .transpose()?,
        reason: wire.reason,
        evidence_refs: wire.evidence_refs,
        sources: wire.sources,
        as_of: wire.as_of,
    };
    envelope.validate_export()?;
    Ok(envelope)
}
