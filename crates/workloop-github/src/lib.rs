//! GitHub integration behind the adapter port. Slice 0 ships a fixture
//! that records `HostActionIntent`s and emits synthetic Observations;
//! the real adapter ([`adapter::GitHubAdapter`]) runs the same port and
//! the same contract tests against GitHub through the
//! [`api::GitHubApi`] transport seam.

pub mod adapter;
pub mod api;
pub mod credential;
pub mod evidence;
pub mod fixture;
pub mod gitlab_fixture;
pub mod gitlab_spike;
pub mod port;
pub mod sparse_fixture;
