//! The remote executor reference endpoint (#377): one durable helper
//! plus its coordinator-side authenticated transport adapter.
//!
//! The helper serves the six fixed executor operations
//! (`prepare`/`start`/`observe`/`request_stop`/`collect`/`acknowledge`
//! from `docs/executor-transport-contract.md`) on an authenticated,
//! framed channel; it never accepts a shell string or an open-ended
//! RPC. Durable state — receipts, operation records, staged inputs,
//! retained output — lives in the endpoint's own state root in local
//! durable receipt files, so no separate daemon, database, or broker
//! is required beyond the helper itself (see
//! `docs/remote-executor-endpoint.md` for the necessity record).
//!
//! The Worker behind the endpoint is an independently configured,
//! conforming adapter: the helper observes its `describe` declaration
//! and launches `run --request <path> --out <dir>` through the same
//! v0.3 containment profile and negative preflight the local
//! supervisor uses, reusing `workloop-exec` components. The executor
//! selects nothing about the work — it executes one bound request at
//! a time under one execution slot.

pub mod adapter;
pub mod auth;
pub mod channel;
pub mod config;
pub mod runner;
pub mod service;
pub mod state;
