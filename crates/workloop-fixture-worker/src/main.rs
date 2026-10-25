//! Fixture worker: a real process the contract tests spawn. It writes to
//! the managed workspace and reports evidence on request. Behavior is
//! driven by argv so tests can model clean exits, hangs, setsid escapes,
//! and zombie children.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    eprintln!("fixture-worker: unimplemented mode {args:?}");
}
