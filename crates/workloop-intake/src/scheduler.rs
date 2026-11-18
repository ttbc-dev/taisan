//! The intake scheduler's due decision (#215): a pure, clock-driven
//! state machine the driver ticks every 60 seconds independently of
//! the execution-loop cadence.
//!
//! The scheduler owns exactly one piece of process-local state —
//! `next_due` plus the last-applied settings signature — because the
//! durable truth lives in `intake_runs` and the settings record.
//! Restart re-derives the schedule: a fresh scheduler sees its first
//! tick as startup, so enabled intake is immediately due and disabled
//! intake is not, exactly as the contract specifies. No cron parser,
//! no calendar arithmetic, no catch-up queue: a due time observed
//! while intake is active is skipped, and `next_due` always advances
//! to the first grid point strictly after the tick — missed periods
//! collapse into the next future scheduled time, never a burst.
//!
//! Settings reload is the caller's job: each tick supplies the
//! *currently validated* settings (`None` when the profile failed to
//! load or validate this tick). An invalid tick suppresses the
//! decision entirely — the scheduler's state is untouched, so the
//! last valid settings keep governing and the next tick retries.

use workloop_config::profile::IntakeSettings;

use workloop_core::store::IntakeTrigger;

/// How often the driver evaluates intake state — the settings-check
/// tick, independent of `loop.interval_seconds` and of the intake
/// interval itself.
pub const SETTINGS_TICK_SECONDS: i64 = 60;

/// What one settings-check tick decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickDecision {
    /// No invocation is due this tick.
    Idle,
    /// An invocation is due now; the caller launches it outside any
    /// store transaction and outside the pass phases.
    Due { trigger: IntakeTrigger },
}

/// The last-applied settings signature the schedule is anchored to.
/// Only the fields that can move `next_due` participate: `enabled`
/// and `interval_minutes` directly, and the selector binding as the
/// "adapter" axis — a binding change preserves the due time while an
/// interval change re-anchors it. Timeout, queue limit, and task
/// defaults are schedule-invisible: they change the settings revision
/// recorded on runs but never when intake fires.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AppliedSettings {
    enabled: bool,
    interval_minutes: u64,
    /// The adapter-binding signature: adapter id, selector version,
    /// policy revision, and argv — the fields the contract names as
    /// "adapter-only" changes. `None` when no selector is bound.
    adapter_signature: Option<String>,
}

fn adapter_signature(settings: &IntakeSettings) -> Option<String> {
    settings.selector.as_ref().map(|binding| {
        // A canonical, unambiguous encoding: length-prefixed fields so
        // no pair of distinct bindings can collide on concatenation.
        let mut signature = String::new();
        for field in [
            binding.adapter.as_str(),
            &binding.selector_version.to_string(),
            &binding.policy_revision.to_string(),
        ]
        .into_iter()
        .chain(binding.argv.iter().map(String::as_str))
        {
            signature.push_str(&field.len().to_string());
            signature.push(':');
            signature.push_str(field);
            signature.push(';');
        }
        signature
    })
}

/// The process-local intake schedule. One per `taisan run` driver;
/// never persisted — restart re-derives it from the startup rule.
#[derive(Debug, Default)]
pub struct IntakeScheduler {
    applied: Option<AppliedSettings>,
    next_due_unix_seconds: Option<i64>,
}

impl IntakeScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    /// The next scheduled due time, for status reporting. `None` when
    /// intake is disabled or no grid point has been anchored yet.
    pub fn next_due_unix_seconds(&self) -> Option<i64> {
        self.next_due_unix_seconds
    }

    /// Evaluate one settings-check tick.
    ///
    /// - `now_unix_seconds`: the caller's clock reading.
    /// - `settings`: the validated intake settings for this tick, or
    ///   `None` when the profile could not be resolved — an invalid
    ///   tick suppresses the decision and leaves all state untouched.
    /// - `intake_active`: a durable `open`/`admitting` run exists or
    ///   an invocation is in flight — a due time observed now is
    ///   skipped, never queued.
    ///
    /// Returns the tick's decision; [`Self::next_due_unix_seconds`]
    /// always reflects the post-tick grid.
    pub fn tick(
        &mut self,
        now_unix_seconds: i64,
        settings: Option<&IntakeSettings>,
        intake_active: bool,
    ) -> TickDecision {
        let Some(settings) = settings else {
            // Invalid or absent settings this tick: suppress new
            // intake, keep the last applied state, retry next tick.
            return TickDecision::Idle;
        };
        let current = AppliedSettings {
            enabled: settings.enabled,
            interval_minutes: settings.interval_minutes,
            adapter_signature: adapter_signature(settings),
        };
        let interval_seconds = i64::try_from(settings.interval_minutes)
            .unwrap_or(i64::MAX)
            .saturating_mul(60);

        let first_sight = self.applied.is_none();
        let was_enabled = self.applied.as_ref().is_some_and(|p| p.enabled);
        let interval_changed = self
            .applied
            .as_ref()
            .is_some_and(|p| p.interval_minutes != settings.interval_minutes);
        let adapter_changed = self
            .applied
            .as_ref()
            .is_some_and(|p| p.adapter_signature != current.adapter_signature);
        // `adapter_changed` is computed for clarity of the transition
        // table; both it and every non-interval change preserve
        // `next_due`, so only `interval_changed` re-anchors below.
        let _ = adapter_changed;

        if !settings.enabled {
            self.applied = Some(current);
            self.next_due_unix_seconds = None;
            return TickDecision::Idle;
        }

        // Startup and re-enable are immediately due; an interval change
        // re-anchors the grid to this tick plus the new interval (the
        // pending old-grid slot is replaced, never fired); every other
        // transition preserves `next_due`.
        if first_sight {
            self.next_due_unix_seconds = Some(now_unix_seconds.saturating_add(interval_seconds));
            self.applied = Some(current);
            return TickDecision::Due {
                trigger: IntakeTrigger::ScheduledStartup,
            };
        }
        if !was_enabled {
            self.next_due_unix_seconds = Some(now_unix_seconds.saturating_add(interval_seconds));
            self.applied = Some(current);
            return TickDecision::Due {
                trigger: IntakeTrigger::ScheduledReenable,
            };
        }
        if interval_changed {
            self.next_due_unix_seconds = Some(now_unix_seconds.saturating_add(interval_seconds));
            self.applied = Some(current);
            return TickDecision::Idle;
        }

        self.applied = Some(current);
        let Some(next_due) = self.next_due_unix_seconds else {
            // Enabled with no anchored grid — unreachable through the
            // transitions above, but fail toward the startup rule
            // rather than scheduling nothing forever.
            self.next_due_unix_seconds = Some(now_unix_seconds.saturating_add(interval_seconds));
            return TickDecision::Due {
                trigger: IntakeTrigger::ScheduledStartup,
            };
        };
        if next_due > now_unix_seconds {
            return TickDecision::Idle;
        }

        // A due time is here. Advance the grid past this tick first —
        // whether the invocation fires or is skipped for an active
        // run, the consumed slot never produces a catch-up event.
        self.next_due_unix_seconds =
            Some(advance_past(next_due, now_unix_seconds, interval_seconds));
        if intake_active {
            return TickDecision::Idle;
        }
        TickDecision::Due {
            trigger: IntakeTrigger::ScheduledInterval,
        }
    }
}

/// The first grid point strictly after `now`, stepping the anchored
/// `next_due` forward by whole intervals — arithmetic, not a loop, so
/// arbitrarily many missed periods cost one division.
fn advance_past(next_due: i64, now: i64, interval_seconds: i64) -> i64 {
    if interval_seconds <= 0 {
        return now.saturating_add(1);
    }
    if next_due > now {
        return next_due;
    }
    let missed = (now - next_due) / interval_seconds + 1;
    next_due.saturating_add(missed.saturating_mul(interval_seconds))
}
