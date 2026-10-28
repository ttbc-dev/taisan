//! `workloop-tui` — the management console, the second interface adapter
//! after the CLI. It reaches `work` only through `preview` and
//! `submit(CommandRequest)` and reaches `projection` only through the
//! Query functions — the same contract surface as the CLI, with no
//! TUI-only commands or queries and no separate state model. The board
//! is a projection of saved Work/Execution records; the only state the
//! console owns is selection: the open Work, the in-progress judgment
//! draft, the staged request, and an issued-but-unresolved one.

pub mod board;
pub mod install;
pub mod judgment;
pub mod render;
pub mod session;
