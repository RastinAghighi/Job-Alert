//! `next_check_at` — jitter, priority probes, backoff and `Retry-After` (§11.2).
//!
//! This module answers one question, *when is this source due again?*, as a pure
//! function of values the caller already holds. It is the whole of §11.2's
//! rescheduling table and nothing else.
//!
//! # Counter ownership — the reason this module reads two snapshots
//!
//! `core::health` is the only Phase-1 module permitted to mutate
//! `consecutive_failures`, `probe_attempts` and `first_failure_at` (§8.1). Nothing
//! here increments, resets or returns a counter; [`ScheduleInput`] carries a
//! **pre-poll** view and a **post-health** view side by side because §10.3 keys the
//! two decisions off different instants:
//!
//! - **probe eligibility** tests [`ScheduleInput::state_before`] and
//!   [`ScheduleInput::probe_attempts_before`], the values as they stood *before*
//!   this poll was classified;
//! - **backoff** reads [`ScheduleInput::state_after`] and
//!   [`ScheduleInput::consecutive_failures_after`], the values `core::health` has
//!   already written for this poll.
//!
//! Collapsing them to one snapshot is what made §8.1 and §11.2 disagree at v1.1.
//! The second transient of a run is the case that proves it: its pre-state is
//! `DEGRADED` with one probe spent, so it is probe #2 — while `core::health` has
//! *already* moved the source to `FAILED` in the same poll. Read post-state alone
//! and that probe silently becomes a backoff; read pre-state alone and an
//! already-`FAILED` source starts a fresh two-probe sequence against an upstream
//! that is known to be down.
//!
//! # Randomness lives in the engine, not here
//!
//! §11.2 draws jitter from `uniform(0, min(0.10 × effective, 30 s))`. The *draw* is
//! the `Jitter` port's (§17.1); this module publishes the bound as [`max_jitter`]
//! and clamps whatever it is handed to that bound, so the invariant holds even if a
//! caller passes something wild. That keeps D11's core deterministic: every test
//! below fixes `now` and `jitter` and asserts an exact instant.
//!
//! Jitter is not evasion. At 288 requests/day/source nothing about the traffic is
//! remarkable. It exists so that thirty sources configured at 5 minutes do not
//! align on the same tick and the same top-of-minute forever.
//!
//! # Arithmetic never wraps
//!
//! `consecutive_failures` legitimately reaches 20 before quarantine (§8.1), and
//! `2^18 × 1800 s` overflows a naive seconds computation. Every multiplication here
//! is checked and capped at the two-hour ceiling *before* the conversion to
//! [`TimeDelta`], and the final `now + delay` saturates to
//! `DateTime::<Utc>::MAX_UTC` rather than panicking on an absurd input. Saturating
//! is always toward *later*, never toward a past instant that would make a source
//! due immediately.

use crate::model::{Criticality, HealthState, PollOutcome};
use chrono::{DateTime, TimeDelta, Utc};
use jobmon_errors::{FailureKind, FaultDomain, PipelineError, Stage};
use std::time::Duration;

/// The priority-probe delay (§10.3).
///
/// An internal value, not a published promise. EventBridge Scheduler has
/// minute-level resolution, so 30 s exists to guarantee the re-poll lands on the
/// **next** tick rather than the current one; the SLA it buys is "confirmation
/// within 1–2 ticks, typically ≤ 60 s, worst case ≈ 90 s".
const PROBE_DELAY: Duration = Duration::from_secs(30);

/// How many priority probes one transient run gets before it drops to backoff
/// (§10.3). Compared against the **pre-poll** `probe_attempts`.
const MAX_PROBE_ATTEMPTS: u32 = 2;

/// The ceiling on jitter, whatever 10 % of the interval comes to (§11.2).
const JITTER_CAP: Duration = Duration::from_secs(30);

/// The cap on exponential backoff (§8, §10.4). A source stuck for hours is not
/// worth polling more often than this, and the cap is what keeps the exponent
/// harmless at `cf = 20`.
const MAX_BACKOFF: Duration = Duration::from_secs(2 * 60 * 60);

/// The floor under a 429's `Retry-After` (§10.4). A rate limiter that asks to be
/// retried in 10 s is still asking us to hammer it.
const RATE_LIMIT_FLOOR: Duration = Duration::from_secs(60);

/// §10.4 backoff for a broken adapter contract or a failed parse/normalize:
/// `RequiredFieldMissing`, `ArrayPathMissing`, `ParseFailed`, `NormalizeFailed`.
/// The upstream schema is not going to change back within a tick, and the alert has
/// already gone out on the first observation.
const SCHEMA_BACKOFF: Duration = Duration::from_secs(30 * 60);

/// §10.4 backoff for 401/403 — `AuthRequired`, `Forbidden`, `BotChallenge`. Retrying
/// a soft block quickly is how it becomes a hard one.
const ACCESS_BACKOFF: Duration = Duration::from_secs(30 * 60);

/// §10.4 backoff for 404/410 — `NotFound`, `Gone`. The endpoint is gone; this needs
/// a human to re-point the source, not a faster retry.
const GONE_BACKOFF: Duration = Duration::from_secs(60 * 60);

/// §10.4 backoff for `WrongMediaType`, `MalformedBody`, `EmptyBody` — the shortest
/// hard backoff, because these are the hard failures most likely to be a transient
/// upstream deploy in disguise.
const BODY_BACKOFF: Duration = Duration::from_secs(15 * 60);

/// Everything §11.2 needs to place the next poll (§17.3.1).
///
/// No serde: this is a module DTO computed and consumed inside one tick.
/// [`ScheduleDecision::next_check_at`] reaches storage through
/// `ScheduleState::next_check_at` (§16.2), which owns the wire format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleInput {
    /// The tick's current time, supplied by the caller — `core` never reads a
    /// clock (§32).
    pub now: DateTime<Utc>,
    /// `interval_override_secs.unwrap_or(base_interval_secs)`, already validated
    /// against the §10.2 ceiling by [`validate_interval`].
    pub effective_interval: Duration,
    pub outcome: PollOutcome,
    /// `Some` for [`PollOutcome::Hard`], which is the only branch that reads it.
    pub failure_kind: Option<FailureKind>,
    /// Health state **before** this poll — the probe-eligibility test (§10.3).
    pub state_before: HealthState,
    /// Health state **after** `core::health` applied §8.1 to this poll.
    pub state_after: HealthState,
    /// `probe_attempts` **before** `core::health` touched it (§10.3).
    pub probe_attempts_before: u32,
    /// `consecutive_failures` **after** the increment for this poll (§8.1).
    pub consecutive_failures_after: u32,
    /// The upstream's `Retry-After`, when it sent one with a 429.
    pub retry_after: Option<Duration>,
    /// An already-drawn jitter value, clamped here to [`max_jitter`].
    pub jitter: Duration,
}

/// When the source is next due, and whether that was a priority probe (§17.3.1).
///
/// `probe_scheduled` is the value `core::health`'s probe accounting is checked
/// against in tests and the reason the engine can report "probe" versus "backoff"
/// in `POLL` telemetry without re-deriving the branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScheduleDecision {
    pub next_check_at: DateTime<Utc>,
    pub probe_scheduled: bool,
}

/// Places the next poll for one source — §11.2's table, in order.
///
/// | Outcome | Delay | Probe |
/// |---|---|---|
/// | `Success`, `NotModified` | `effective_interval + jitter` | no |
/// | `Transient`, pre-state `HEALTHY`/`DEGRADED` and `probe_attempts_before < 2` | 30 s, no jitter | **yes** |
/// | `Transient`, post-state `INITIALIZING` | `effective_interval + jitter` | no |
/// | `Transient`, otherwise | `min(interval × 2^(cf_after − 2), 2 h) + jitter` | no |
/// | `Hard` | `backoff_for(kind)` or `effective_interval`, `+ jitter` | no |
/// | `RateLimited` | `max(retry_after, 60 s)`, no jitter | no |
///
/// # Why the probe row is tested first
///
/// Because §10.3 says the *pre*-poll state decides it. The second transient of a
/// run arrives with pre-state `DEGRADED` and one probe spent while `core::health`
/// has already moved the source to `FAILED` — it is still probe #2, and ordering
/// the post-state row first would silently swallow it.
///
/// # Why the last transient row is a fallback rather than a `FAILED` test
///
/// §11.2 enumerates `INITIALIZING` and `FAILED`, but §8.1 lets a transient carry a
/// source from `FAILED` straight to `QUARANTINED` at `cf == 20`. Every transient
/// that is neither an eligible probe nor still initializing therefore backs off:
/// an already-`FAILED` source (including one that got there through a hard failure
/// with `probe_attempts == 0`, which must **not** start a fresh probe sequence), a
/// `DEGRADED` source that has spent both probes, and the poll that quarantines a
/// source. The last of those is inert — a `QUARANTINED` source is not polled at all
/// (§8) — but it still needs a defined, non-panicking value.
#[must_use]
pub fn next_check_at(input: &ScheduleInput) -> ScheduleDecision {
    let (delay, probe_scheduled) = delay_for(input);

    ScheduleDecision {
        next_check_at: advance(input.now, delay),
        probe_scheduled,
    }
}

/// The delay §11.2 selects, and whether it is a priority probe.
///
/// Split out from [`next_check_at`] so that the branch table and the single
/// `now + delay` conversion are separately readable; the conversion is the only
/// place a `Duration` becomes an instant.
fn delay_for(input: &ScheduleInput) -> (Duration, bool) {
    match input.outcome {
        // A 304 is a success (§8.1): the upstream answered correctly that nothing
        // changed, and punishing a well-cached source for it would be absurd.
        PollOutcome::Success | PollOutcome::NotModified => (normal_delay(input), false),

        PollOutcome::Transient => {
            if probe_eligible(input) {
                // No jitter: the probe's whole purpose is to land on the next tick.
                (PROBE_DELAY, true)
            } else if input.state_after == HealthState::Initializing {
                // Initialization never priority-probes and never uses the
                // exponential branch before it enters FAILED (§10.3).
                (normal_delay(input), false)
            } else {
                let backoff =
                    exponential_backoff(input.effective_interval, input.consecutive_failures_after);
                (backoff.saturating_add(clamped_jitter(input)), false)
            }
        }

        // `failure_kind` is `None` only if a caller classified a hard failure
        // without saying which — no override, so the normal interval applies, the
        // same answer `PlausibilityFailed` gets.
        PollOutcome::Hard => {
            let backoff = input
                .failure_kind
                .and_then(backoff_for)
                .unwrap_or(input.effective_interval);
            (backoff.saturating_add(clamped_jitter(input)), false)
        }

        // Honoured exactly, floored at 60 s, never jittered and never probed
        // (§10.3, §10.4): `Retry-After` is an instruction, not a suggestion.
        PollOutcome::RateLimited => (
            input
                .retry_after
                .unwrap_or(RATE_LIMIT_FLOOR)
                .max(RATE_LIMIT_FLOOR),
            false,
        ),
    }
}

/// §10.3's probe test, in one place: **pre**-poll state and **pre**-poll count.
fn probe_eligible(input: &ScheduleInput) -> bool {
    matches!(
        input.state_before,
        HealthState::Healthy | HealthState::Degraded
    ) && input.probe_attempts_before < MAX_PROBE_ATTEMPTS
}

/// One polling interval plus clamped jitter — the ordinary schedule.
fn normal_delay(input: &ScheduleInput) -> Duration {
    input
        .effective_interval
        .saturating_add(clamped_jitter(input))
}

/// The caller's jitter, held to [`max_jitter`].
fn clamped_jitter(input: &ScheduleInput) -> Duration {
    input.jitter.min(max_jitter(input.effective_interval))
}

/// The jitter bound §11.2 publishes to the engine: `min(10 % of effective, 30 s)`.
///
/// The `Jitter` port draws `uniform(0, max_jitter(effective))`; this module clamps
/// whatever arrives to the same bound, so the invariant survives a caller that
/// draws from the wrong range. Exposed because the bound is the port's contract —
/// the draw is not part of the pure core (§11.2, D11).
#[must_use]
pub fn max_jitter(effective_interval: Duration) -> Duration {
    // Exact: `Duration / u32` divides the whole nanosecond count.
    (effective_interval / 10).min(JITTER_CAP)
}

/// The §10.4 backoff column, keyed by failure kind.
///
/// `None` means *no override* — the caller applies the normal effective interval.
/// [`FailureKind::PlausibilityFailed`] is the one hard failure §10.4 gives that
/// answer to deliberately: a suspicious response is a judgement about *this*
/// response, and INV-4 has already protected canonical state by failing the poll,
/// so there is nothing to be gained by polling a working upstream less often.
///
/// Everything else returns `None` because it never reaches this function: the
/// transient kinds take §11.2's probe/exponential branch, `RateLimited` takes
/// `Retry-After`, `ShapeChanged` is not a failure at all (INV-11), and the infra,
/// notify and archive kinds never become a source-health `PollOutcome` (INV-6).
#[must_use]
pub fn backoff_for(kind: FailureKind) -> Option<Duration> {
    match kind {
        FailureKind::RequiredFieldMissing
        | FailureKind::ArrayPathMissing
        | FailureKind::ParseFailed
        | FailureKind::NormalizeFailed => Some(SCHEMA_BACKOFF),

        FailureKind::Forbidden | FailureKind::BotChallenge | FailureKind::AuthRequired => {
            Some(ACCESS_BACKOFF)
        }

        FailureKind::NotFound | FailureKind::Gone => Some(GONE_BACKOFF),

        FailureKind::WrongMediaType | FailureKind::MalformedBody | FailureKind::EmptyBody => {
            Some(BODY_BACKOFF)
        }

        // Stated explicitly rather than left to the fall-through: this is a §10.4
        // row that reads "normal interval", not a kind that never gets here.
        FailureKind::PlausibilityFailed => None,

        _ => None,
    }
}

/// `min(effective × 2^(cf_after − 2), 2 h)`, evaluated so that nothing can overflow.
///
/// `cf_after` is the **post**-health count, so the second consecutive failure —
/// the one that enters `FAILED` from `DEGRADED` — is `2^0`, one plain interval.
/// `saturating_sub` covers a `cf_after` below 2, which §8.1 cannot produce on this
/// branch but which must not underflow if it ever arrives.
///
/// Both the shift and the multiplication are checked, and a failure of either
/// yields the cap: at `cf_after = 20` the true product is `2^18 × interval`, which
/// is beyond two hours by five orders of magnitude, so saturating there is the same
/// answer arrived at sooner.
fn exponential_backoff(effective_interval: Duration, cf_after: u32) -> Duration {
    1_u32
        .checked_shl(cf_after.saturating_sub(2))
        .and_then(|factor| effective_interval.checked_mul(factor))
        .unwrap_or(MAX_BACKOFF)
        .min(MAX_BACKOFF)
}

/// `now + delay`, saturating instead of panicking.
///
/// [`TimeDelta::from_std`] rejects a `Duration` outside chrono's range and
/// `checked_add_signed` rejects a sum outside it; either way the answer is the far
/// end of time rather than a wrapped instant in the past. A source whose next check
/// is `MAX_UTC` is never due again, which is the safe direction — the alternative
/// makes it due immediately and forever.
fn advance(now: DateTime<Utc>, delay: Duration) -> DateTime<Utc> {
    TimeDelta::from_std(delay)
        .ok()
        .and_then(|delta| now.checked_add_signed(delta))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

/// Resolves the override and enforces §10.2's criticality ceiling.
///
/// Returns the effective interval in seconds. §10.2 runs this at registration
/// (`admin add-source`) *and* again at tick start, which is why it lives here
/// rather than in the admin binary.
///
/// # Only the interval actually in force is checked
///
/// The rule is `effective = override.unwrap_or(base); effective <= ceiling`, so a
/// stored `base_interval_secs` above the ceiling passes while an override holds it
/// down. That is deliberate: the ceiling bounds the blind spot the source really
/// has, and the blind spot is one *effective* polling interval (§10.1). Removing
/// the override later re-runs this check at the next tick and fails it then, which
/// is the correct moment to complain.
///
/// # Errors
///
/// [`Stage::Scheduler`] / [`FaultDomain::Infra`] / [`FailureKind::ConfigInvalid`]
/// when the effective interval exceeds `criticality.max_interval_secs()`. §10.2
/// requires the message to explain the tradeoff rather than merely refuse:
/// `admin add-source --criticality critical --interval 30m` is rejected precisely
/// so that an operator who needs a 30-minute interval has to downgrade criticality
/// and thereby record the weaker detection as a decision.
pub fn validate_interval(
    base_interval_secs: u32,
    interval_override_secs: Option<u32>,
    criticality: Criticality,
) -> Result<u32, PipelineError> {
    let (effective_secs, knob) = match interval_override_secs {
        Some(secs) => (secs, "interval_override_secs"),
        None => (base_interval_secs, "base_interval_secs"),
    };
    let ceiling = criticality.max_interval_secs();

    if effective_secs > ceiling {
        return Err(PipelineError::new(
            Stage::Scheduler,
            FaultDomain::Infra,
            FailureKind::ConfigInvalid,
            format!(
                "{knob} = {effective_secs} s exceeds the {ceiling} s interval ceiling for \
                 criticality {criticality} (§10.2); poll this source at least every {ceiling} s, \
                 or downgrade its criticality to accept the wider blind spot explicitly"
            ),
        ));
    }

    Ok(effective_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2026-08-17T10:00:00Z";

    /// Five minutes — a `Critical` source at its ceiling, and the interval whose
    /// 10 % is exactly [`JITTER_CAP`], so the crossover tests can sit either side
    /// of it.
    const INTERVAL: Duration = Duration::from_secs(300);

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .expect("the fixture timestamp is valid RFC 3339")
            .with_timezone(&Utc)
    }

    fn now() -> DateTime<Utc> {
        at(NOW)
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// A healthy 5-minute source that has just polled successfully, with no jitter.
    /// Every test below states only the fields its §11.2 row turns on.
    fn input() -> ScheduleInput {
        ScheduleInput {
            now: now(),
            effective_interval: INTERVAL,
            outcome: PollOutcome::Success,
            failure_kind: None,
            state_before: HealthState::Healthy,
            state_after: HealthState::Healthy,
            probe_attempts_before: 0,
            consecutive_failures_after: 0,
            retry_after: None,
            jitter: Duration::ZERO,
        }
    }

    /// A transient failure observed from `HEALTHY` — §8.1's `HEALTHY | transient`
    /// row, so `core::health` has moved the source to `DEGRADED` and `cf` is 1.
    fn first_transient() -> ScheduleInput {
        ScheduleInput {
            outcome: PollOutcome::Transient,
            state_before: HealthState::Healthy,
            state_after: HealthState::Degraded,
            probe_attempts_before: 0,
            consecutive_failures_after: 1,
            ..input()
        }
    }

    /// The delay a decision represents, which is what every §11.2 row is stated in.
    fn delay_of(decision: &ScheduleDecision) -> Duration {
        (decision.next_check_at - now())
            .to_std()
            .expect("a decision must never land before `now`")
    }

    #[track_caller]
    fn assert_schedule(input: &ScheduleInput, delay: Duration, probe_scheduled: bool) {
        let decision = next_check_at(input);
        assert_eq!(delay_of(&decision), delay, "delay");
        assert_eq!(decision.probe_scheduled, probe_scheduled, "probe_scheduled");
    }

    // -----------------------------------------------------------------------
    // Success and 304 (§11.2 row 1)
    // -----------------------------------------------------------------------

    #[test]
    fn success_schedules_one_interval_plus_jitter_and_no_probe() {
        assert_schedule(
            &ScheduleInput {
                jitter: secs(7),
                ..input()
            },
            secs(307),
            false,
        );
    }

    /// A 304 is a success for §8.1, so it takes the identical row. Asserted rather
    /// than assumed: treating a well-cached source as a failure would push it into
    /// `DEGRADED` and probe an upstream that is behaving perfectly.
    #[test]
    fn not_modified_schedules_exactly_like_success() {
        let success = next_check_at(&input());
        let not_modified = next_check_at(&ScheduleInput {
            outcome: PollOutcome::NotModified,
            ..input()
        });

        assert_eq!(not_modified, success);
        assert_eq!(delay_of(&not_modified), INTERVAL);
    }

    /// The clamp is what makes the bound an invariant of this module rather than a
    /// promise about the caller. A `Jitter` port that drew from the wrong range, or
    /// a test that passed an absurd value, must not be able to push a source hours
    /// out.
    #[test]
    fn supplied_jitter_is_clamped_to_the_bound() {
        assert_schedule(
            &ScheduleInput {
                jitter: secs(3600),
                ..input()
            },
            INTERVAL + JITTER_CAP,
            false,
        );

        // Below the bound it is used exactly — the clamp is a ceiling, not a
        // rounding.
        assert_schedule(
            &ScheduleInput {
                jitter: secs(11),
                ..input()
            },
            secs(311),
            false,
        );
    }

    // -----------------------------------------------------------------------
    // max_jitter (§11.2)
    // -----------------------------------------------------------------------

    /// `min(10 % of effective, 30 s)` crosses over at exactly 300 s. Below it the
    /// percentage binds; above it the 30 s cap does.
    #[test]
    fn max_jitter_crosses_from_ten_percent_to_thirty_seconds_at_a_five_minute_interval() {
        assert_eq!(max_jitter(secs(60)), secs(6));
        assert_eq!(max_jitter(secs(240)), secs(24));
        assert_eq!(max_jitter(secs(299)), Duration::from_millis(29_900));

        // The crossover itself: 10 % of 300 s *is* the cap.
        assert_eq!(max_jitter(secs(300)), JITTER_CAP);

        assert_eq!(max_jitter(secs(301)), JITTER_CAP);
        assert_eq!(max_jitter(secs(600)), JITTER_CAP);
        assert_eq!(max_jitter(secs(1800)), JITTER_CAP);

        // Degenerate inputs stay total.
        assert_eq!(max_jitter(Duration::ZERO), Duration::ZERO);
        assert_eq!(max_jitter(Duration::MAX), JITTER_CAP);
    }

    // -----------------------------------------------------------------------
    // Priority probes (§10.3, §11.2 row 2)
    // -----------------------------------------------------------------------

    /// Probe #1: the first transient from `HEALTHY`. Exactly 30 s, and no jitter —
    /// a jittered probe would sometimes miss the next tick, which is the only thing
    /// the 30 s value buys.
    #[test]
    fn healthy_transient_with_no_prior_probe_schedules_the_thirty_second_probe() {
        assert_schedule(
            &ScheduleInput {
                jitter: secs(30),
                ..first_transient()
            },
            PROBE_DELAY,
            true,
        );
    }

    /// Probe #2, and the regression this module's branch order exists for. Pre-state
    /// `DEGRADED` with one probe spent makes this probe #2 (§10.3) even though
    /// `core::health` has already applied §8.1's `DEGRADED | transient` row and
    /// moved the source to `FAILED` in the same poll. Testing the post-state first
    /// would swallow the second probe and cost a whole confirmation tick.
    #[test]
    fn degraded_transient_at_probe_one_still_probes_although_health_moved_it_to_failed() {
        assert_schedule(
            &ScheduleInput {
                state_before: HealthState::Degraded,
                state_after: HealthState::Failed,
                probe_attempts_before: 1,
                consecutive_failures_after: 2,
                ..first_transient()
            },
            PROBE_DELAY,
            true,
        );
    }

    /// A 429 run parks a source in `DEGRADED` without touching `probe_attempts`
    /// (§8.1), so a transient arriving afterwards is still probe #1 — the pre-poll
    /// counter, not the pre-poll state, is what has been spent.
    #[test]
    fn degraded_transient_after_a_rate_limit_run_probes_because_no_probe_was_spent() {
        assert_schedule(
            &ScheduleInput {
                state_before: HealthState::Degraded,
                state_after: HealthState::Failed,
                probe_attempts_before: 0,
                consecutive_failures_after: 2,
                ..first_transient()
            },
            PROBE_DELAY,
            true,
        );
    }

    /// Both probes spent: eligibility ends at the counter regardless of the state
    /// it is read from, and the exponential branch takes over.
    #[test]
    fn degraded_transient_with_both_probes_spent_backs_off_instead_of_probing() {
        assert_schedule(
            &ScheduleInput {
                state_before: HealthState::Degraded,
                state_after: HealthState::Failed,
                probe_attempts_before: 2,
                consecutive_failures_after: 3,
                ..first_transient()
            },
            secs(600),
            false,
        );
    }

    // -----------------------------------------------------------------------
    // FAILED — exponential backoff (§11.2 row 4)
    // -----------------------------------------------------------------------

    /// A source that entered `FAILED` through a hard failure has `probe_attempts`
    /// at 0 (§8.1), and §10.3 is explicit that this must **not** start a fresh
    /// two-probe sequence: the pre-state, not the counter, disqualifies it. Getting
    /// this wrong probes a known-dead upstream every 30 s indefinitely.
    #[test]
    fn failed_transient_with_zero_probe_attempts_backs_off_and_starts_no_probe_sequence() {
        assert_schedule(
            &ScheduleInput {
                state_before: HealthState::Failed,
                state_after: HealthState::Failed,
                probe_attempts_before: 0,
                consecutive_failures_after: 3,
                ..first_transient()
            },
            secs(600),
            false,
        );
    }

    #[test]
    fn failed_transient_with_both_probes_spent_backs_off() {
        assert_schedule(
            &ScheduleInput {
                state_before: HealthState::Failed,
                state_after: HealthState::Failed,
                probe_attempts_before: 2,
                consecutive_failures_after: 4,
                ..first_transient()
            },
            secs(1200),
            false,
        );
    }

    /// `2^(cf_after − 2)`: the post-health count, so the failure that *enters*
    /// `FAILED` is one plain interval and each one after it doubles. Jitter rides on
    /// top of the capped value, which is why the last row is 2 h + 30 s rather than
    /// 2 h.
    #[test]
    fn exponential_backoff_doubles_from_the_second_consecutive_failure() {
        let failed = |cf_after| ScheduleInput {
            state_before: HealthState::Failed,
            state_after: HealthState::Failed,
            probe_attempts_before: 2,
            consecutive_failures_after: cf_after,
            ..first_transient()
        };

        assert_schedule(&failed(2), INTERVAL, false); // 2^0
        assert_schedule(&failed(3), secs(600), false); // 2^1
        assert_schedule(&failed(4), secs(1200), false); // 2^2
        assert_schedule(&failed(5), secs(2400), false); // 2^3
        assert_schedule(&failed(6), secs(4800), false); // 2^4

        // 2^5 × 300 s is 9600 s, past the cap; from here the answer stops moving
        // and §8.1 quarantines the source at 20.
        assert_schedule(&failed(7), MAX_BACKOFF, false);
        assert_schedule(&failed(20), MAX_BACKOFF, false);

        assert_schedule(
            &ScheduleInput {
                jitter: secs(30),
                ..failed(20)
            },
            MAX_BACKOFF + JITTER_CAP,
            false,
        );
    }

    /// `2^18 × 1800 s` is the case §11.2 calls out by name: it overflows a naive
    /// `u32` seconds computation. The cap has to be applied before the
    /// multiplication can hurt, and `Duration::MAX` proves the checked
    /// multiplication itself is total.
    #[test]
    fn exponential_backoff_caps_at_two_hours_without_overflowing() {
        let background = ScheduleInput {
            effective_interval: secs(1800),
            state_before: HealthState::Failed,
            state_after: HealthState::Failed,
            consecutive_failures_after: 20,
            ..first_transient()
        };
        assert_schedule(&background, MAX_BACKOFF, false);

        assert_eq!(exponential_backoff(secs(1800), 20), MAX_BACKOFF);
        assert_eq!(exponential_backoff(Duration::MAX, 20), MAX_BACKOFF);

        // A shift wide enough to be undefined in C is merely `None` here.
        assert_eq!(exponential_backoff(INTERVAL, u32::MAX), MAX_BACKOFF);

        // Below the branch's reachable range: `saturating_sub` must not underflow.
        assert_eq!(exponential_backoff(INTERVAL, 0), INTERVAL);
        assert_eq!(exponential_backoff(INTERVAL, 1), INTERVAL);
    }

    // -----------------------------------------------------------------------
    // INITIALIZING (§11.2 row 3)
    // -----------------------------------------------------------------------

    /// While a source is still initializing it neither probes nor backs off — it
    /// keeps its normal interval (§10.3). The assertion is exact for a reason: at
    /// `cf = 2` an off-by-one exponent (`2^(cf−1)`) would return 600 s here, and
    /// nothing else in this module would notice.
    #[test]
    fn initializing_transient_that_stays_initializing_uses_the_normal_interval() {
        let initializing = |cf_after| ScheduleInput {
            state_before: HealthState::Initializing,
            state_after: HealthState::Initializing,
            probe_attempts_before: 0,
            consecutive_failures_after: cf_after,
            jitter: secs(9),
            ..first_transient()
        };

        assert_schedule(&initializing(1), secs(309), false);
        assert_schedule(&initializing(2), secs(309), false);
    }

    /// The third transient during initialization is §8.1's `INITIALIZING |
    /// transient, cf == 3` row: `FAILED`, and from there the exponential branch —
    /// `2^(3−2)` — applies like any other failed source.
    #[test]
    fn initializing_third_transient_that_enters_failed_uses_exponential_backoff() {
        assert_schedule(
            &ScheduleInput {
                state_before: HealthState::Initializing,
                state_after: HealthState::Failed,
                probe_attempts_before: 0,
                consecutive_failures_after: 3,
                ..first_transient()
            },
            secs(600),
            false,
        );
    }

    // -----------------------------------------------------------------------
    // Hard failures (§10.4, §11.2 row 5)
    // -----------------------------------------------------------------------

    fn hard(kind: FailureKind) -> ScheduleInput {
        ScheduleInput {
            outcome: PollOutcome::Hard,
            failure_kind: Some(kind),
            state_before: HealthState::Healthy,
            state_after: HealthState::Failed,
            probe_attempts_before: 0,
            consecutive_failures_after: 1,
            ..input()
        }
    }

    #[test]
    fn not_found_backs_off_one_hour() {
        assert_schedule(&hard(FailureKind::NotFound), secs(3600), false);
        assert_schedule(&hard(FailureKind::Gone), secs(3600), false);
    }

    #[test]
    fn required_field_missing_backs_off_thirty_minutes() {
        assert_schedule(&hard(FailureKind::RequiredFieldMissing), secs(1800), false);
    }

    /// §10.4's one "normal interval" row. `backoff_for` returns no override, so the
    /// source keeps polling at its configured cadence: the response was suspicious,
    /// not the upstream broken, and INV-4 has already refused to write it.
    #[test]
    fn plausibility_failed_keeps_the_normal_interval() {
        assert_eq!(backoff_for(FailureKind::PlausibilityFailed), None);
        assert_schedule(
            &ScheduleInput {
                jitter: secs(4),
                ..hard(FailureKind::PlausibilityFailed)
            },
            secs(304),
            false,
        );
    }

    /// A hard failure carries jitter, unlike a probe or a 429 — nothing about a
    /// fixed backoff should re-align every failing source on the same minute.
    #[test]
    fn hard_backoff_carries_clamped_jitter() {
        assert_schedule(
            &ScheduleInput {
                jitter: secs(3600),
                ..hard(FailureKind::NotFound)
            },
            secs(3600) + JITTER_CAP,
            false,
        );
    }

    /// A `Hard` outcome with no kind attached cannot be looked up, so it falls back
    /// to the normal interval rather than inventing a backoff.
    #[test]
    fn hard_without_a_failure_kind_falls_back_to_the_normal_interval() {
        assert_schedule(
            &ScheduleInput {
                failure_kind: None,
                ..hard(FailureKind::NotFound)
            },
            INTERVAL,
            false,
        );
    }

    /// The whole §10.4 backoff column, in one place, so a future edit to the table
    /// has to touch a test that names the section.
    #[test]
    fn backoff_for_reproduces_the_ten_point_four_column() {
        for kind in [
            FailureKind::RequiredFieldMissing,
            FailureKind::ArrayPathMissing,
            FailureKind::ParseFailed,
            FailureKind::NormalizeFailed,
            FailureKind::Forbidden,
            FailureKind::BotChallenge,
            FailureKind::AuthRequired,
        ] {
            assert_eq!(backoff_for(kind), Some(secs(1800)), "{kind:?}");
        }

        for kind in [FailureKind::NotFound, FailureKind::Gone] {
            assert_eq!(backoff_for(kind), Some(secs(3600)), "{kind:?}");
        }

        for kind in [
            FailureKind::WrongMediaType,
            FailureKind::MalformedBody,
            FailureKind::EmptyBody,
        ] {
            assert_eq!(backoff_for(kind), Some(secs(900)), "{kind:?}");
        }

        // Kinds that never reach this branch: transient ones take §11.2's probe or
        // exponential row, 429 takes `Retry-After`, and `ShapeChanged` is not a
        // failure at all (INV-11).
        for kind in [
            FailureKind::Timeout,
            FailureKind::ConnectFailed,
            FailureKind::DnsFailed,
            FailureKind::TlsError,
            FailureKind::ServerError,
            FailureKind::RateLimited,
            FailureKind::ShapeChanged,
            FailureKind::PlausibilityFailed,
            FailureKind::DbFailed,
            FailureKind::ConfigInvalid,
        ] {
            assert_eq!(backoff_for(kind), None, "{kind:?}");
        }
    }

    // -----------------------------------------------------------------------
    // 429 (§10.4, §11.2 row 6)
    // -----------------------------------------------------------------------

    fn rate_limited(retry_after: Option<Duration>) -> ScheduleInput {
        ScheduleInput {
            outcome: PollOutcome::RateLimited,
            failure_kind: Some(FailureKind::RateLimited),
            retry_after,
            state_before: HealthState::Healthy,
            state_after: HealthState::Degraded,
            probe_attempts_before: 0,
            consecutive_failures_after: 1,
            // Deliberately maximal: no 429 row may pick any of it up.
            jitter: secs(3600),
            ..input()
        }
    }

    /// A rate limiter asking to be retried in 10 s is still asking us to hammer it,
    /// and probing one is how a soft block becomes a hard one (§10.3).
    #[test]
    fn rate_limited_floors_a_short_retry_after_at_sixty_seconds() {
        assert_schedule(&rate_limited(Some(secs(10))), RATE_LIMIT_FLOOR, false);
        assert_schedule(&rate_limited(Some(Duration::ZERO)), RATE_LIMIT_FLOOR, false);
    }

    /// Above the floor the header is honoured exactly — no jitter, no rounding, no
    /// interval. It is an instruction from the upstream about its own capacity.
    #[test]
    fn rate_limited_honours_a_longer_retry_after_exactly() {
        assert_schedule(&rate_limited(Some(secs(300))), secs(300), false);
        assert_schedule(&rate_limited(Some(secs(60))), RATE_LIMIT_FLOOR, false);
    }

    #[test]
    fn rate_limited_without_a_retry_after_uses_the_floor() {
        assert_schedule(&rate_limited(None), RATE_LIMIT_FLOOR, false);
    }

    // -----------------------------------------------------------------------
    // Overflow (§11.2)
    // -----------------------------------------------------------------------

    /// Nothing a caller can pass may panic or wrap. An interval past chrono's range
    /// saturates to the far end of time — never due again — rather than wrapping to
    /// an instant in the past, which would make the source due on every tick
    /// forever.
    #[test]
    fn an_out_of_range_interval_saturates_instead_of_wrapping() {
        let decision = next_check_at(&ScheduleInput {
            effective_interval: Duration::MAX,
            jitter: Duration::MAX,
            ..input()
        });

        assert_eq!(decision.next_check_at, DateTime::<Utc>::MAX_UTC);
        assert!(decision.next_check_at > now());
        assert!(!decision.probe_scheduled);
    }

    // -----------------------------------------------------------------------
    // §10.2 criticality validation
    // -----------------------------------------------------------------------

    #[test]
    fn validate_interval_accepts_every_criticality_at_its_ceiling() {
        assert_eq!(validate_interval(300, None, Criticality::Critical), Ok(300));
        assert_eq!(validate_interval(600, None, Criticality::Standard), Ok(600));
        assert_eq!(
            validate_interval(1800, None, Criticality::Background),
            Ok(1800)
        );

        // Faster than the ceiling is always fine — the ceiling bounds the blind
        // spot, it does not prescribe a cadence.
        assert_eq!(validate_interval(60, None, Criticality::Critical), Ok(60));
    }

    /// §10.2's worked example: `--criticality critical --interval 30m` is rejected,
    /// and rejected as `Scheduler`/`Infra`/`ConfigInvalid` so that §10.4 routes it
    /// as a system fault rather than an upstream one.
    #[test]
    fn validate_interval_rejects_an_interval_above_the_ceiling() {
        let err = validate_interval(1800, None, Criticality::Critical)
            .expect_err("30 min under Critical is the case §10.2 rejects by name");

        assert_eq!(err.stage, Stage::Scheduler);
        assert_eq!(err.domain, FaultDomain::Infra);
        assert_eq!(err.kind, FailureKind::ConfigInvalid);
        assert!(
            err.detail.message.contains("1800") && err.detail.message.contains("300"),
            "the message must state both the value and the ceiling: {}",
            err.detail.message
        );
        assert!(
            err.detail.message.contains("criticality"),
            "§10.2 requires the message to name the tradeoff: {}",
            err.detail.message
        );

        // One second over is over.
        assert!(validate_interval(301, None, Criticality::Critical).is_err());
        assert!(validate_interval(601, None, Criticality::Standard).is_err());
        assert!(validate_interval(1801, None, Criticality::Background).is_err());
    }

    /// The override is the interval actually in force, so it is the one measured
    /// against the ceiling — in both directions.
    #[test]
    fn validate_interval_measures_the_override_rather_than_the_base() {
        // An override inside the ceiling is accepted and returned, even though the
        // stored base is not: the blind spot is one *effective* interval (§10.1).
        assert_eq!(
            validate_interval(1800, Some(120), Criticality::Critical),
            Ok(120)
        );

        // And an override outside it is rejected, even though the base is fine.
        let err = validate_interval(60, Some(1800), Criticality::Critical)
            .expect_err("the override is what the source will actually be polled at");
        assert_eq!(err.kind, FailureKind::ConfigInvalid);
        assert!(
            err.detail.message.contains("interval_override_secs"),
            "the message must name the knob that has to change: {}",
            err.detail.message
        );
    }
}
