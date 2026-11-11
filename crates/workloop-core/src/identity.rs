//! Neutral scoped identities for hosts, repositories, revisions, and
//! actors. These types carry a validated opaque value plus the scope
//! fields needed to compare it; they never encode structure by string
//! concatenation, and they never expose provider-specific semantics.
//!
//! Identity rules (docs/identity-storage.md):
//! - Repository identity is `(host_instance, repository_id)`. The
//!   `repository_id` is a stable opaque id supplied by the adapter's
//!   installation configuration, never a display path.
//! - Issue / change-request identity is repository-scoped with an
//!   opaque native id; display numbers are metadata.
//! - Commit revisions are repository-scoped exact object ids.
//! - Host actor identity is `(host_instance, stable_actor_id)` and
//!   differs from the local Command actor.
//! - Host instance identity is distinct from a display alias and must
//!   not be reused after endpoint retargeting.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;

const MAX_ID_LEN: usize = 512;

fn check_opaque(kind: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{kind}: must not be empty"));
    }
    if value.len() > MAX_ID_LEN {
        return Err(format!("{kind}: exceeds {MAX_ID_LEN} characters"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{kind}: contains control characters"));
    }
    Ok(())
}

macro_rules! opaque_id {
    ($name:ident, $kind:literal) => {
        /// Opaque validated identifier. The value is validated at
        /// construction and deserialization; an empty, oversized, or
        /// control-character value is rejected.
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, String> {
                let value = value.into();
                check_opaque($kind, &value)?;
                Ok(Self(value))
            }
            /// Construct from a value already validated by the caller.
            /// Used by serde and trusted internal callers.
            pub fn trusted(value: impl Into<String>) -> Self {
                Self(value.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let v = String::deserialize(d)?;
                Self::new(v).map_err(serde::de::Error::custom)
            }
        }
    };
}

opaque_id!(StoreId, "store_id");
opaque_id!(HostInstanceId, "host_instance_id");
opaque_id!(RepositoryId, "repository_id");
opaque_id!(IssueId, "issue_id");
opaque_id!(ChangeRequestId, "change_request_id");
opaque_id!(HostActorId, "host_actor_id");
opaque_id!(AdapterId, "adapter_id");
// The only credential shape Core holds: an opaque reference naming a
// secret slot, never the secret value. The `kind:name` provider form
// is adapter business — resolution happens at the adapter boundary
// (`credential` module), so Core stores the reference unparsed.
opaque_id!(CredentialRef, "credential_ref");

/// Repository identity `(host_instance, repository_id)`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RepositoryRef {
    pub host: HostInstanceId,
    pub repository: RepositoryId,
}

impl RepositoryRef {
    pub fn new(host: HostInstanceId, repository: RepositoryId) -> Self {
        Self { host, repository }
    }
}

impl fmt::Display for RepositoryRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.host, self.repository)
    }
}

/// Issue identity inside one repository. `display_number` is
/// presentation metadata and takes no part in identity comparison.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueRef {
    pub repository: RepositoryRef,
    pub issue: IssueId,
    #[serde(default)]
    pub display_number: Option<u64>,
}

// Identity, ordering, and bucketing all ignore `display_number`: the
// same issue observed with a renumbered or absent display number is the
// same issue.
impl PartialEq for IssueRef {
    fn eq(&self, other: &Self) -> bool {
        self.repository == other.repository && self.issue == other.issue
    }
}
impl Eq for IssueRef {}
impl PartialOrd for IssueRef {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for IssueRef {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.repository, &self.issue).cmp(&(&other.repository, &other.issue))
    }
}
impl std::hash::Hash for IssueRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.repository.hash(state);
        self.issue.hash(state);
    }
}

impl fmt::Display for IssueRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.repository, self.issue)
    }
}

/// Change-request identity inside one repository. `display_number` is
/// presentation metadata and takes no part in identity comparison.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeRequestRef {
    pub repository: RepositoryRef,
    pub change_request: ChangeRequestId,
    #[serde(default)]
    pub display_number: Option<u64>,
}

impl PartialEq for ChangeRequestRef {
    fn eq(&self, other: &Self) -> bool {
        self.repository == other.repository && self.change_request == other.change_request
    }
}
impl Eq for ChangeRequestRef {}
impl PartialOrd for ChangeRequestRef {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for ChangeRequestRef {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.repository, &self.change_request).cmp(&(&other.repository, &other.change_request))
    }
}
impl std::hash::Hash for ChangeRequestRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.repository.hash(state);
        self.change_request.hash(state);
    }
}

impl fmt::Display for ChangeRequestRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.repository, self.change_request)
    }
}

/// An exact commit revision inside one repository.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CommitRevision {
    pub repository: RepositoryRef,
    pub object: String,
}

impl CommitRevision {
    pub fn new(repository: RepositoryRef, object: impl Into<String>) -> Result<Self, String> {
        let object = object.into();
        check_opaque("commit object", &object)?;
        Ok(Self { repository, object })
    }
}

impl fmt::Display for CommitRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.repository, self.object)
    }
}

/// A host-side actor `(host_instance, stable_actor_id)`. Distinct from
/// the local `Actor` that submits Commands.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HostActorRef {
    pub host: HostInstanceId,
    pub actor: HostActorId,
}

/// Host capabilities a Work may require. An absent or denied capability
/// never authorizes an effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostCapability {
    RepositoryRead,
    RepositoryReadMetadata,
    PushRef,
    PullRead,
    BranchRead,
    ChangeRequestCreate,
    ChangeRequestUpdate,
    ChangeRequestRead,
    ChangeRequestList,
    ReviewRead,
    ReviewFindingsRead,
    /// Ordinary change-request comment write. The owner documents name
    /// this `comment_write` (repository/host contract baseline); the
    /// legacy `comment_create` spelling stays accepted on input.
    #[serde(alias = "comment_create")]
    CommentWrite,
    CommentRead,
    ThreadResolve,
    IssueRead,
    IssueList,
    ChecksRead,
    ChecksWrite,
    InlineComment,
    ReviewFeedback,
    Approve,
    RequestChanges,
    ThreadReply,
    /// Non-draft/ready state change. The owner documents spell this
    /// both `mark_ready` and `ready`; both stay accepted on input.
    #[serde(alias = "ready")]
    MarkReady,
}

impl HostCapability {
    pub fn name(&self) -> &'static str {
        match self {
            Self::RepositoryRead => "repository_read",
            Self::RepositoryReadMetadata => "repository_read_metadata",
            Self::PushRef => "push_ref",
            Self::PullRead => "pull_read",
            Self::BranchRead => "branch_read",
            Self::ChangeRequestCreate => "change_request_create",
            Self::ChangeRequestUpdate => "change_request_update",
            Self::ChangeRequestRead => "change_request_read",
            Self::ChangeRequestList => "change_request_list",
            Self::ReviewRead => "review_read",
            Self::ReviewFindingsRead => "review_findings_read",
            Self::CommentWrite => "comment_write",
            Self::CommentRead => "comment_read",
            Self::ThreadResolve => "thread_resolve",
            Self::IssueRead => "issue_read",
            Self::IssueList => "issue_list",
            Self::ChecksRead => "checks_read",
            Self::ChecksWrite => "checks_write",
            Self::InlineComment => "inline_comment",
            Self::ReviewFeedback => "review_feedback",
            Self::Approve => "approve",
            Self::RequestChanges => "request_changes",
            Self::ThreadReply => "thread_reply",
            Self::MarkReady => "mark_ready",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "repository_read" => Some(Self::RepositoryRead),
            "repository_read_metadata" => Some(Self::RepositoryReadMetadata),
            "push_ref" => Some(Self::PushRef),
            "pull_read" => Some(Self::PullRead),
            "branch_read" => Some(Self::BranchRead),
            "change_request_create" => Some(Self::ChangeRequestCreate),
            "change_request_update" => Some(Self::ChangeRequestUpdate),
            "change_request_read" => Some(Self::ChangeRequestRead),
            "change_request_list" => Some(Self::ChangeRequestList),
            "review_read" => Some(Self::ReviewRead),
            "review_findings_read" => Some(Self::ReviewFindingsRead),
            "comment_write" | "comment_create" => Some(Self::CommentWrite),
            "comment_read" => Some(Self::CommentRead),
            "thread_resolve" => Some(Self::ThreadResolve),
            "issue_read" => Some(Self::IssueRead),
            "issue_list" => Some(Self::IssueList),
            "checks_read" => Some(Self::ChecksRead),
            "checks_write" => Some(Self::ChecksWrite),
            "inline_comment" => Some(Self::InlineComment),
            "review_feedback" => Some(Self::ReviewFeedback),
            "approve" => Some(Self::Approve),
            "request_changes" => Some(Self::RequestChanges),
            "thread_reply" => Some(Self::ThreadReply),
            "mark_ready" | "ready" => Some(Self::MarkReady),
            _ => None,
        }
    }
}

/// Host-side actor role used by a Work binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorRole {
    Publication,
    Review,
    Discussion,
}

impl ActorRole {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Publication => "publication",
            Self::Review => "review",
            Self::Discussion => "discussion",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "publication" => Some(Self::Publication),
            "review" => Some(Self::Review),
            "discussion" => Some(Self::Discussion),
            _ => None,
        }
    }
}

/// The role → actor → credential-source assignment recorded at
/// acceptance. `credential_ref` names a configured credential source;
/// it never carries a secret value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleBinding {
    pub role: ActorRole,
    pub actor: HostActorRef,
    pub credential_ref: CredentialRef,
}

/// The immutable host binding recorded on Work acceptance. Identities
/// here are already resolved — a later alias or endpoint change cannot
/// retarget this binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostBinding {
    pub adapter: AdapterId,
    pub repository: RepositoryRef,
    pub contract_version: u32,
    /// The capability set this Work requires, recorded at acceptance.
    /// Sorted and deduplicated by `HostBinding::new`.
    pub required_capabilities: BTreeSet<HostCapability>,
    /// Exactly one binding per role.
    pub actors: Vec<RoleBinding>,
}

impl HostBinding {
    pub fn new(
        adapter: AdapterId,
        repository: RepositoryRef,
        contract_version: u32,
        required_capabilities: impl IntoIterator<Item = HostCapability>,
        actors: Vec<RoleBinding>,
    ) -> Result<Self, String> {
        if contract_version == 0 {
            return Err("host binding: contract_version must be greater than 0".into());
        }
        let required_capabilities: BTreeSet<HostCapability> =
            required_capabilities.into_iter().collect();
        let roles: BTreeSet<ActorRole> = actors.iter().map(|a| a.role).collect();
        if roles.len() != actors.len() {
            return Err("host binding: duplicate actor role".into());
        }
        if actors.is_empty() {
            return Err("host binding: no actor roles".into());
        }
        for a in &actors {
            check_opaque("credential_ref", a.credential_ref.as_str())?;
            if a.actor.host != repository.host {
                return Err(format!(
                    "host binding: actor {} is not on host {}",
                    a.actor.actor, repository.host
                ));
            }
        }
        Ok(Self {
            adapter,
            repository,
            contract_version,
            required_capabilities,
            actors,
        })
    }

    pub fn actor_for(&self, role: ActorRole) -> Option<&HostActorRef> {
        self.actors
            .iter()
            .find(|a| a.role == role)
            .map(|a| &a.actor)
    }
}

/// A typed evidence reference: the record id plus the digest that
/// pins it to an exact payload.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub id: String,
    pub digest: String,
}

/// The exact review/check target: the change request, its head, the
/// comparison base, and the target-branch revision. Two assessments
/// are only comparable on an identical `DiffTarget`; movement in any
/// field — including target-branch-only movement — invalidates prior
/// judgments.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DiffTarget {
    pub change: ChangeRequestRef,
    pub head: CommitRevision,
    #[serde(default)]
    pub base: Option<CommitRevision>,
    #[serde(default)]
    pub target_revision: Option<CommitRevision>,
}

/// An immutable diff snapshot: the snapshot is exactly its
/// `(change, base, head)` triple. The identity is derived on demand
/// rather than stored, and the fields are private — the record carries
/// no mutation path, so two reads of the same pair always return the
/// identical snapshot identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DiffSnapshot {
    change: ChangeRequestRef,
    base: CommitRevision,
    head: CommitRevision,
}

impl DiffSnapshot {
    pub fn new(change: ChangeRequestRef, base: CommitRevision, head: CommitRevision) -> Self {
        Self { change, base, head }
    }

    pub fn change(&self) -> &ChangeRequestRef {
        &self.change
    }

    pub fn base(&self) -> &CommitRevision {
        &self.base
    }

    pub fn head(&self) -> &CommitRevision {
        &self.head
    }

    /// The snapshot's stable identity: a digest over the exact triple,
    /// computed from the immutable fields — never a stored, driftable
    /// value.
    pub fn snapshot_id(&self) -> String {
        crate::domain::sha256_hex(
            "diff-snapshot-v1",
            &crate::domain::canonical_json(&serde_json::json!({
                "change": self.change,
                "base": self.base,
                "head": self.head,
            })),
        )
    }
}

/// The immutable snapshot of one source issue's authoritative read,
/// recorded inside a spec revision. A `source_revision` is a
/// host-supplied version token or content digest — never a timestamp.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SourceSnapshot {
    pub issue: IssueRef,
    pub source_revision: String,
    pub content_digest: String,
}

/// The source-gate state of a Work. Anything other than `Matches` or
/// `NotApplicable` blocks admission until the recorded prerequisite is
/// satisfied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceGate {
    /// The Work declares no source issues.
    NotApplicable,
    /// Source issues are declared; no authoritative read has matched
    /// the spec snapshot yet.
    NeedsCheck,
    /// The latest authoritative reads match the approved spec snapshot.
    Matches,
    /// A read showed changed or closed source content; a new spec
    /// revision and reapproval are required.
    Changed,
    /// The source could not be read; this is a hold, not proof of
    /// unchanged content.
    Unavailable,
}

impl SourceGate {
    pub fn name(&self) -> &'static str {
        match self {
            Self::NotApplicable => "not_applicable",
            Self::NeedsCheck => "needs_check",
            Self::Matches => "matches",
            Self::Changed => "changed",
            Self::Unavailable => "unavailable",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "not_applicable" => Some(Self::NotApplicable),
            "needs_check" => Some(Self::NeedsCheck),
            "matches" => Some(Self::Matches),
            "changed" => Some(Self::Changed),
            "unavailable" => Some(Self::Unavailable),
            _ => None,
        }
    }

    /// The stricter of two gate states. Ordering: matches < needs_check
    /// < unavailable < changed (not_applicable is never produced by a
    /// read-driven update).
    pub fn min_gate(self, other: Self) -> Self {
        fn rank(g: SourceGate) -> u8 {
            match g {
                SourceGate::NotApplicable | SourceGate::Matches => 0,
                SourceGate::NeedsCheck => 1,
                SourceGate::Unavailable => 2,
                SourceGate::Changed => 3,
            }
        }
        if rank(self) >= rank(other) {
            self
        } else {
            other
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Identity, ordering, and bucketing must not split on display
    /// metadata: a renumbered or absent display number still names the
    /// same issue and the same change request.
    #[test]
    fn identity_refs_ignore_the_display_number() {
        let repo = RepositoryRef::new(HostInstanceId::trusted("h"), RepositoryId::trusted("r"));
        let a = IssueRef {
            repository: repo.clone(),
            issue: IssueId::trusted("issue-1"),
            display_number: Some(5),
        };
        let b = IssueRef {
            repository: repo.clone(),
            issue: IssueId::trusted("issue-1"),
            display_number: None,
        };
        assert_eq!(a, b);
        assert_eq!(a.cmp(&b), std::cmp::Ordering::Equal);
        let mut set = std::collections::HashSet::new();
        set.insert(a.clone());
        set.insert(b);
        assert_eq!(set.len(), 1);

        let c1 = ChangeRequestRef {
            repository: repo.clone(),
            change_request: ChangeRequestId::trusted("cr-1"),
            display_number: Some(7),
        };
        let c2 = ChangeRequestRef {
            repository: repo,
            change_request: ChangeRequestId::trusted("cr-1"),
            display_number: Some(9),
        };
        assert_eq!(c1, c2);
        assert_eq!(c1.cmp(&c2), std::cmp::Ordering::Equal);
    }

    /// A versionless host binding is never representable: version 0 is
    /// not a negotiated contract.
    #[test]
    fn host_binding_rejects_version_zero() {
        let repository =
            RepositoryRef::new(HostInstanceId::trusted("h"), RepositoryId::trusted("r"));
        let actors = vec![RoleBinding {
            role: ActorRole::Publication,
            actor: HostActorRef {
                host: HostInstanceId::trusted("h"),
                actor: HostActorId::trusted("a"),
            },
            credential_ref: CredentialRef::trusted("cred"),
        }];
        assert!(
            HostBinding::new(
                AdapterId::trusted("fixture"),
                repository.clone(),
                0,
                [],
                actors.clone()
            )
            .is_err()
        );
        assert!(HostBinding::new(AdapterId::trusted("fixture"), repository, 1, [], actors).is_ok());
    }
}
