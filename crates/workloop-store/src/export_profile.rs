//! Named evidence-export profiles and the profiled export manifest.
//!
//! A profile decides which artifact classes may leave the installation.
//! `sanitized` and `shared` exclude planner-authored classes by default;
//! `full` (also spelled `local`) exports every class. Widening an
//! exclusion is an explicit profile change and is recorded in the
//! manifest, so an audited export always states what left and what did
//! not.
//!
//! Exclusion runs at the replay: an event whose artifact class the
//! profile excludes projects no records and feeds no derived bindings,
//! so the exported page carries neither the artifact nor copies of its
//! payload. Excluded artifacts emit no content file or preview. A
//! still-referenced excluded artifact keeps a descriptor shell marked
//! `unavailable` plus an artifact-class gap, so the page stays valid;
//! and every excluded event in the export's range keeps its identity,
//! digest, length, media type, and availability in
//! [`ExportManifest::artifacts`] so the exclusion is auditable without
//! carrying the content.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use workloop_evidence_protocol::{
    ArtifactDescriptor, Availability, ErrorCode, EvidenceClass, ExportError, ExportPage,
    ExportRequest, Gap, GapCode, GapScope, MAX_PAGE_BYTES,
};

use crate::artifact_class::{ArtifactClass, classify_artifact};
use crate::evidence::{
    HistorySnapshot, artifact_seq, event_artifact_id, event_descriptor, export_page_excluding,
};

/// The manifest format version.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
/// The full/local profile name.
pub const FULL_PROFILE: &str = "full";
/// The sanitized profile name.
pub const SANITIZED_PROFILE: &str = "sanitized";
/// The shared profile name.
pub const SHARED_PROFILE: &str = "shared";
/// Maximum characters copied into one preview render.
pub const PREVIEW_CHARS: usize = 256;
/// Maximum gaps one export page may carry.
pub const MAX_PAGE_GAPS: usize = 2000;

/// A named export profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportProfile {
    name: String,
    excluded: BTreeSet<ArtifactClass>,
    widened: BTreeSet<ArtifactClass>,
}

impl ExportProfile {
    /// Resolve a documented profile name. `local` and `default` alias
    /// `full`; unknown names are refused by the caller.
    pub fn named(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "" | "full" | "local" | "default" => Some(Self::full()),
            "sanitized" | "sanitised" | "sanitize" | "private" => Some(Self::sanitized()),
            "shared" | "share" | "shareable" | "public" => Some(Self::shared()),
            _ => None,
        }
    }

    /// Every class is exportable.
    pub fn full() -> Self {
        Self {
            name: FULL_PROFILE.into(),
            excluded: BTreeSet::new(),
            widened: BTreeSet::new(),
        }
    }

    /// Private planner-authored and derived classes are excluded by
    /// default.
    pub fn sanitized() -> Self {
        Self {
            name: SANITIZED_PROFILE.into(),
            excluded: ArtifactClass::ALL
                .into_iter()
                .filter(|class| class.excluded_by_default())
                .collect(),
            widened: BTreeSet::new(),
        }
    }

    /// Shared exports use the sanitized default exclusions, under their
    /// own profile name so the manifest states the audience.
    pub fn shared() -> Self {
        Self {
            name: SHARED_PROFILE.into(),
            ..Self::sanitized()
        }
    }

    /// Alias for [`ExportProfile::named`].
    pub fn from_name(name: &str) -> Option<Self> {
        Self::named(name)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether this profile applies default exclusions.
    pub fn default_exclusion_on(&self) -> bool {
        self.name == SANITIZED_PROFILE || self.name == SHARED_PROFILE
    }

    pub fn excluded_classes(&self) -> impl Iterator<Item = ArtifactClass> + '_ {
        self.excluded.iter().copied()
    }

    pub fn widened_classes(&self) -> impl Iterator<Item = ArtifactClass> + '_ {
        self.widened.iter().copied()
    }

    /// Whether `class` must not be exported under this profile.
    pub fn excludes(&self, class: ArtifactClass) -> bool {
        self.excluded.contains(&class)
    }

    /// Explicitly widen the profile by exporting `class`.
    ///
    /// Returns `true` when the profile changed. The widening is recorded
    /// in the manifest, which is the only way to carry an excluded class
    /// past a sanitized/shared boundary.
    pub fn widen(&mut self, class: ArtifactClass) -> bool {
        if self.excluded.remove(&class) {
            self.widened.insert(class);
            true
        } else {
            false
        }
    }
}

/// One integrity entry in the export manifest. Present for every
/// artifact the page would have carried, whether or not its content was
/// exported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactIntegrityEntry {
    pub artifact_id: String,
    pub class: ArtifactClass,
    pub excluded: bool,
    pub content_included: bool,
    pub preview_included: bool,
    pub sha256: String,
    pub byte_length: u64,
    pub media_type: String,
    pub identity: workloop_evidence_protocol::ArtifactIdentity,
    pub availability: Availability,
}

/// One exported preview render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewEntry {
    pub artifact_id: String,
    pub class: ArtifactClass,
    pub sha256: String,
    pub path: String,
    pub truncated: bool,
}

/// The manifest written beside every profiled export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportManifest {
    pub schema_version: u32,
    pub profile: String,
    pub default_exclusion_on: bool,
    pub excluded_classes: Vec<ArtifactClass>,
    pub widened_classes: Vec<ArtifactClass>,
    pub excludes_planner_content: bool,
    pub artifacts: Vec<ArtifactIntegrityEntry>,
    pub previews: Vec<PreviewEntry>,
}

impl ExportManifest {
    pub fn excluded_artifact(&self, artifact_id: &str) -> Option<&ArtifactIntegrityEntry> {
        self.artifacts
            .iter()
            .find(|entry| entry.artifact_id == artifact_id && entry.excluded)
    }
}

/// One artifact whose content is included in the export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncludedArtifact {
    pub descriptor: ArtifactDescriptor,
    pub class: ArtifactClass,
    pub bytes: Vec<u8>,
}

/// A preview render derived from an included artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactPreview {
    pub artifact_id: String,
    pub class: ArtifactClass,
    pub sha256: String,
    pub path: String,
    pub text: String,
    pub truncated: bool,
}

/// A profiled page plus everything an export bundle writes.
#[derive(Debug, Clone)]
pub struct ProfiledExport {
    pub page: ExportPage,
    pub manifest: ExportManifest,
    pub included: Vec<IncludedArtifact>,
    pub previews: Vec<ArtifactPreview>,
}

/// One generated bundle file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

fn preview_text(bytes: &[u8]) -> (String, bool) {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index >= PREVIEW_CHARS {
            return (out, true);
        }
        out.push(ch);
    }
    (out, false)
}

fn safe_name(artifact_id: &str, sha256: &str) -> String {
    let mut name: String = artifact_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    while name.starts_with('.') {
        name.remove(0);
    }
    if name.is_empty() {
        name.push_str("artifact");
    }
    if name.len() > 80 {
        name.truncate(80);
    }
    let suffix = sha256.get(..12).unwrap_or(sha256);
    format!("{name}-{suffix}")
}

/// The recorded rows whose artifact this profile excludes, keyed by
/// sequence. An excluded row is skipped by the export replay — every
/// record projected from it would be a copy of excluded content — and
/// listed here so the manifest can keep the integrity entry the
/// artifact's absence requires.
fn excluded_event_seqs(
    snap: &HistorySnapshot,
    profile: &ExportProfile,
) -> BTreeMap<u64, ArtifactClass> {
    snap.events
        .iter()
        .filter_map(|event| {
            let id = event_artifact_id(event.seq);
            let class = classify_artifact(
                &id,
                Some(event.payload.as_bytes()),
                snap.artifact_classes.get(&id).copied(),
            );
            profile.excludes(class).then_some((event.seq, class))
        })
        .collect()
}

/// Apply `profile` to a page and build its integrity manifest.
pub fn profile_page(
    mut page: ExportPage,
    snap: &HistorySnapshot,
    profile: &ExportProfile,
) -> ProfiledExport {
    let referenced: BTreeSet<String> = page
        .entities
        .iter()
        .chain(page.links.iter())
        .filter_map(|record| serde_json::from_str::<serde_json::Value>(&record.body_json).ok())
        .filter_map(|body| body.get("source_refs").cloned())
        .filter_map(|refs| refs.as_array().cloned())
        .flatten()
        .filter_map(|source_ref| source_ref.get("artifact_id").cloned())
        .filter_map(|id| id.as_str().map(str::to_string))
        .collect();
    let mut artifacts = Vec::new();
    let mut included = Vec::new();
    let mut previews = Vec::new();
    let mut preview_entries = Vec::new();
    let mut excluded_gaps = Vec::new();
    let payloads: BTreeMap<u64, &str> = snap
        .events
        .iter()
        .map(|event| (event.seq, event.payload.as_str()))
        .collect();

    for descriptor in std::mem::take(&mut page.artifacts) {
        let payload = artifact_seq(&descriptor.artifact_id)
            .and_then(|seq| payloads.get(&seq))
            .map(|payload| payload.as_bytes());
        let registered = snap.artifact_classes.get(&descriptor.artifact_id).copied();
        let class = classify_artifact(&descriptor.artifact_id, payload, registered);
        let excluded = profile.excludes(class);
        if excluded && referenced.contains(&descriptor.artifact_id) {
            excluded_gaps.push(Gap {
                code: GapCode::SourceUnavailable,
                scope: GapScope::Artifact {
                    artifact_id: descriptor.artifact_id.clone(),
                    sha256: descriptor.sha256.clone(),
                },
                evidence_class: EvidenceClass::Artifact,
                reason: format!(
                    "artifact class {class} is excluded by the {} export profile",
                    profile.name()
                ),
            });
        }
        let mut preview_included = false;
        if !excluded && let Some(bytes) = payload {
            let (text, truncated) = preview_text(bytes);
            let path = format!(
                "previews/{}.json",
                safe_name(&descriptor.artifact_id, &descriptor.sha256)
            );
            preview_included = true;
            preview_entries.push(PreviewEntry {
                artifact_id: descriptor.artifact_id.clone(),
                class,
                sha256: descriptor.sha256.clone(),
                path: path.clone(),
                truncated,
            });
            previews.push(ArtifactPreview {
                artifact_id: descriptor.artifact_id.clone(),
                class,
                sha256: descriptor.sha256.clone(),
                path,
                text,
                truncated,
            });
            included.push(IncludedArtifact {
                descriptor: descriptor.clone(),
                class,
                bytes: bytes.to_vec(),
            });
        }
        let content_included = included
            .last()
            .is_some_and(|a| a.descriptor.artifact_id == descriptor.artifact_id);
        artifacts.push(ArtifactIntegrityEntry {
            artifact_id: descriptor.artifact_id.clone(),
            class,
            excluded,
            content_included,
            preview_included,
            sha256: descriptor.sha256.clone(),
            byte_length: descriptor.byte_length,
            media_type: descriptor.media_type.clone(),
            identity: descriptor.identity.clone(),
            availability: descriptor.availability,
        });
        if !excluded {
            page.artifacts.push(descriptor);
        } else if referenced.contains(&descriptor.artifact_id) {
            // A still-cited exclusion keeps a descriptor shell — no
            // content, availability downgraded — so the source
            // references that name it resolve and the page stays
            // valid; the artifact gap emitted above marks the bytes
            // withheld.
            let mut shell = descriptor;
            shell.availability = Availability::Unavailable;
            page.artifacts.push(shell);
        }
    }
    // Excluded events project no records, so their descriptors never
    // reach the page — the manifest still owes each of them an
    // integrity entry for the exclusion to stay auditable.
    let excluded_events = excluded_event_seqs(snap, profile);
    let lower = page.after_seq.unwrap_or(0);
    let mut covered: BTreeSet<String> = artifacts
        .iter()
        .map(|entry| entry.artifact_id.clone())
        .collect();
    for event in &snap.events {
        if event.seq > page.snapshot_seq {
            break;
        }
        if event.seq <= lower {
            continue;
        }
        let Some(&class) = excluded_events.get(&event.seq) else {
            continue;
        };
        let descriptor = event_descriptor(snap, event.seq, event.payload.as_bytes());
        if !covered.insert(descriptor.artifact_id.clone()) {
            continue;
        }
        artifacts.push(ArtifactIntegrityEntry {
            artifact_id: descriptor.artifact_id,
            class,
            excluded: true,
            content_included: false,
            preview_included: false,
            sha256: descriptor.sha256,
            byte_length: descriptor.byte_length,
            media_type: descriptor.media_type,
            identity: descriptor.identity,
            availability: descriptor.availability,
        });
    }

    page.gaps.extend(excluded_gaps);
    let manifest = ExportManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        profile: profile.name().into(),
        default_exclusion_on: profile.default_exclusion_on(),
        excluded_classes: profile.excluded_classes().collect(),
        widened_classes: profile.widened_classes().collect(),
        excludes_planner_content: profile.excludes(ArtifactClass::Plan)
            || profile.excludes(ArtifactClass::Decision),
        artifacts,
        previews: preview_entries,
    };
    ProfiledExport {
        page,
        manifest,
        included,
        previews,
    }
}

/// Produce a profiled export page and its manifest. Events whose
/// artifact class the profile excludes are withheld from the replay
/// itself — an exported record body is a copy of its source payload,
/// so the exclusion must run before projection, not only over the
/// emitted artifact set.
pub fn export_profiled(
    request: &ExportRequest,
    snap: &HistorySnapshot,
    now_unix_seconds: i64,
    profile: &ExportProfile,
) -> Result<ProfiledExport, ExportError> {
    let excluded: BTreeSet<u64> = excluded_event_seqs(snap, profile).keys().copied().collect();
    let page = export_page_excluding(request, snap, now_unix_seconds, &excluded)?;
    let export = profile_page(page, snap, profile);
    if export.page.gaps.len() > MAX_PAGE_GAPS
        || serde_json::to_vec(&export.page)
            .map(|wire| wire.len() > MAX_PAGE_BYTES)
            .unwrap_or(true)
    {
        return Err(ExportError::new(
            ErrorCode::RecordTooLarge,
            "the profiled page's exclusion metadata cannot fit in a page",
        ));
    }
    Ok(export)
}

/// Render every file of one export bundle. Excluded artifacts appear
/// only in `manifest.json` as integrity metadata; no content or preview
/// file is generated for them.
pub fn bundle_files(export: &ProfiledExport) -> Result<Vec<ExportedFile>, serde_json::Error> {
    let mut files = Vec::new();
    files.push(ExportedFile {
        path: "page.json".into(),
        bytes: serde_json::to_vec_pretty(&export.page)?,
    });
    for artifact in &export.included {
        files.push(ExportedFile {
            path: format!(
                "artifacts/{}.json",
                safe_name(
                    &artifact.descriptor.artifact_id,
                    &artifact.descriptor.sha256
                )
            ),
            bytes: artifact.bytes.clone(),
        });
    }
    for preview in &export.previews {
        files.push(ExportedFile {
            path: preview.path.clone(),
            bytes: serde_json::to_vec_pretty(preview)?,
        });
    }
    // The manifest is the bundle commit marker: write it last so a
    // partial artifact/preview write never leaves a manifest claiming a
    // complete export.
    files.push(ExportedFile {
        path: "manifest.json".into(),
        bytes: serde_json::to_vec_pretty(&export.manifest)?,
    });
    Ok(files)
}
