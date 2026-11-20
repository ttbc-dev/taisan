//! The `execution` module: consumes approved Invocation requests, supervises
//! real child processes via `ProcessControlPort`, collects artifacts, and
//! reports process/artifact Observations. It never decides lifecycle state —
//! that stays in `work`.

pub mod adapter;
pub mod bodies;
pub mod bounded;
pub mod containment;
pub mod handoff;
pub mod isolation;
pub mod request_v2;
pub mod scripted;
pub mod supervisor;
pub mod workspace;
