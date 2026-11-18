use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
};
use workloop_evidence_protocol::*;

fn bytes(name: &str) -> Vec<u8> {
    fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/rui-evidence-v1")
            .join(name),
    )
    .unwrap()
}
#[derive(Deserialize)]
struct PageFixture {
    request: ExportRequest,
    envelope: Box<serde_json::value::RawValue>,
}
fn page(name: &str) -> (ExportRequest, ExportPage) {
    let f: PageFixture = serde_json::from_slice(&bytes(name)).unwrap();
    (
        f.request,
        decode_export_envelope(f.envelope.get().as_bytes())
            .unwrap()
            .result
            .unwrap(),
    )
}
fn full() -> [(ExportRequest, ExportPage); 2] {
    [page("full-1.json"), page("full-2.json")]
}
fn artifacts() -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([("observations".into(), bytes("observations.json"))])
}
fn rf(id: &str, rev: &str) -> RevisionRef {
    RevisionRef {
        source_id: id.into(),
        revision: rev.into(),
    }
}

#[test]
fn freeze_matches_generated_schema_and_actual_files() {
    let raw = bytes("schema.json");
    assert_eq!(
        raw,
        format!("{}\n", serde_json::to_string_pretty(&schema()).unwrap()).into_bytes()
    );
    let schema: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert!(schema["$defs"]["RecordBody"].is_object());
    assert_eq!(
        schema["$defs"]["Record"]["properties"]["body_json"]["type"],
        "string"
    );
    #[derive(Deserialize)]
    struct Manifest {
        protocol_version: ProtocolVersion,
        sha256: BTreeMap<String, String>,
    }
    let manifest: Manifest = serde_json::from_slice(&bytes("freeze-manifest.json")).unwrap();
    assert_eq!(manifest.protocol_version, ProtocolVersion::V1);
    let expected = [
        "schema.json",
        "observations.json",
        "artifact-success.json",
        "artifact-unavailable.json",
        "artifact-too-large.json",
        "digest-mismatch.json",
        "full-1.json",
        "full-2.json",
        "delta.json",
        "snapshot-expired.json",
        "checkpoint-expired.json",
        "forbidden.json",
        "store-unavailable.json",
        "usage.json",
        "config.json",
        "store.json",
        "unknown-version.json",
    ];
    assert_eq!(manifest.sha256.len(), expected.len());
    for name in expected {
        assert_eq!(manifest.sha256[name], sha256(&bytes(name)), "{name}");
    }
}

#[test]
fn full_snapshot_and_delta_preserve_revisions_and_bindings() {
    let pages = full();
    assert_eq!(pages[0].1.entities.last().unwrap().change_seq, 7);
    assert_eq!(pages[1].1.links.first().unwrap().change_seq, 7);
    let snapshot = validate_snapshot(&pages, None, &artifacts()).unwrap();
    assert_eq!(snapshot.checkpoint(), "checkpoint-full");
    assert_eq!(
        snapshot.unsupported_links().iter().collect::<Vec<_>>(),
        vec![&rf("unsupported", "r1")]
    );
    for (suffix, state, head) in [
        ("a", State::Failed, "head-a1"),
        ("b", State::Passed, "head-b1"),
    ] {
        let check = rf(&format!("check-{suffix}"), "r1");
        let repair = rf(&format!("repair-{suffix}"), "r1");
        let binding = rf(&format!("binding-{suffix}"), "r1");
        assert!(
            matches!(&snapshot.record(&binding).unwrap().data, RecordData::Link {
            relation: Relation::VerificationTestsRepair, from, to } if from == &check && to == &repair)
        );
        assert!(
            matches!(&snapshot.record(&check).unwrap().data, RecordData::Entity {
            entity: EntityPayload::Verification { tested_head: Some(h), state:s, .. } } if h == head && *s == state)
        );
        assert!(
            matches!(&snapshot.record(&repair).unwrap().data, RecordData::Entity {
            entity: EntityPayload::RepairAttempt { result_head: Some(h), state:s, .. } } if h == head && *s == state)
        );
    }
    assert_eq!(
        snapshot
            .record(&rf("finding-a", "r1"))
            .unwrap()
            .revision_seq,
        1
    );
    assert_eq!(
        snapshot
            .record(&rf("finding-a", "r2"))
            .unwrap()
            .revision_seq,
        2
    );
    assert!(matches!(
        snapshot.record(&rf("finding-b", "r2")).unwrap().data,
        RecordData::EntityTombstone { .. }
    ));
    assert!(matches!(
        snapshot.record(&rf("addresses-a", "r2")).unwrap().data,
        RecordData::LinkTombstone { .. }
    ));
    let delta = [page("delta.json")];
    assert_eq!(
        validate_snapshot(&delta, None, &artifacts()).unwrap_err(),
        ValidationError::SnapshotMetadata
    );
    let next = validate_snapshot(&delta, Some(&snapshot), &artifacts()).unwrap();
    assert_eq!(next.checkpoint(), "checkpoint-delta");
    assert!(
        matches!(&next.record(&rf("delta-addresses","r1")).unwrap().data,RecordData::Link { from,to,.. }
        if *from == rf("repair-b","r1") && *to == rf("finding-a","r1"))
    );
}

fn edit(record: &mut Record, op: impl FnOnce(&mut serde_json::Value)) {
    let mut body = serde_json::from_str(&record.body_json).unwrap();
    op(&mut body);
    record.body_json = serde_json::to_string(&body).unwrap();
    record.body_sha256 = sha256(record.body_json.as_bytes());
}

#[test]
fn malformed_snapshots_report_the_failed_rule() {
    use ValidationError::*;
    type Mutation = fn(&mut [(ExportRequest, ExportPage); 2]);
    let cases: [(ValidationError, Mutation); 12] = [
        (InvalidRequest, |p| {
            p[1].0.after = Some("another-token".into());
        }),
        (PageTokens, |p| {
            p[0].1.checkpoint = Some("early".into());
        }),
        (SnapshotMetadata, |p| {
            p[1].1.snapshot_id = "changed".into();
        }),
        (BodyDigest, |p| {
            p[0].1.entities[0].body_json.push(' ');
        }),
        (BodyIdentity, |p| {
            edit(&mut p[0].1.entities[0], |b| b["revision_seq"] = 2.into())
        }),
        (RecordKindMismatch, |p| {
            edit(&mut p[0].1.entities[0], |b| {
                b["record_kind"] = "link".into()
            })
        }),
        (Ordering, |p| {
            let tied = p[1].1.links.remove(0);
            p[0].1.links.push(tied);
            let earlier = p[0].1.entities.pop().unwrap();
            p[1].1.entities.insert(0, earlier);
        }),
        (DuplicateRevision, |p| {
            let duplicate = p[0].1.entities[0].clone();
            p[0].1.entities.insert(0, duplicate);
        }),
        (Watermark, |p| {
            p[0].1.entities[0].change_seq = 2;
        }),
        (MissingEndpoint, |p| {
            p[1].1
                .gaps
                .retain(|g| g.evidence_class != EvidenceClass::Relation);
        }),
        (HeadMismatch, |p| {
            let r = p[1]
                .1
                .entities
                .iter_mut()
                .find(|r| r.source_id == "check-b")
                .unwrap();
            edit(r, |b| b["payload"]["tested_head"] = "other".into());
        }),
        (InvalidLocator, |p| {
            edit(&mut p[0].1.entities[0], |b| {
                b["source_refs"][0]["locator"]["pointer"] = "/absent".into()
            })
        }),
    ];
    for (error, change) in cases {
        let mut p = full();
        change(&mut p);
        assert_eq!(
            validate_snapshot(&p, None, &artifacts()).unwrap_err(),
            error
        );
    }
}

#[test]
fn unknown_version_and_required_enum_fail_but_additions_parse() {
    assert_eq!(
        decode_page(&bytes("unknown-version.json")).unwrap_err(),
        ValidationError::UnsupportedProtocol
    );
    let mut p = full();
    edit(&mut p[0].1.entities[0], |b| {
        b["type"] = "future_entity".into()
    });
    assert_eq!(
        validate_snapshot(&p, None, &artifacts()).unwrap_err(),
        ValidationError::InvalidJson
    );
    let mut p = full();
    edit(&mut p[0].1.entities[0], |b| {
        b["future_optional"] = true.into()
    });
    validate_snapshot(&p, None, &artifacts()).unwrap();
}

#[test]
fn artifact_receipts_and_query_errors_use_literal_codes() {
    #[derive(Deserialize)]
    struct ArtifactFixture {
        request: ArtifactRequest,
        envelope: QueryEnvelope<ArtifactResult>,
    }
    let f: ArtifactFixture = serde_json::from_slice(&bytes("artifact-success.json")).unwrap();
    f.envelope.validate_artifact_receipt().unwrap();
    let result = f.envelope.result.unwrap();
    let expected = &full()[0].1.artifacts[0];
    let mut artifact = bytes("observations.json");
    let mut redirected = result.clone();
    redirected.descriptor.identity.source_id = "other-source".into();
    assert_eq!(
        validate_artifact_result(&f.request, expected, &redirected, &artifact),
        Err(ValidationError::ArtifactIdentityMismatch)
    );
    redirected.descriptor = expected.clone();
    redirected.descriptor.availability = Availability::Unavailable;
    assert_eq!(
        validate_artifact_result(&f.request, expected, &redirected, &artifact),
        Err(ValidationError::ArtifactUnavailable)
    );
    validate_artifact_result(&f.request, expected, &result, &artifact).unwrap();
    artifact[0] ^= 1;
    assert_eq!(
        validate_artifact_result(&f.request, expected, &result, &artifact),
        Err(ValidationError::ArtifactDigest)
    );
    for (name, code, kind) in [
        (
            "snapshot-expired.json",
            ErrorCode::SnapshotExpired,
            CommandKind::EvidenceExport,
        ),
        (
            "checkpoint-expired.json",
            ErrorCode::CheckpointExpired,
            CommandKind::EvidenceExport,
        ),
        (
            "artifact-unavailable.json",
            ErrorCode::ArtifactUnavailable,
            CommandKind::EvidenceArtifact,
        ),
        (
            "artifact-too-large.json",
            ErrorCode::ArtifactTooLarge,
            CommandKind::EvidenceArtifact,
        ),
        (
            "digest-mismatch.json",
            ErrorCode::DigestMismatch,
            CommandKind::EvidenceArtifact,
        ),
        (
            "forbidden.json",
            ErrorCode::Forbidden,
            CommandKind::EvidenceExport,
        ),
        (
            "store-unavailable.json",
            ErrorCode::StoreUnavailable,
            CommandKind::EvidenceExport,
        ),
        ("usage.json", ErrorCode::Usage, CommandKind::EvidenceExport),
        (
            "config.json",
            ErrorCode::Config,
            CommandKind::EvidenceExport,
        ),
        ("store.json", ErrorCode::Store, CommandKind::EvidenceExport),
    ] {
        let reason = match kind {
            CommandKind::EvidenceExport => decode_export_envelope(&bytes(name)).unwrap().reason,
            CommandKind::EvidenceArtifact => {
                let envelope: QueryEnvelope<ArtifactResult> =
                    serde_json::from_slice(&bytes(name)).unwrap();
                envelope.validate_artifact_receipt().unwrap();
                envelope.reason
            }
        };
        assert_eq!(reason.unwrap().code, code);
    }
}

#[test]
fn revision_sequences_and_intrinsic_references_are_checked() {
    use ValidationError::*;
    let mut p = full();
    let revised = p[1]
        .1
        .entities
        .iter_mut()
        .find(|r| r.source_id == "finding-a")
        .unwrap();
    revised.revision_seq = 1;
    edit(revised, |b| b["revision_seq"] = 1.into());
    assert_eq!(
        validate_snapshot(&p, None, &artifacts()).unwrap_err(),
        RevisionSequence
    );
    p[0].1.entities[0].revision_seq = 0;
    assert_eq!(
        validate_snapshot(&p, None, &artifacts()).unwrap_err(),
        InvalidField
    );
    let mut p = full();
    let exec = p[0]
        .1
        .entities
        .iter_mut()
        .find(|r| r.source_id == "exec-a")
        .unwrap();
    edit(exec, |b| {
        b["payload"]["work"]["source_id"] = "round-a".into()
    });
    assert_eq!(
        validate_snapshot(&p, None, &artifacts()).unwrap_err(),
        EndpointType
    );
}

#[test]
fn descriptor_and_bytes_fail_independently() {
    use ValidationError::*;
    let mut p = full();
    p[0].1.artifacts[0].byte_length += 1;
    assert_eq!(
        validate_snapshot(&p, None, &artifacts()).unwrap_err(),
        ArtifactLength
    );
    p[0].1.artifacts[0].identity.store_epoch = "different-epoch".into();
    assert_eq!(
        validate_snapshot(&p, None, &artifacts()).unwrap_err(),
        ArtifactIdentityMismatch
    );
    p[0].1.artifacts.clear();
    assert_eq!(
        validate_snapshot(&p, None, &artifacts()).unwrap_err(),
        ArtifactIdentityMismatch
    );
    let mut p = full();
    assert_eq!(
        validate_snapshot(&p, None, &BTreeMap::new()).unwrap_err(),
        ArtifactUnavailable
    );
    let mut extra = p[0].1.artifacts[0].clone();
    extra.artifact_id = "unreferenced".into();
    extra.availability = Availability::Unavailable;
    p[0].1.artifacts.push(extra);
    validate_snapshot(&p, None, &artifacts()).unwrap();
}

#[test]
fn envelope_watermark_and_limits_are_checked() {
    use ValidationError::*;
    let f: PageFixture = serde_json::from_slice(&bytes("full-1.json")).unwrap();
    let mut envelope = decode_export_envelope(f.envelope.get().as_bytes()).unwrap();
    envelope.as_of = Some(9);
    assert_eq!(envelope.validate_export(), Err(InvalidField));
    envelope.as_of = Some(10);
    envelope.command.as_mut().unwrap().kind = CommandKind::EvidenceArtifact;
    assert_eq!(envelope.validate_export(), Err(InvalidField));
    let [mut p, _] = full();
    p.0.limit = 501;
    assert_eq!(validate_page(&p.0, &p.1), Err(InvalidRequest));
    p.0.limit = 1;
    assert_eq!(validate_page(&p.0, &p.1), Err(PageSize));
    assert_eq!(decode_page(&vec![b' '; MAX_PAGE_BYTES + 1]), Err(PageSize));
    let mut a = p.1.artifacts.remove(0);
    a.byte_length = MAX_ARTIFACT_BYTES as u64 + 1;
    assert_eq!(validate_descriptor(&a), Err(ArtifactTooLarge));
}

#[test]
fn artifact_aliases_cannot_rebind_an_immutable_revision() {
    let mut pages = full();
    let mut alias = pages[0].1.artifacts[0].clone();
    alias.artifact_id = "alias".into();
    let mut content = artifacts();
    content.insert(alias.artifact_id.clone(), bytes("observations.json"));
    pages[1].1.artifacts.push(alias.clone());
    validate_snapshot(&pages, None, &content).unwrap();

    let different = b"independently valid alternate bytes".to_vec();
    alias.sha256 = sha256(&different);
    alias.byte_length = different.len() as u64;
    content.insert(alias.artifact_id.clone(), different);
    validate_artifact(&alias, &content["alias"]).unwrap();
    pages[1].1.artifacts[0] = alias.clone();
    assert_eq!(
        validate_snapshot(&pages, None, &content).unwrap_err(),
        ValidationError::ArtifactIdentityMismatch
    );

    let history = validate_snapshot(&full(), None, &content).unwrap();
    let mut delta = [page("delta.json")];
    delta[0].1.artifacts.push(alias);
    assert_eq!(
        validate_snapshot(&delta, Some(&history), &content).unwrap_err(),
        ValidationError::ArtifactIdentityMismatch
    );
    delta[0].1.artifacts[0].identity.revision = "different-revision-same-sequence".into();
    assert_eq!(
        validate_snapshot(&delta, Some(&history), &content).unwrap_err(),
        ValidationError::ArtifactIdentityMismatch
    );
}

#[test]
fn raw_export_length_counts_unknown_fields_and_omitted_nulls() {
    let fixture: PageFixture = serde_json::from_slice(&bytes("full-1.json")).unwrap();
    let mut envelope: serde_json::Value = serde_json::from_str(fixture.envelope.get()).unwrap();
    let page = envelope["result"].as_object_mut().unwrap();
    page.remove("after_seq");
    page.remove("checkpoint");
    page.insert("future_optional".into(), "".into());
    let remaining = MAX_PAGE_BYTES - serde_json::to_vec(&envelope["result"]).unwrap().len();
    envelope["result"]["future_optional"] = "x".repeat(remaining).into();
    let raw = serde_json::to_vec(&envelope).unwrap();
    let decoded = decode_export_envelope(&raw).unwrap();
    validate_page(&fixture.request, &decoded.result.unwrap()).unwrap();

    // A known nested field makes the compact DTO exceed the raw limit after null expansion.
    let mut near_limit = envelope.clone();
    near_limit["result"]
        .as_object_mut()
        .unwrap()
        .remove("future_optional");
    let empty_intent = near_limit["result"]["entities"][0]["body_json"]
        .as_str()
        .unwrap()
        .to_owned();
    let raw_size = serde_json::to_vec(&near_limit["result"]).unwrap().len();
    near_limit["result"]["entities"][0]["body_json"] =
        (empty_intent + &" ".repeat(MAX_PAGE_BYTES - raw_size)).into();
    let body = near_limit["result"]["entities"][0]["body_json"]
        .as_str()
        .unwrap();
    near_limit["result"]["entities"][0]["body_sha256"] = sha256(body.as_bytes()).into();
    let decoded = decode_export_envelope(&serde_json::to_vec(&near_limit).unwrap()).unwrap();
    let page = decoded.result.unwrap();
    assert!(serde_json::to_vec(&page).unwrap().len() > MAX_PAGE_BYTES);
    validate_page(&fixture.request, &page).unwrap();

    envelope["result"]["future_optional"] = "x".repeat(remaining + 1).into();
    assert_eq!(
        decode_export_envelope(&serde_json::to_vec(&envelope).unwrap()).unwrap_err(),
        ValidationError::PageSize
    );
}

#[test]
fn missing_retained_text_requires_exact_evidence_gaps() {
    let mut pages = full();
    pages[1]
        .1
        .gaps
        .retain(|g| g.evidence_class != EvidenceClass::Body);
    assert_eq!(
        validate_snapshot(&pages, None, &artifacts()).unwrap_err(),
        ValidationError::MissingEvidenceGap
    );
    let mut pages = full();
    edit(&mut pages[0].1.entities[0], |b| {
        b["payload"]
            .as_object_mut()
            .unwrap()
            .remove("intent")
            .map(drop)
            .unwrap()
    });
    assert_eq!(
        validate_snapshot(&pages, None, &artifacts()).unwrap_err(),
        ValidationError::MissingEvidenceGap
    );
    pages[0].1.gaps.push(Gap {
        code: GapCode::NotRecorded,
        evidence_class: EvidenceClass::Intent,
        scope: GapScope::Revision {
            record: rf("work", "r1"),
        },
        reason: "Original intent expired.".into(),
    });
    validate_snapshot(&pages, None, &artifacts()).unwrap();
}

#[test]
fn artifact_availability_can_change_between_snapshots() {
    let history = validate_snapshot(&full(), None, &artifacts()).unwrap();
    let mut delta = [page("delta.json")];
    let mut descriptor = full()[0].1.artifacts[0].clone();
    descriptor.availability = Availability::RetentionExpired;
    delta[0].1.artifacts.push(descriptor.clone());
    let no_bytes = BTreeMap::new();
    assert_eq!(
        validate_snapshot(&delta, Some(&history), &no_bytes).unwrap_err(),
        ValidationError::ArtifactUnavailable
    );
    delta[0].1.gaps.push(Gap {
        code: GapCode::RetentionExpired,
        evidence_class: EvidenceClass::Artifact,
        scope: GapScope::Artifact {
            artifact_id: descriptor.artifact_id,
            sha256: descriptor.sha256,
        },
        reason: "Bytes expired.".into(),
    });
    validate_snapshot(&delta, Some(&history), &no_bytes).unwrap();
    delta[0].1.artifacts[0].media_type = "application/vnd.example+json".into();
    assert_eq!(
        validate_snapshot(&delta, Some(&history), &no_bytes).unwrap_err(),
        ValidationError::ArtifactIdentityMismatch
    );
}

#[test]
fn snapshot_and_checkpoint_bindings_survive_multiple_deltas() {
    let full = full();
    let history = validate_snapshot(&full, None, &artifacts()).unwrap();
    let [mut delta] = [page("delta.json")];
    let next = validate_snapshot(&[delta.clone()], Some(&history), &artifacts()).unwrap();
    delta.1.snapshot_id = full[0].1.snapshot_id.clone();
    assert_eq!(
        validate_snapshot(&[delta.clone()], Some(&history), &artifacts()).unwrap_err(),
        ValidationError::SnapshotMetadata
    );
    delta.1.snapshot_id = "snapshot-third".into();
    delta.1.checkpoint = Some(history.checkpoint().into());
    assert_eq!(
        validate_snapshot(&[delta.clone()], Some(&history), &artifacts()).unwrap_err(),
        ValidationError::SnapshotMetadata
    );
    delta.0.after = Some(next.checkpoint().into());
    delta.1.after_seq = Some(11);
    delta.1.links.clear();
    delta.1.checkpoint = Some(next.checkpoint().into());
    validate_snapshot(&[delta.clone()], Some(&next), &artifacts()).unwrap();
    delta.1.snapshot_seq = 12;
    delta.1.checkpoint = Some(history.checkpoint().into());
    assert_eq!(
        validate_snapshot(&[delta.clone()], Some(&next), &artifacts()).unwrap_err(),
        ValidationError::SnapshotMetadata
    );
    delta.1.checkpoint = Some("checkpoint-third".into());
    delta.1.snapshot_id = full[0].1.snapshot_id.clone();
    assert_eq!(
        validate_snapshot(&[delta], Some(&next), &artifacts()).unwrap_err(),
        ValidationError::SnapshotMetadata
    );
}

#[test]
fn recovering_an_endpoint_rechecks_history_without_promoting_old_links() {
    let mut pages = full();
    let entities = &mut pages[1].1.entities;
    let index = entities
        .iter()
        .position(|r| r.source_id == "repair-b")
        .unwrap();
    let mut repair = entities.remove(index);
    pages[1].1.gaps.push(Gap {
        code: GapCode::NotRecorded,
        evidence_class: EvidenceClass::Relation,
        scope: GapScope::Revision {
            record: rf("repair-b", "r1"),
        },
        reason: "Repair observation missing.".into(),
    });
    let history = validate_snapshot(&pages, None, &artifacts()).unwrap();
    assert!(history.unsupported_links().contains(&rf("binding-b", "r1")));

    let mut delta = [page("delta.json")];
    repair.change_seq = 11;
    let mut wrong = repair.clone();
    edit(&mut wrong, |b| {
        b["payload"]["result_head"] = "wrong-head".into()
    });
    delta[0].1.entities.push(wrong);
    assert_eq!(
        validate_snapshot(&delta, Some(&history), &artifacts()).unwrap_err(),
        ValidationError::HeadMismatch
    );
    delta[0].1.entities[0] = repair;
    let mut binding = pages[1]
        .1
        .links
        .iter()
        .find(|r| r.source_id == "binding-b")
        .unwrap()
        .clone();
    binding.change_seq = 11;
    binding.revision = "r2".into();
    binding.revision_seq = 2;
    edit(&mut binding, |b| {
        b["revision"] = "r2".into();
        b["revision_seq"] = 2.into();
    });
    delta[0].1.links.insert(0, binding);
    let recovered = validate_snapshot(&delta, Some(&history), &artifacts()).unwrap();
    assert!(
        recovered
            .unsupported_links()
            .contains(&rf("binding-b", "r1"))
    );
    assert!(
        !recovered
            .unsupported_links()
            .contains(&rf("binding-b", "r2"))
    );
    assert!(recovered.record(&rf("binding-b", "r2")).is_some());
}

#[test]
fn a_delta_resolves_missing_endpoints_against_historical_gaps() {
    let mut pages = full();
    let entities = &mut pages[1].1.entities;
    let index = entities
        .iter()
        .position(|r| r.source_id == "repair-b")
        .unwrap();
    entities.remove(index);
    pages[1].1.gaps.push(Gap {
        code: GapCode::NotRecorded,
        evidence_class: EvidenceClass::Relation,
        scope: GapScope::Revision {
            record: rf("repair-b", "r1"),
        },
        reason: "Repair observation missing.".into(),
    });
    let history = validate_snapshot(&pages, None, &artifacts()).unwrap();
    // The delta's new link references the endpoint still declared missing
    // by the validated history's relation gap; the delta does not
    // re-declare it, and the link is archived as unsupported.
    let delta = [page("delta.json")];
    let next = validate_snapshot(&delta, Some(&history), &artifacts()).unwrap();
    assert!(
        next.unsupported_links()
            .contains(&rf("delta-addresses", "r1"))
    );
}

#[test]
fn assertion_kinds_and_required_fields_are_explicit() {
    let mut kinds = BTreeSet::new();
    for (.., page) in full().iter().chain(&[page("delta.json")]) {
        for record in page.entities.iter().chain(page.links.iter()) {
            let body: serde_json::Value = serde_json::from_str(&record.body_json).unwrap();
            kinds.insert(body["assertion"]["kind"].as_str().unwrap().to_string());
        }
    }
    assert_eq!(
        kinds.into_iter().collect::<Vec<_>>(),
        [
            "human_inference",
            "model_interpretation",
            "observed",
            "source_claim"
        ]
    );
    // Interpretation assertions require author, tool, version and rationale:
    // a missing field fails to parse; a blank one is an invalid value.
    let mut pages = full();
    let outcome = pages[1]
        .1
        .entities
        .iter_mut()
        .find(|r| r.source_id == "outcome")
        .unwrap();
    edit(outcome, |b| {
        b["assertion"].as_object_mut().unwrap().remove("rationale");
    });
    assert_eq!(
        validate_snapshot(&pages, None, &artifacts()).unwrap_err(),
        ValidationError::InvalidJson
    );
    let mut pages = full();
    let finding = pages[1]
        .1
        .entities
        .iter_mut()
        .find(|r| r.source_id == "finding-a" && r.revision == "r2")
        .unwrap();
    edit(finding, |b| b["assertion"]["author"] = " ".into());
    assert_eq!(
        validate_snapshot(&pages, None, &artifacts()).unwrap_err(),
        ValidationError::InvalidField
    );
}

#[test]
fn recovered_intrinsic_endpoint_must_have_the_recorded_type() {
    let mut pages = full();
    let index = pages[1]
        .1
        .entities
        .iter()
        .position(|r| r.source_id == "outcome")
        .unwrap();
    let mut missing = pages[1].1.entities.remove(index);
    pages[1].1.gaps.push(Gap {
        code: GapCode::NotRecorded,
        evidence_class: EvidenceClass::Relation,
        scope: GapScope::Revision {
            record: rf("outcome", "r1"),
        },
        reason: "Outcome unavailable.".into(),
    });
    let history = validate_snapshot(&pages, None, &artifacts()).unwrap();
    missing.change_seq = 11;
    edit(&mut missing, |body| {
        body["type"] = "requirement".into();
        body["payload"] = serde_json::json!({"requirement_id":"outcome","provenance":"dispatch"});
    });
    let mut delta = [page("delta.json")];
    delta[0].1.entities.push(missing);
    assert_eq!(
        validate_snapshot(&delta, Some(&history), &artifacts()).unwrap_err(),
        ValidationError::EndpointType
    );
}

#[test]
fn artifact_and_record_revisions_share_identity_mappings() {
    let mut pages = full();
    let work = pages[0].1.entities[0].clone();
    let descriptor = &mut pages[0].1.artifacts[0];
    descriptor.identity.source_id = work.source_id;
    descriptor.identity.revision = work.revision;
    descriptor.identity.revision_seq = work.revision_seq;
    assert_ne!(descriptor.sha256, work.body_sha256);
    validate_snapshot(&pages, None, &artifacts()).unwrap();

    pages[0].1.artifacts[0].identity.revision_seq = work.revision_seq + 1;
    assert_eq!(
        validate_snapshot(&pages, None, &artifacts()).unwrap_err(),
        ValidationError::ArtifactIdentityMismatch
    );
    pages[0].1.artifacts[0].identity.revision_seq = work.revision_seq;
    pages[0].1.artifacts[0].identity.revision = "conflicting-revision".into();
    assert_eq!(
        validate_snapshot(&pages, None, &artifacts()).unwrap_err(),
        ValidationError::ArtifactIdentityMismatch
    );

    let history = validate_snapshot(&full(), None, &artifacts()).unwrap();
    let mut delta = [page("delta.json")];
    let mut alias = pages[0].1.artifacts[0].clone();
    alias.artifact_id = "new-alias".into();
    delta[0].1.artifacts.push(alias);
    assert_eq!(
        validate_snapshot(&delta, Some(&history), &artifacts()).unwrap_err(),
        ValidationError::ArtifactIdentityMismatch
    );
}
