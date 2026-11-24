//! Artifact classification and named export-profile contract.

use workloop_store::artifact_class::{
    ArtifactClass, classify_artifact, classify_artifact_id, classify_payload,
};
use workloop_store::export_profile::ExportProfile;

#[test]
fn named_profiles_exclude_planner_classes_by_default() {
    let full = ExportProfile::named("full").unwrap();
    assert!(full.excluded_classes().next().is_none());
    assert!(!full.default_exclusion_on());

    for name in ["sanitized", "shared"] {
        let profile = ExportProfile::named(name).unwrap();
        assert_eq!(profile.name(), name);
        assert!(profile.default_exclusion_on());
        for class in [
            ArtifactClass::Plan,
            ArtifactClass::Decision,
            ArtifactClass::Preview,
            ArtifactClass::Manifest,
        ] {
            assert!(profile.excludes(class), "{name} must exclude {class}");
        }
        assert!(!profile.excludes(ArtifactClass::Evidence));
    }
    assert!(ExportProfile::named("unknown-profile").is_none());
}

#[test]
fn widening_is_explicit_and_removes_only_the_named_class() {
    let mut profile = ExportProfile::sanitized();
    assert!(profile.widen(ArtifactClass::Plan));
    assert!(!profile.excludes(ArtifactClass::Plan));
    assert!(profile.excludes(ArtifactClass::Decision));
    assert_eq!(
        profile.widened_classes().collect::<Vec<_>>(),
        vec![ArtifactClass::Plan]
    );
    // Widening an already-exportable class changes nothing.
    assert!(!profile.widen(ArtifactClass::Plan));
    assert!(!profile.widen(ArtifactClass::Evidence));
}

#[test]
fn classification_prefers_registration_then_id_then_payload_field() {
    assert_eq!(classify_artifact_id("plan:release"), ArtifactClass::Plan);
    assert_eq!(
        classify_artifact_id("docs/implementation-plan.md"),
        ArtifactClass::Plan
    );
    assert_eq!(
        classify_artifact_id("decision/adr-7.json"),
        ArtifactClass::Decision
    );
    assert_eq!(classify_artifact_id("event:12"), ArtifactClass::Evidence);
    assert_eq!(
        classify_payload(br#"{"metadata":{"artifact_class":"plan"}}"#),
        ArtifactClass::Plan
    );
    assert_eq!(
        classify_artifact("event:12", Some(br#"{"class":"decision"}"#), None),
        ArtifactClass::Decision
    );
    // Explicit registration is authoritative even over an evidence id.
    assert_eq!(
        classify_artifact(
            "event:12",
            Some(br#"{"class":"evidence"}"#),
            Some(ArtifactClass::Plan)
        ),
        ArtifactClass::Plan
    );
    // Prose is never scanned.
    assert_eq!(
        classify_payload(b"this implementation plan mentions decisions"),
        ArtifactClass::Evidence
    );
}

#[test]
fn preview_renders_are_bounded_and_excluded_classes_have_none() {
    use std::collections::BTreeMap;
    use workloop_evidence_protocol::{
        ArtifactDescriptor, ArtifactIdentity, Availability, ExportPage, ProtocolVersion,
    };
    use workloop_store::evidence::{HistoryEvent, HistorySnapshot};
    use workloop_store::export_profile::{PREVIEW_CHARS, profile_page};

    let evidence_payload = serde_json::json!({
        "text": "x".repeat(PREVIEW_CHARS * 4),
    })
    .to_string();
    let plan_payload = serde_json::json!({
        "artifact_class": "plan",
        "text": "private plan",
    })
    .to_string();
    let descriptor = |seq: u64, payload: &str| ArtifactDescriptor {
        artifact_id: format!("event:{seq}"),
        identity: ArtifactIdentity {
            installation_id: "installation".into(),
            store_epoch: "epoch".into(),
            source_id: "event_log".into(),
            revision: format!("seq-{seq}"),
            revision_seq: seq,
        },
        sha256: workloop_evidence_protocol::sha256(payload.as_bytes()),
        byte_length: payload.len() as u64,
        media_type: "application/json".into(),
        availability: Availability::Available,
    };
    let snapshot = HistorySnapshot {
        installation_id: "installation".into(),
        store_epoch: "epoch".into(),
        watermark: 2,
        retained_from_seq: 1,
        events: vec![
            HistoryEvent {
                seq: 1,
                kind: "effect".into(),
                payload: evidence_payload.clone(),
            },
            HistoryEvent {
                seq: 2,
                kind: "effect".into(),
                payload: plan_payload.clone(),
            },
        ],
        artifact_classes: BTreeMap::new(),
    };
    let page = ExportPage {
        protocol_version: ProtocolVersion::V1,
        installation_id: "installation".into(),
        store_epoch: "epoch".into(),
        snapshot_id: "snapshot".into(),
        snapshot_seq: 2,
        retained_from_seq: 1,
        after_seq: None,
        entities: vec![],
        links: vec![],
        artifacts: vec![
            descriptor(1, &evidence_payload),
            descriptor(2, &plan_payload),
        ],
        gaps: vec![],
        next_cursor: None,
        checkpoint: Some("checkpoint".into()),
    };

    let full = profile_page(page.clone(), &snapshot, &ExportProfile::full());
    assert_eq!(full.included.len(), 2);
    let preview = full
        .previews
        .iter()
        .find(|preview| preview.artifact_id == "event:1")
        .unwrap();
    assert_eq!(preview.text.chars().count(), PREVIEW_CHARS);
    assert!(preview.truncated);

    let sanitized = profile_page(page, &snapshot, &ExportProfile::sanitized());
    assert_eq!(sanitized.included.len(), 1);
    assert!(
        sanitized
            .previews
            .iter()
            .all(|preview| preview.artifact_id != "event:2"),
        "an excluded artifact must not get a preview render"
    );
    assert!(sanitized.manifest.excluded_artifact("event:2").is_some());
}
