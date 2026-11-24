//! Artifact classification for evidence exports.
//!
//! The recorded-history exporter treats every retained effect payload as
//! an artifact. Not every artifact may leave the installation: planner
//! output (implementation plans), internal decision records, and the
//! derived previews/manifests rendered from them are private working
//! material. This module names those classes and classifies an artifact
//! from its identity and bytes.
//!
//! Classification is deliberately conservative: a class is only assigned
//! from an explicit registration, a recognized artifact-id token, or an
//! explicit class field in the payload. Unclassified artifacts are
//! ordinary `evidence` and stay exportable, so the sanitized profiles can
//! exclude private material without guessing from prose.

use serde::{Deserialize, Serialize};

/// The class of one exportable artifact.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactClass {
    /// Ordinary retained evidence. Exportable in every profile.
    #[default]
    Evidence,
    /// Planner-authored implementation plans and planning notes.
    Plan,
    /// Internal decision documents and recorded decision rationale.
    Decision,
    /// Rendered previews derived from another artifact.
    Preview,
    /// Export manifests and integrity indexes.
    Manifest,
}

impl ArtifactClass {
    /// Every class, in stable order.
    pub const ALL: [ArtifactClass; 5] = [
        Self::Evidence,
        Self::Plan,
        Self::Decision,
        Self::Preview,
        Self::Manifest,
    ];

    /// The stable wire/file spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Evidence => "evidence",
            Self::Plan => "plan",
            Self::Decision => "decision",
            Self::Preview => "preview",
            Self::Manifest => "manifest",
        }
    }

    /// Parse a class name, accepting the documented aliases.
    pub fn parse(value: &str) -> Option<Self> {
        match value
            .trim()
            .to_ascii_lowercase()
            .replace(['_', ' '], "-")
            .as_str()
        {
            "evidence" | "artifact" | "data" | "source" => Some(Self::Evidence),
            "plan" | "plans" | "planning" | "implementation-plan" | "roadmap" => Some(Self::Plan),
            "decision" | "decisions" | "adr" | "internal-decision" | "rationale" => {
                Some(Self::Decision)
            }
            "preview" | "previews" | "render" | "renders" | "preview-render" => Some(Self::Preview),
            "manifest" | "manifests" | "index" | "integrity-index" => Some(Self::Manifest),
            _ => None,
        }
    }

    /// Whether a sanitized/shared export excludes this class by default.
    ///
    /// Only ordinary evidence is exportable by default; every
    /// planner-authored or derived class is private.
    pub fn excluded_by_default(self) -> bool {
        !matches!(self, Self::Evidence)
    }
}

impl std::fmt::Display for ArtifactClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

fn tokens(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
}

fn class_from_name(name: &str) -> Option<ArtifactClass> {
    let lower = name.to_ascii_lowercase();
    let mut saw_plan = false;
    let mut saw_decision = false;
    let mut saw_preview = false;
    let mut saw_manifest = false;
    for token in tokens(&lower) {
        match token {
            "plan" | "plans" | "planning" | "roadmap" => saw_plan = true,
            "decision" | "decisions" | "adr" | "rationale" => saw_decision = true,
            "preview" | "previews" | "render" | "renders" => saw_preview = true,
            "manifest" | "manifests" => saw_manifest = true,
            _ => {}
        }
    }
    if saw_manifest {
        Some(ArtifactClass::Manifest)
    } else if saw_preview {
        Some(ArtifactClass::Preview)
    } else if saw_plan {
        Some(ArtifactClass::Plan)
    } else if saw_decision {
        Some(ArtifactClass::Decision)
    } else {
        None
    }
}

/// Classify an artifact from its opaque id or path.
///
/// The first id segment (`plan:...`, `decision/...`) and the final path
/// stem are inspected for the recognized class tokens. An unknown id is
/// ordinary evidence.
pub fn classify_artifact_id(id: &str) -> ArtifactClass {
    let first = id.split([':', '/', '\\']).next().unwrap_or(id);
    if let Some(class) = class_from_name(first) {
        return class;
    }
    let stem = id
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(id)
        .split('.')
        .next()
        .unwrap_or(id);
    class_from_name(stem).unwrap_or_default()
}

fn value_class(value: &serde_json::Value) -> Option<ArtifactClass> {
    value.as_str().and_then(ArtifactClass::parse)
}

fn search_json(value: &serde_json::Value) -> Option<ArtifactClass> {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                let key = key.to_ascii_lowercase();
                if matches!(
                    key.as_str(),
                    "artifact_class" | "artifactclass" | "classification"
                ) && let Some(class) = value_class(value)
                {
                    return Some(class);
                }
                if matches!(key.as_str(), "class" | "type")
                    && let Some(class) = value_class(value)
                {
                    return Some(class);
                }
            }
            map.values().find_map(search_json)
        }
        serde_json::Value::Array(values) => values.iter().find_map(search_json),
        _ => None,
    }
}

/// Classify an artifact from its retained payload.
///
/// Only an explicit class field is honored; prose is never scanned. A
/// payload that is not JSON, or that carries no recognized class field,
/// is ordinary evidence.
pub fn classify_payload(payload: &[u8]) -> ArtifactClass {
    serde_json::from_slice::<serde_json::Value>(payload)
        .ok()
        .and_then(|value| search_json(&value))
        .unwrap_or_default()
}

/// Resolve an artifact's class: explicit registration wins, then the
/// artifact id, then an explicit payload class field.
pub fn classify_artifact(
    id: &str,
    payload: Option<&[u8]>,
    registered: Option<ArtifactClass>,
) -> ArtifactClass {
    if let Some(class) = registered {
        return class;
    }
    let from_id = classify_artifact_id(id);
    if from_id != ArtifactClass::Evidence {
        return from_id;
    }
    payload.map(classify_payload).unwrap_or_default()
}
