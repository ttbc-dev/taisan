//! Scheduler contract tests for intake (#215): the pure due-decision
//! state machine — startup, disable/enable, interval re-anchoring,
//! adapter-binding changes, due-time skips while intake is active,
//! missed-period collapse, and invalid-settings suppression.

use workloop_config::profile::{IntakeSelectorBinding, IntakeSettings};
use workloop_core::store::IntakeTrigger;
use workloop_intake::scheduler::{IntakeScheduler, SETTINGS_TICK_SECONDS, TickDecision};

const NOW: i64 = 1_700_000_000;

fn settings(enabled: bool, interval_minutes: u64) -> IntakeSettings {
    IntakeSettings {
        enabled,
        interval_minutes,
        timeout_seconds: 120,
        pending_queue_limit: 10,
        selector: Some(IntakeSelectorBinding {
            adapter: "test-sel".into(),
            selector_version: 1,
            policy_revision: 7,
            argv: vec!["/bin/true".into()],
            task_constraints: vec![],
            verification_argv: vec![],
            required_permissions: vec![],
        }),
    }
}

fn due(decision: TickDecision) -> IntakeTrigger {
    match decision {
        TickDecision::Due { trigger } => trigger,
        other => panic!("expected Due, got {other:?}"),
    }
}

#[test]
fn the_settings_tick_is_sixty_seconds() {
    assert_eq!(SETTINGS_TICK_SECONDS, 60);
}

#[test]
fn startup_is_immediately_due_and_anchors_the_grid() {
    let mut sched = IntakeScheduler::new();
    // 30-minute interval → first grid point 1800s out.
    let trigger = due(sched.tick(NOW, Some(&settings(true, 30)), false));
    assert_eq!(trigger, IntakeTrigger::ScheduledStartup);
    assert_eq!(sched.next_due_unix_seconds(), Some(NOW + 1800));
}

#[test]
fn disabled_intake_never_fires() {
    let mut sched = IntakeScheduler::new();
    assert_eq!(
        sched.tick(NOW, Some(&settings(false, 30)), false),
        TickDecision::Idle
    );
    assert_eq!(sched.next_due_unix_seconds(), None);
    // Far-future ticks stay idle while disabled.
    assert_eq!(
        sched.tick(NOW + 86_400, Some(&settings(false, 30)), false),
        TickDecision::Idle
    );
    assert_eq!(sched.next_due_unix_seconds(), None);
}

#[test]
fn enable_after_disabled_is_a_reenable_not_a_startup() {
    let mut sched = IntakeScheduler::new();
    sched.tick(NOW, Some(&settings(false, 30)), false);
    let trigger = due(sched.tick(NOW + 60, Some(&settings(true, 30)), false));
    assert_eq!(trigger, IntakeTrigger::ScheduledReenable);
    assert_eq!(sched.next_due_unix_seconds(), Some(NOW + 60 + 1800));
}

#[test]
fn the_interval_grid_fires_once_per_due_point() {
    let mut sched = IntakeScheduler::new();
    sched.tick(NOW, Some(&settings(true, 30)), false);
    let due_at = NOW + 1800;

    // Before the grid point: idle.
    assert_eq!(
        sched.tick(due_at - 1, Some(&settings(true, 30)), false),
        TickDecision::Idle
    );
    // Exactly at the grid point: due, interval trigger.
    let trigger = due(sched.tick(due_at, Some(&settings(true, 30)), false));
    assert_eq!(trigger, IntakeTrigger::ScheduledInterval);
    assert_eq!(sched.next_due_unix_seconds(), Some(due_at + 1800));
    // Just after: idle again — one decision per slot, no burst.
    assert_eq!(
        sched.tick(due_at + 1, Some(&settings(true, 30)), false),
        TickDecision::Idle
    );
}

#[test]
fn a_due_slot_observed_while_active_is_skipped_not_queued() {
    let mut sched = IntakeScheduler::new();
    sched.tick(NOW, Some(&settings(true, 30)), false);
    let due_at = NOW + 1800;
    // Intake is active at the due instant: the slot is consumed and
    // the grid still advances — no catch-up invocation later.
    assert_eq!(
        sched.tick(due_at, Some(&settings(true, 30)), true),
        TickDecision::Idle
    );
    assert_eq!(sched.next_due_unix_seconds(), Some(due_at + 1800));
    // The skipped slot never fires retroactively.
    assert_eq!(
        sched.tick(due_at + 60, Some(&settings(true, 30)), false),
        TickDecision::Idle
    );
}

#[test]
fn startup_and_reenable_dues_observed_while_active_are_skipped() {
    // First sight with an already-active run: the startup due is
    // skipped like any other due — the grid still anchors so the
    // next firing is one interval out, never a retry burst.
    let mut sched = IntakeScheduler::new();
    assert_eq!(
        sched.tick(NOW, Some(&settings(true, 30)), true),
        TickDecision::Idle
    );
    assert_eq!(
        sched.next_due_unix_seconds(),
        Some(NOW + 1800),
        "the skipped startup due still anchors the interval grid"
    );

    // Disabled -> enabled while a run is active: the re-enable due is
    // skipped the same way.
    let mut sched = IntakeScheduler::new();
    sched.tick(NOW, Some(&settings(false, 30)), false);
    assert_eq!(
        sched.tick(NOW + 60, Some(&settings(true, 30)), true),
        TickDecision::Idle
    );
    assert_eq!(sched.next_due_unix_seconds(), Some(NOW + 60 + 1800));
}

#[test]
fn missed_periods_collapse_to_the_next_future_grid_point() {
    let mut sched = IntakeScheduler::new();
    sched.tick(NOW, Some(&settings(true, 30)), false);
    // Sleep through five grid points: 1800,3600,5400,7200,9000 — the
    // next tick at NOW+10000 sees one due decision, not five.
    let late = NOW + 10_000;
    let trigger = due(sched.tick(late, Some(&settings(true, 30)), false));
    assert_eq!(trigger, IntakeTrigger::ScheduledInterval);
    // Grid: 1800·k > 10000 → 10800.
    assert_eq!(sched.next_due_unix_seconds(), Some(NOW + 10_800));
}

#[test]
fn an_invalid_tick_suppresses_the_decision_and_preserves_state() {
    let mut sched = IntakeScheduler::new();
    sched.tick(NOW, Some(&settings(true, 30)), false);
    let due_at = NOW + 1800;
    // A tick with unresolvable settings: suppressed, state untouched.
    assert_eq!(sched.tick(due_at, None, false), TickDecision::Idle);
    assert_eq!(sched.next_due_unix_seconds(), Some(due_at));
    // The very next valid tick still sees the same due point.
    let trigger = due(sched.tick(due_at + 1, Some(&settings(true, 30)), false));
    assert_eq!(trigger, IntakeTrigger::ScheduledInterval);
}

#[test]
fn an_interval_change_reanchors_without_firing() {
    let mut sched = IntakeScheduler::new();
    sched.tick(NOW, Some(&settings(true, 30)), false);
    // Mid-interval, the operator changes 30 → 60 minutes: the pending
    // slot is replaced by now+3600, never fired.
    let retimed = settings(true, 60);
    assert_eq!(
        sched.tick(NOW + 600, Some(&retimed), false),
        TickDecision::Idle
    );
    assert_eq!(sched.next_due_unix_seconds(), Some(NOW + 600 + 3600));
    // The old grid point (NOW+1800) is gone: a tick there is idle.
    assert_eq!(
        sched.tick(NOW + 1800, Some(&retimed), false),
        TickDecision::Idle
    );
}

#[test]
fn adapter_and_policy_changes_preserve_the_due_time() {
    let mut sched = IntakeScheduler::new();
    sched.tick(NOW, Some(&settings(true, 30)), false);
    let due_at = NOW + 1800;

    // A different argv is an adapter-binding change: schedule-
    // invisible, `next_due` preserved.
    let mut rebound = settings(true, 30);
    rebound.selector.as_mut().unwrap().argv = vec!["/bin/other-sel".into()];
    assert_eq!(
        sched.tick(NOW + 300, Some(&rebound), false),
        TickDecision::Idle
    );
    assert_eq!(sched.next_due_unix_seconds(), Some(due_at));

    // Queue-limit and timeout changes are likewise invisible.
    let mut resized = rebound.clone();
    resized.pending_queue_limit = 3;
    resized.timeout_seconds = 5;
    assert_eq!(
        sched.tick(NOW + 600, Some(&resized), false),
        TickDecision::Idle
    );
    assert_eq!(sched.next_due_unix_seconds(), Some(due_at));

    // The preserved grid still fires on schedule.
    let trigger = due(sched.tick(due_at, Some(&resized), false));
    assert_eq!(trigger, IntakeTrigger::ScheduledInterval);
}

#[test]
fn disable_clears_the_grid_and_reenable_restarts_it() {
    let mut sched = IntakeScheduler::new();
    sched.tick(NOW, Some(&settings(true, 30)), false);
    // Disable mid-interval: grid cleared.
    assert_eq!(
        sched.tick(NOW + 900, Some(&settings(false, 30)), false),
        TickDecision::Idle
    );
    assert_eq!(sched.next_due_unix_seconds(), None);
    // Re-enable much later: immediately due as a reenable, grid
    // re-anchored from that tick — the stale slot never fires.
    let later = NOW + 5000;
    let trigger = due(sched.tick(later, Some(&settings(true, 30)), false));
    assert_eq!(trigger, IntakeTrigger::ScheduledReenable);
    assert_eq!(sched.next_due_unix_seconds(), Some(later + 1800));
}
