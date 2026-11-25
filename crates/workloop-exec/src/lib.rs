//! The `execution` module: consumes approved Invocation requests, supervises
//! real child processes via `ProcessControlPort`, collects artifacts, and
//! reports process/artifact Observations. It never decides lifecycle state —
//! that stays in `work`.

pub mod adapter;
pub mod assessment_v2;
pub mod bindings;
pub mod bodies;
pub mod boundary;
pub mod bounded;
pub mod containment;
pub mod credentials;
pub mod egress;
pub mod handoff;
pub mod isolation;
pub mod remote_collect;
pub mod remote_custody;
pub mod remote_export;
pub mod remote_prepare;
pub mod remote_transfer;
pub mod report_v2;
pub mod request_v2;
pub mod scripted;
pub mod supervisor;
pub mod workspace;
