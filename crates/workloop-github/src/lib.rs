//! GitHub integration behind the adapter port. Slice 0 ships a fixture that
//! records `PrActionIntent`s and emits synthetic Observations; the real
//! adapter runs the same contract tests against GitHub.

pub mod fixture;
pub mod sparse_fixture;
