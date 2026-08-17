//! Source health — §8.1's transition table, and nothing else (§8, §10.3, §13.6).
//!
//! §8.1 is the single state-transition authority: *"where it and any other section
//! disagree, this table wins."* Every one of its 23 rows is written out below as its
//! own branch, even where two rows could be folded together, because the rows that
//! look redundant are exactly the three v1.1 got wrong — what a transient failure
//! does while `INITIALIZING`, whether a hard failure during initialization waits for
//! the third failure, and how a permanently rate-limited source ever leaves
//! `DEGRADED`. A compressed match reintroduces the ambiguity the table was written
//! to remove.
//!
//! # Counter ownership (§8.1, §10.3)
//!
//! This module is the **only** Phase-1 module permitted to mutate
//! `consecutive_failures`, `probe_attempts` and `first_failure_at`. `core::schedule`
//! receives the pre-poll state/probe count and the post-health state/failure count
//! and computes `next_check_at` alone. Because [`next`] returns a whole
//! [`HealthSnapshot`], it also carries the derived timestamps and the §9 failure
//! triple forward — a caller that had to patch those afterwards would be a second
//! writer of the same item, which is the divergence §8.1 exists to prevent.
//!
//! `cf` throughout is **post-increment**: the value *after* this poll has been
//! counted, which is how §8.1 states its `cf < 3` and `cf == 20` predicates.
//!
//! # `probe_attempts` is not a failure counter
//!
//! Only a transient failure observed from `HEALTHY` or `DEGRADED` increments it
//! (§10.3), and **a 429 never touches it** (§10.4) — probing a rate limiter is how a
//! soft block becomes a hard one. The distinction between the table's `0` and its
//! *unchanged* is therefore load-bearing: a hard failure from `DEGRADED` resets a
//! spent probe budget to `0`, while a 429 from the same state leaves it exactly
//! where it was.
//!
//! # Bootstrap recovery is not recovery (INV-10, §13.6)
//!
//! `bootstrap_state`, not `health_state`, is authoritative for the pipeline branch.
//! A source that failed or was rate-limited during initialization and then succeeds
//! returns to `INITIALIZING` with its counters cleared and **no** `SOURCE_RECOVERED`
//! event. Only the bootstrap commit may make it `HEALTHY` and emit the one
//! `SOURCE_BOOTSTRAPPED` summary. This module never runs diff and never produces a
//! job event, so no path through it can turn a recovery into a `NEW_JOB` storm.
//!
//! # A 429 never breaks a source, and never hides one (§8.1, INV-16)
//!
//! Rate limiting is the upstream working as designed, so a 429 never reaches
//! `FAILED`. But `DEGRADED` re-alerts are suppressed while in state, so without the
//! `DEGRADED → QUARANTINED` edge a source returning 429 forever would alert once and
//! then be listed nowhere, for months — *stops re-alerting* is required of INV-16,
//! *silently forgotten* is forbidden.
//!
//! # Deliberately not here
//!
//! Alert **delivery** is a §15 policy applied to the events this module returns, not
//! a property of a transition: the `FAILED` re-alert every 6 h re-sends the
//! already-durable `SOURCE_FAILED` (same identity, because `first_failure_at` is
//! preserved) rather than minting a new event, so those rows emit `None` here.
//! `last_health_alert_at` is carried through untouched for the same reason. System
//! correlation (§25), scheduling (§11.2) and diffing (§13.8) are other modules'.

use crate::model::{EventType, HealthSnapshot, HealthState, PollOutcome};
use chrono::{DateTime, Utc};
use jobmon_errors::{FailureKind, FaultDomain, Stage};

/// The consecutive transient failure that ends initialization (§8.1, `INITIALIZING |
/// transient, cf == 3`).
///
/// Initialization gets three because the first poll of a brand-new source is the
/// most likely one to hit a cold DNS entry or a slow first response, and there is no
/// established baseline to compare a failure against.
const INIT_TRANSIENT_LIMIT: u32 = 3;

/// The consecutive failure that quarantines a source, reached from `FAILED` **or**
/// from `DEGRADED` (§8.1, INV-16).
const QUARANTINE_LIMIT: u32 = 20;

/// The `SOURCE_BOOTSTRAPPED` summary of §8.1's second row.
///
/// It carries no `first_failure_at` because §13.2.3 keys this type on `poll_seq`,
/// which is `core::schedule`/META state and not health's to supply.
const BOOTSTRAPPED: HealthEvent = HealthEvent {
    event_type: EventType::SourceBootstrapped,
    first_failure_at: None,
};

/// The event a §8.1 row emits, if it emits one (§17.3.1).
///
/// No serde: this is a module DTO computed and consumed inside one tick. It carries
/// `first_failure_at` because that value — not `detected_at`, not a ULID — is the
/// discriminator in the `SOURCE_DEGRADED` / `SOURCE_FAILED` / `SOURCE_RECOVERED` /
/// `SOURCE_QUARANTINED` identities of §13.2.3. One outage therefore produces one
/// identity per event type no matter how many polls it spans, which is what makes a
/// retried transition idempotent (INV-2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HealthEvent {
    pub event_type: EventType,
    /// The instant the current outage began. `None` only for
    /// [`EventType::SourceBootstrapped`], whose identity does not use it.
    pub first_failure_at: Option<DateTime<Utc>>,
}

/// The source-health classification of a [`FailureKind`], if it has one (§17.3.1).
///
/// The mapping is deliberately **partial**, and the `None` arms are the whole point
/// of the function rather than a fall-through:
///
/// - `ShapeChanged` is not a failure (INV-11). A changed response shape is
///   `API_CHANGED` telemetry and the poll succeeds; only a violated adapter
///   contract, a parse error or a plausibility failure is a failure.
/// - `LeaseContention` and `DbConditionalCheckFailed` are **success signals** (§9,
///   §13.5) — another invocation legitimately owns the source, or a prior attempt
///   already applied this exact transition.
/// - The infra kinds are system-level (§10.4). A source whose poll died because
///   *our* DynamoDB was throttled has told us nothing about the upstream, and
///   recording it against the source would both slander a working API and hide a
///   system fault behind N unrelated source alerts.
/// - The notify and archive kinds are INV-6 verbatim: "a Telegram outage must not
///   mark a source unhealthy". `ArchivePutFailed` never invalidates a poll at all.
///
/// Returning `Option` rather than defaulting the unmapped kinds to
/// [`PollOutcome::Hard`] is what makes that separation impossible to get wrong by
/// accident: there is no value to pass to [`next`], so a caller cannot silently
/// charge a Telegram or DynamoDB fault to source health.
///
/// [`PollOutcome::Success`] and [`PollOutcome::NotModified`] are unreachable here —
/// a `FailureKind` describes something that went wrong.
#[must_use]
pub fn outcome_for(kind: FailureKind) -> Option<PollOutcome> {
    match kind {
        // §10.4: retryable, alert only after confirmation, probe twice.
        FailureKind::Timeout
        | FailureKind::ConnectFailed
        | FailureKind::DnsFailed
        | FailureKind::TlsError
        | FailureKind::ServerError => Some(PollOutcome::Transient),

        // §10.4: its own class, because it is the upstream working as designed.
        FailureKind::RateLimited => Some(PollOutcome::RateLimited),

        // §10.4: alert on first observation; a retry cannot fix any of these.
        FailureKind::NotFound
        | FailureKind::Gone
        | FailureKind::Forbidden
        | FailureKind::BotChallenge
        | FailureKind::AuthRequired
        | FailureKind::WrongMediaType
        | FailureKind::MalformedBody
        | FailureKind::EmptyBody
        | FailureKind::ParseFailed
        | FailureKind::RequiredFieldMissing
        | FailureKind::ArrayPathMissing
        | FailureKind::NormalizeFailed
        | FailureKind::PlausibilityFailed => Some(PollOutcome::Hard),

        // Not a failure — `API_CHANGED` telemetry, poll succeeds (INV-11).
        FailureKind::ShapeChanged => None,

        // Not failures — success signals (§9, §13.5).
        FailureKind::LeaseContention | FailureKind::DbConditionalCheckFailed => None,

        // Ours, not theirs: system-level health (§10.4).
        FailureKind::DbThrottled
        | FailureKind::DbAccessDenied
        | FailureKind::DbFailed
        | FailureKind::TickTimeout
        | FailureKind::ConfigInvalid
        | FailureKind::SecretUnavailable => None,

        // Notification health is independent of source health (INV-6).
        FailureKind::NotifySendFailed
        | FailureKind::NotifyRateLimited
        | FailureKind::NotifyAuthFailed => None,

        // Degrades the archive subsystem only (INV-6 corollary).
        FailureKind::ArchivePutFailed => None,
    }
}

/// Applies one poll's outcome to a source's health — §8.1, row by row.
///
/// | From | Poll result | To | `cf` | `probe_attempts` | Event |
/// |---|---|---|---|---|---|
/// | `INITIALIZING` | success, bootstrap not yet complete | `INITIALIZING` | 0 | 0 | — |
/// | `INITIALIZING` | success, bootstrap complete | `HEALTHY` | 0 | 0 | `SOURCE_BOOTSTRAPPED` |
/// | `INITIALIZING` | transient, `cf < 3` | `INITIALIZING` | +1 | unchanged (0) | — |
/// | `INITIALIZING` | transient, `cf == 3` | `FAILED` | +1 | 0 | `SOURCE_FAILED` |
/// | `INITIALIZING` | 429 | `DEGRADED` | +1 | 0 | `SOURCE_DEGRADED` |
/// | `INITIALIZING` | hard | `FAILED` | +1 | 0 | `SOURCE_FAILED` |
/// | `HEALTHY` | success | `HEALTHY` | 0 | 0 | — |
/// | `HEALTHY` | transient | `DEGRADED` | +1 | +1 | `SOURCE_DEGRADED` |
/// | `HEALTHY` | 429 | `DEGRADED` | +1 | unchanged | `SOURCE_DEGRADED` |
/// | `HEALTHY` | hard | `FAILED` | +1 | 0 | `SOURCE_FAILED` |
/// | `DEGRADED` | success, bootstrap incomplete | `INITIALIZING` | 0 | 0 | — |
/// | `DEGRADED` | success, bootstrap complete | `HEALTHY` | 0 | 0 | `SOURCE_RECOVERED` |
/// | `DEGRADED` | transient | `FAILED` | +1 | +1 | `SOURCE_FAILED` |
/// | `DEGRADED` | hard | `FAILED` | +1 | 0 | `SOURCE_FAILED` |
/// | `DEGRADED` | 429, `cf < 20` | `DEGRADED` | +1 | unchanged | — |
/// | `DEGRADED` | 429, `cf == 20` | `QUARANTINED` | +1 | unchanged | `SOURCE_QUARANTINED` |
/// | `FAILED` | success, bootstrap incomplete | `INITIALIZING` | 0 | 0 | — |
/// | `FAILED` | success, bootstrap complete | `HEALTHY` | 0 | 0 | `SOURCE_RECOVERED` |
/// | `FAILED` | any failure, `cf < 20` | `FAILED` | +1 | unchanged | — |
/// | `FAILED` | any failure, `cf == 20` | `QUARANTINED` | +1 | unchanged | `SOURCE_QUARANTINED` |
/// | `QUARANTINED` | *not polled* | `QUARANTINED` | — | — | — |
///
/// The remaining two rows are the admin operations [`disable`] and [`enable`].
///
/// # `bootstrap_complete`
///
/// Is `bootstrap_state == complete` going to hold once this poll commits? That one
/// reading covers both places the table asks: for `DEGRADED`/`FAILED` it means
/// bootstrap finished at some earlier poll, and for `INITIALIZING` it means *this*
/// poll's bootstrap commit finished it. §13.6 makes the commit the only operation
/// allowed to reach `HEALTHY` and emit `SOURCE_BOOTSTRAPPED`, so the engine passes
/// `true` from `INITIALIZING` exactly once.
///
/// # `failure`
///
/// The §9 triple describing *this* poll's failure. It is recorded verbatim onto the
/// snapshot for the `SOURCE_FAILED` alert body (§8) and cleared on any success — the
/// fields describe the outage in progress, so a stale triple surviving a recovery
/// would put a resolved fault in the next alert.
///
/// # `first_failure_at`
///
/// Set on the `cf` 0 → 1 transition, preserved for the rest of the outage, cleared
/// by any success. A failing poll that arrives with `cf > 0` and no timestamp — a
/// snapshot no sequence of calls here can produce — has one written rather than
/// inherited, because a `None` discriminator would give every poll of that outage
/// the same event key as every other outage's (§13.2.3).
///
/// A `SOURCE_RECOVERED` carries the **pre-clear** value, since the event names the
/// outage that just ended and its key must stay derivable.
///
/// # Thresholds are `>=`, not `==`
///
/// §8.1 writes `cf == 3` and `cf == 20`; every reachable sequence hits them exactly,
/// because the poll that reaches 20 leaves the source `QUARANTINED` and a
/// quarantined source is not polled. `>=` differs only for a stored counter that
/// somehow starts beyond the threshold, and there the difference matters: `==` would
/// leave such a source `FAILED` forever, re-alerting every 6 h and never reaching
/// the digest's quarantined list, which is precisely the silent-forever state INV-16
/// forbids.
#[must_use]
pub fn next(
    current: &HealthSnapshot,
    outcome: PollOutcome,
    bootstrap_complete: bool,
    failure: Option<(Stage, FaultDomain, FailureKind)>,
    now: DateTime<Utc>,
) -> (HealthSnapshot, Option<HealthEvent>) {
    use EventType::{SourceDegraded, SourceFailed, SourceQuarantined};
    use HealthState::{Degraded, Disabled, Failed, Healthy, Initializing, Quarantined};
    use PollOutcome::{Hard, NotModified, RateLimited, Success, Transient};

    let poll = Poll {
        current,
        now,
        cf_after: current.consecutive_failures.saturating_add(1),
        failure,
    };

    match (current.health_state, outcome) {
        // ── INITIALIZING ────────────────────────────────────────────────────
        (Initializing, Success | NotModified) => {
            if bootstrap_complete {
                // `INITIALIZING | success, bootstrap complete → HEALTHY`. The
                // bootstrap commit landed; this is the one summary event (§13.6).
                success_row(&poll, Healthy, Some(BOOTSTRAPPED))
            } else {
                // `INITIALIZING | success, bootstrap not yet complete →
                // INITIALIZING`. Silent by design (INV-10): the engine continues
                // the bootstrap algorithm, and normal DIFF is still forbidden.
                success_row(&poll, Initializing, None)
            }
        }

        (Initializing, Transient) => {
            if poll.cf_after >= INIT_TRANSIENT_LIMIT {
                // `INITIALIZING | transient, cf == 3 → FAILED`.
                failure_row(&poll, Failed, Probe::Reset, Some(SourceFailed))
            } else {
                // `INITIALIZING | transient, cf < 3 → INITIALIZING`, probes
                // unchanged (0): an initializing source never priority-probes
                // (§10.3), so it keeps its normal interval while it retries.
                failure_row(&poll, Initializing, Probe::Unchanged, None)
            }
        }

        // `INITIALIZING | 429 → DEGRADED`. Rate limiting during initialization is
        // still not a broken source.
        (Initializing, RateLimited) => {
            failure_row(&poll, Degraded, Probe::Reset, Some(SourceDegraded))
        }

        // `INITIALIZING | hard → FAILED`. A hard failure does **not** wait for the
        // third failure — one of the three ambiguities §8.1 was written to settle.
        (Initializing, Hard) => failure_row(&poll, Failed, Probe::Reset, Some(SourceFailed)),

        // ── HEALTHY ─────────────────────────────────────────────────────────
        // `HEALTHY | success → HEALTHY`. A 304 is a success (§8.1): the upstream
        // answered correctly that nothing changed.
        (Healthy, Success | NotModified) => success_row(&poll, Healthy, None),

        // `HEALTHY | transient → DEGRADED`, `probe_attempts +1` — this schedules
        // probe #1 (§10.3).
        (Healthy, Transient) => {
            failure_row(&poll, Degraded, Probe::Increment, Some(SourceDegraded))
        }

        // `HEALTHY | 429 → DEGRADED`, probes **unchanged**: no probe is scheduled
        // and none is spent (§10.4).
        (Healthy, RateLimited) => {
            failure_row(&poll, Degraded, Probe::Unchanged, Some(SourceDegraded))
        }

        // `HEALTHY | hard → FAILED`, alert immediately on first observation.
        (Healthy, Hard) => failure_row(&poll, Failed, Probe::Reset, Some(SourceFailed)),

        // ── DEGRADED ────────────────────────────────────────────────────────
        (Degraded, Success | NotModified) => {
            if bootstrap_complete {
                // `DEGRADED | success, bootstrap complete → HEALTHY`, with the
                // pre-clear `first_failure_at` so the outage stays identifiable.
                success_row(&poll, Healthy, recovered(current))
            } else {
                // `DEGRADED | success, bootstrap incomplete → INITIALIZING`.
                // Counters clear, but there is **no** `SOURCE_RECOVERED`: nothing
                // has recovered yet, the source has merely become able to finish
                // bootstrapping (INV-10, §13.6).
                success_row(&poll, Initializing, None)
            }
        }

        // `DEGRADED | transient → FAILED`, `probe_attempts +1`. Pre-state is
        // `DEGRADED`, so `core::schedule` still sees this as probe #2 even though
        // health has already escalated the state (§10.3).
        (Degraded, Transient) => failure_row(&poll, Failed, Probe::Increment, Some(SourceFailed)),

        // `DEGRADED | hard → FAILED`, probes back to 0: the probe sequence is
        // abandoned, and §10.3 forbids a `FAILED` source starting a new one.
        (Degraded, Hard) => failure_row(&poll, Failed, Probe::Reset, Some(SourceFailed)),

        (Degraded, RateLimited) => {
            if poll.cf_after >= QUARANTINE_LIMIT {
                // `DEGRADED | 429, cf == 20 → QUARANTINED`. The edge that stops a
                // permanently rate-limited source from being silently forgotten
                // (INV-16); in practice only a 429 run can reach it.
                failure_row(
                    &poll,
                    Quarantined,
                    Probe::Unchanged,
                    Some(SourceQuarantined),
                )
            } else {
                // `DEGRADED | 429, cf < 20 → DEGRADED`, no event: re-alerts are
                // suppressed while in state (§8).
                failure_row(&poll, Degraded, Probe::Unchanged, None)
            }
        }

        // ── FAILED ──────────────────────────────────────────────────────────
        (Failed, Success | NotModified) => {
            if bootstrap_complete {
                // `FAILED | success, bootstrap complete → HEALTHY`.
                success_row(&poll, Healthy, recovered(current))
            } else {
                // `FAILED | success, bootstrap incomplete → INITIALIZING`, no
                // `SOURCE_RECOVERED` (INV-10, §13.6).
                success_row(&poll, Initializing, None)
            }
        }

        (Failed, Transient | Hard | RateLimited) => {
            if poll.cf_after >= QUARANTINE_LIMIT {
                // `FAILED | any failure, cf == 20 → QUARANTINED`, one final message.
                failure_row(
                    &poll,
                    Quarantined,
                    Probe::Unchanged,
                    Some(SourceQuarantined),
                )
            } else {
                // `FAILED | any failure, cf < 20 → FAILED`, no new event: the 6 h
                // re-alert re-sends the existing `SOURCE_FAILED`, whose identity is
                // unchanged because `first_failure_at` is preserved (§13.2.3, §15).
                //
                // A 429 lands here too, and that does not contradict "a 429 never
                // reaches `FAILED`" — the source is already `FAILED` for some other
                // reason, and being rate-limited on top of that is still a failed
                // poll that must count toward quarantine.
                failure_row(&poll, Failed, Probe::Unchanged, None)
            }
        }

        // ── QUARANTINED, DISABLED ───────────────────────────────────────────
        // `QUARANTINED | not polled → QUARANTINED`, every column a dash. §8 stops
        // polling in both states, so if the engine calls anyway — a stale GSI1 hint
        // (INV-7), a race with an admin command — the answer is that nothing
        // happened: no counter moves, no timestamp moves, no event. `DISABLED` has
        // no poll row of its own for the same reason, and any other completion of
        // the table would let a source the owner switched off keep alerting.
        (Quarantined | Disabled, _) => (current.clone(), None),
    }
}

/// `any | admin disable-source → DISABLED` (§8.1), from any state including
/// `DISABLED` itself.
///
/// Counters reset to 0 and `first_failure_at` clears with them: the pair must stay
/// coherent, and an outage that is no longer being polled has no start time worth
/// keeping. The §9 failure triple is kept — it is the last thing known about why the
/// source was switched off, and `admin` has no other record of it.
///
/// Returns the same shape as [`next`] so that "this row emits no event" is something
/// a caller and a test can assert rather than infer from the type.
#[must_use]
pub fn disable(current: &HealthSnapshot) -> (HealthSnapshot, Option<HealthEvent>) {
    (
        HealthSnapshot {
            health_state: HealthState::Disabled,
            consecutive_failures: 0,
            probe_attempts: 0,
            first_failure_at: None,
            ..current.clone()
        },
        None,
    )
}

/// `QUARANTINED, DISABLED | admin enable-source → INITIALIZING` (§8.1).
///
/// The source restarts from initialization with a clean slate: counters at 0, no
/// `first_failure_at`, no stale failure triple.
///
/// # Any other state is a no-op
///
/// The row's `From` column is part of the row. Forcing an already-running source
/// back to `INITIALIZING` would be actively harmful rather than merely redundant:
/// its `bootstrap_state` is already `complete`, so §8.1's second row would fire on
/// the very next poll and emit a **second** `SOURCE_BOOTSTRAPPED` for a source that
/// bootstrapped months ago. Re-enabling something that is already enabled changes
/// nothing.
#[must_use]
pub fn enable(current: &HealthSnapshot) -> (HealthSnapshot, Option<HealthEvent>) {
    if !matches!(
        current.health_state,
        HealthState::Quarantined | HealthState::Disabled
    ) {
        return (current.clone(), None);
    }

    (
        HealthSnapshot {
            health_state: HealthState::Initializing,
            failure_stage: None,
            failure_domain: None,
            failure_kind: None,
            consecutive_failures: 0,
            probe_attempts: 0,
            first_failure_at: None,
            ..current.clone()
        },
        None,
    )
}

/// The per-call inputs every §8.1 row shares, so that each row branch states only
/// what its columns actually say.
struct Poll<'a> {
    current: &'a HealthSnapshot,
    now: DateTime<Utc>,
    /// `consecutive_failures` **after** this poll's increment — the value §8.1's
    /// `cf` column and its `cf < 3` / `cf == 20` predicates are written in.
    /// Computed once so that the threshold tests and the stored counter can never
    /// disagree.
    cf_after: u32,
    failure: Option<(Stage, FaultDomain, FailureKind)>,
}

/// §8.1's `probe_attempts` column, which has three distinct values and not two.
enum Probe {
    /// The literal `0` — a spent probe budget is abandoned.
    Reset,
    /// The literal *unchanged* — notably every 429 row (§10.4).
    Unchanged,
    /// `+1` — a transient failure observed from `HEALTHY` or `DEGRADED` (§10.3).
    Increment,
}

/// Every success row: `cf` 0, `probe_attempts` 0, failure facts cleared.
///
/// All seven of them agree on those columns, so the branches above differ only in
/// the destination state and the event — which is exactly the decision §13.6 makes
/// interesting.
fn success_row(
    poll: &Poll<'_>,
    to: HealthState,
    event: Option<HealthEvent>,
) -> (HealthSnapshot, Option<HealthEvent>) {
    (
        HealthSnapshot {
            health_state: to,
            failure_stage: None,
            failure_domain: None,
            failure_kind: None,
            consecutive_failures: 0,
            probe_attempts: 0,
            first_failure_at: None,
            last_attempt_at: Some(poll.now),
            last_success_at: Some(poll.now),
            // §15 delivery policy, not a transition (§8's 6 h re-alert rule).
            last_health_alert_at: poll.current.last_health_alert_at,
        },
        event,
    )
}

/// Every failure row: `cf + 1`, the probe column as given, the outage start held.
///
/// The event — when the row has one — carries the post-update `first_failure_at`,
/// which for a consistent snapshot is the instant the outage began. That is what
/// gives `SOURCE_DEGRADED` and the `SOURCE_FAILED` that follows it, twenty polls
/// apart, one stable identity each (§13.2.3).
fn failure_row(
    poll: &Poll<'_>,
    to: HealthState,
    probe: Probe,
    event_type: Option<EventType>,
) -> (HealthSnapshot, Option<HealthEvent>) {
    let (failure_stage, failure_domain, failure_kind) = poll
        .failure
        .map_or((None, None, None), |(stage, domain, kind)| {
            (Some(stage), Some(domain), Some(kind))
        });

    let first_failure_at = poll.current.first_failure_at.or(Some(poll.now));

    let snapshot = HealthSnapshot {
        health_state: to,
        failure_stage,
        failure_domain,
        failure_kind,
        consecutive_failures: poll.cf_after,
        probe_attempts: match probe {
            Probe::Reset => 0,
            Probe::Unchanged => poll.current.probe_attempts,
            Probe::Increment => poll.current.probe_attempts.saturating_add(1),
        },
        first_failure_at,
        last_attempt_at: Some(poll.now),
        last_success_at: poll.current.last_success_at,
        last_health_alert_at: poll.current.last_health_alert_at,
    };

    let event = event_type.map(|event_type| HealthEvent {
        event_type,
        first_failure_at,
    });

    (snapshot, event)
}

/// `SOURCE_RECOVERED` for the outage described by `current`.
///
/// Reads `first_failure_at` **before** the success clears it. The event names the
/// outage that just ended, so its §13.2.3 key must be derivable from the outage's
/// own start instant; taking the post-clear `None` would give every recovery in the
/// system the same identity.
fn recovered(current: &HealthSnapshot) -> Option<HealthEvent> {
    Some(HealthEvent {
        event_type: EventType::SourceRecovered,
        first_failure_at: current.first_failure_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2026-08-17T10:06:04Z";
    /// When the outage in [`failing`] began — the value every event identity in
    /// these tests is keyed on.
    const OUTAGE_START: &str = "2026-08-17T10:05:07Z";
    const LAST_SUCCESS: &str = "2026-08-17T10:00:12Z";

    /// A representative of each §10.4 class, as the §9 triple a real caller passes.
    const TRANSIENT: (Stage, FaultDomain, FailureKind) =
        (Stage::Connect, FaultDomain::Upstream, FailureKind::Timeout);
    const HARD: (Stage, FaultDomain, FailureKind) = (
        Stage::Schema,
        FaultDomain::Adapter,
        FailureKind::RequiredFieldMissing,
    );
    const RATE: (Stage, FaultDomain, FailureKind) =
        (Stage::Http, FaultDomain::Upstream, FailureKind::RateLimited);

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .expect("the fixture timestamp is valid RFC 3339")
            .with_timezone(&Utc)
    }

    fn now() -> DateTime<Utc> {
        at(NOW)
    }

    /// A source in `state` that is not in an outage: no failures counted, no probes
    /// spent, no `first_failure_at`. This is §7's registration shape for
    /// `INITIALIZING` and the steady state for `HEALTHY`.
    fn clean(state: HealthState) -> HealthSnapshot {
        HealthSnapshot {
            health_state: state,
            failure_stage: None,
            failure_domain: None,
            failure_kind: None,
            consecutive_failures: 0,
            probe_attempts: 0,
            first_failure_at: None,
            last_attempt_at: None,
            last_success_at: None,
            last_health_alert_at: None,
        }
    }

    /// A source `cf` failures into an outage that began at [`OUTAGE_START`], with
    /// `probe_attempts` already spent and one earlier success on record.
    fn failing(state: HealthState, cf: u32, probe_attempts: u32) -> HealthSnapshot {
        HealthSnapshot {
            health_state: state,
            failure_stage: Some(TRANSIENT.0),
            failure_domain: Some(TRANSIENT.1),
            failure_kind: Some(TRANSIENT.2),
            consecutive_failures: cf,
            probe_attempts,
            first_failure_at: Some(at(OUTAGE_START)),
            last_attempt_at: Some(at(OUTAGE_START)),
            last_success_at: Some(at(LAST_SUCCESS)),
            last_health_alert_at: Some(at(OUTAGE_START)),
        }
    }

    /// The four columns §8.1 states for every row: destination, `cf`,
    /// `probe_attempts`, event.
    #[track_caller]
    fn assert_row(
        got: &(HealthSnapshot, Option<HealthEvent>),
        state: HealthState,
        cf: u32,
        probe_attempts: u32,
        event: Option<EventType>,
    ) {
        assert_eq!(got.0.health_state, state, "health_state");
        assert_eq!(got.0.consecutive_failures, cf, "cf");
        assert_eq!(got.0.probe_attempts, probe_attempts, "probe_attempts");
        assert_eq!(got.1.map(|e| e.event_type), event, "event");
    }

    /// §8.1's `first_failure_at` rule, stated as the invariant it implies: the
    /// timestamp exists exactly while an outage is being counted.
    #[track_caller]
    fn assert_outage_coherent(snapshot: &HealthSnapshot) {
        assert_eq!(
            snapshot.first_failure_at.is_some(),
            snapshot.consecutive_failures > 0,
            "first_failure_at must be set iff cf > 0 (§8.1): {snapshot:?}"
        );
    }

    // -----------------------------------------------------------------------
    // §8.1 rows 1–6 — INITIALIZING
    // -----------------------------------------------------------------------

    #[test]
    fn row_01_initializing_success_bootstrap_incomplete_stays_initializing() {
        let got = next(
            &clean(HealthState::Initializing),
            PollOutcome::Success,
            false,
            None,
            now(),
        );

        assert_row(&got, HealthState::Initializing, 0, 0, None);
    }

    #[test]
    fn row_02_initializing_success_bootstrap_complete_becomes_healthy() {
        let got = next(
            &clean(HealthState::Initializing),
            PollOutcome::Success,
            true,
            None,
            now(),
        );

        assert_row(
            &got,
            HealthState::Healthy,
            0,
            0,
            Some(EventType::SourceBootstrapped),
        );
        // §13.2.3 keys this type on `poll_seq`, which health does not own.
        assert_eq!(got.1.expect("row 2 emits an event").first_failure_at, None);
    }

    #[test]
    fn row_03_initializing_transient_below_limit_stays_initializing() {
        let got = next(
            &failing(HealthState::Initializing, 1, 0),
            PollOutcome::Transient,
            false,
            Some(TRANSIENT),
            now(),
        );

        // cf 2 < 3, and an initializing source never spends a probe (§10.3).
        assert_row(&got, HealthState::Initializing, 2, 0, None);
    }

    #[test]
    fn row_04_initializing_third_transient_fails() {
        let got = next(
            &failing(HealthState::Initializing, 2, 0),
            PollOutcome::Transient,
            false,
            Some(TRANSIENT),
            now(),
        );

        assert_row(
            &got,
            HealthState::Failed,
            3,
            0,
            Some(EventType::SourceFailed),
        );
    }

    #[test]
    fn row_05_initializing_rate_limited_degrades() {
        let got = next(
            &clean(HealthState::Initializing),
            PollOutcome::RateLimited,
            false,
            Some(RATE),
            now(),
        );

        assert_row(
            &got,
            HealthState::Degraded,
            1,
            0,
            Some(EventType::SourceDegraded),
        );
    }

    /// The second of the three v1.1 ambiguities §8.1 settles: a hard failure during
    /// initialization does **not** wait for the third failure.
    #[test]
    fn row_06_initializing_hard_fails_on_first_observation() {
        let got = next(
            &clean(HealthState::Initializing),
            PollOutcome::Hard,
            false,
            Some(HARD),
            now(),
        );

        assert_row(
            &got,
            HealthState::Failed,
            1,
            0,
            Some(EventType::SourceFailed),
        );
    }

    // -----------------------------------------------------------------------
    // §8.1 rows 7–10 — HEALTHY
    // -----------------------------------------------------------------------

    #[test]
    fn row_07_healthy_success_stays_healthy() {
        let got = next(
            &clean(HealthState::Healthy),
            PollOutcome::Success,
            true,
            None,
            now(),
        );

        assert_row(&got, HealthState::Healthy, 0, 0, None);
    }

    /// A 304 takes the same row. Asserted rather than assumed: treating a
    /// well-cached source as a failure would degrade an upstream behaving perfectly.
    #[test]
    fn row_07_healthy_not_modified_is_a_success() {
        let got = next(
            &clean(HealthState::Healthy),
            PollOutcome::NotModified,
            true,
            None,
            now(),
        );

        assert_row(&got, HealthState::Healthy, 0, 0, None);
    }

    #[test]
    fn row_08_healthy_transient_degrades_and_spends_probe_one() {
        let got = next(
            &clean(HealthState::Healthy),
            PollOutcome::Transient,
            true,
            Some(TRANSIENT),
            now(),
        );

        assert_row(
            &got,
            HealthState::Degraded,
            1,
            1,
            Some(EventType::SourceDegraded),
        );
    }

    /// The 429 row differs from the transient row above in exactly one column, and
    /// that column is the whole of §10.4's "never probe a rate limiter".
    #[test]
    fn row_09_healthy_rate_limited_degrades_without_spending_a_probe() {
        let got = next(
            &clean(HealthState::Healthy),
            PollOutcome::RateLimited,
            true,
            Some(RATE),
            now(),
        );

        assert_row(
            &got,
            HealthState::Degraded,
            1,
            0,
            Some(EventType::SourceDegraded),
        );
    }

    #[test]
    fn row_10_healthy_hard_fails_immediately() {
        let got = next(
            &clean(HealthState::Healthy),
            PollOutcome::Hard,
            true,
            Some(HARD),
            now(),
        );

        assert_row(
            &got,
            HealthState::Failed,
            1,
            0,
            Some(EventType::SourceFailed),
        );
    }

    // -----------------------------------------------------------------------
    // §8.1 rows 11–16 — DEGRADED
    // -----------------------------------------------------------------------

    #[test]
    fn row_11_degraded_success_bootstrap_incomplete_returns_to_initializing() {
        let got = next(
            &failing(HealthState::Degraded, 1, 1),
            PollOutcome::Success,
            false,
            None,
            now(),
        );

        assert_row(&got, HealthState::Initializing, 0, 0, None);
    }

    #[test]
    fn row_12_degraded_success_bootstrap_complete_recovers() {
        let got = next(
            &failing(HealthState::Degraded, 1, 1),
            PollOutcome::Success,
            true,
            None,
            now(),
        );

        assert_row(
            &got,
            HealthState::Healthy,
            0,
            0,
            Some(EventType::SourceRecovered),
        );
    }

    #[test]
    fn row_13_degraded_transient_fails_and_spends_probe_two() {
        let got = next(
            &failing(HealthState::Degraded, 1, 1),
            PollOutcome::Transient,
            true,
            Some(TRANSIENT),
            now(),
        );

        assert_row(
            &got,
            HealthState::Failed,
            2,
            2,
            Some(EventType::SourceFailed),
        );
    }

    /// `0`, not *unchanged*: the probe budget is abandoned, and §10.3 forbids a
    /// `FAILED` source starting a fresh two-probe sequence with it.
    #[test]
    fn row_14_degraded_hard_fails_and_resets_probes() {
        let got = next(
            &failing(HealthState::Degraded, 1, 1),
            PollOutcome::Hard,
            true,
            Some(HARD),
            now(),
        );

        assert_row(
            &got,
            HealthState::Failed,
            2,
            0,
            Some(EventType::SourceFailed),
        );
    }

    /// *Unchanged*, and this is the state where that is distinguishable from both
    /// `0` and `+1`: a `DEGRADED` source legitimately holds a spent probe.
    #[test]
    fn row_15_degraded_rate_limited_below_limit_stays_degraded_and_silent() {
        let got = next(
            &failing(HealthState::Degraded, 1, 1),
            PollOutcome::RateLimited,
            true,
            Some(RATE),
            now(),
        );

        assert_row(&got, HealthState::Degraded, 2, 1, None);
    }

    #[test]
    fn row_16_degraded_rate_limited_at_limit_quarantines() {
        let got = next(
            &failing(HealthState::Degraded, 19, 1),
            PollOutcome::RateLimited,
            true,
            Some(RATE),
            now(),
        );

        assert_row(
            &got,
            HealthState::Quarantined,
            20,
            1,
            Some(EventType::SourceQuarantined),
        );
    }

    // -----------------------------------------------------------------------
    // §8.1 rows 17–20 — FAILED
    // -----------------------------------------------------------------------

    #[test]
    fn row_17_failed_success_bootstrap_incomplete_returns_to_initializing() {
        let got = next(
            &failing(HealthState::Failed, 5, 2),
            PollOutcome::Success,
            false,
            None,
            now(),
        );

        assert_row(&got, HealthState::Initializing, 0, 0, None);
    }

    #[test]
    fn row_18_failed_success_bootstrap_complete_recovers() {
        let got = next(
            &failing(HealthState::Failed, 5, 2),
            PollOutcome::Success,
            true,
            None,
            now(),
        );

        assert_row(
            &got,
            HealthState::Healthy,
            0,
            0,
            Some(EventType::SourceRecovered),
        );
    }

    /// "Any failure" means all three classes, and none of them emits a new event —
    /// the 6 h re-alert re-sends the existing `SOURCE_FAILED` (§15), it does not
    /// mint a second one.
    #[test]
    fn row_19_failed_any_failure_below_limit_stays_failed_and_silent() {
        for (outcome, failure) in [
            (PollOutcome::Transient, TRANSIENT),
            (PollOutcome::Hard, HARD),
            (PollOutcome::RateLimited, RATE),
        ] {
            let got = next(
                &failing(HealthState::Failed, 5, 2),
                outcome,
                true,
                Some(failure),
                now(),
            );

            assert_row(&got, HealthState::Failed, 6, 2, None);
        }
    }

    #[test]
    fn row_20_failed_any_failure_at_limit_quarantines() {
        for (outcome, failure) in [
            (PollOutcome::Transient, TRANSIENT),
            (PollOutcome::Hard, HARD),
            (PollOutcome::RateLimited, RATE),
        ] {
            let got = next(
                &failing(HealthState::Failed, 19, 2),
                outcome,
                true,
                Some(failure),
                now(),
            );

            assert_row(
                &got,
                HealthState::Quarantined,
                20,
                2,
                Some(EventType::SourceQuarantined),
            );
        }
    }

    // -----------------------------------------------------------------------
    // §8.1 rows 21–23 — not polled, and the admin operations
    // -----------------------------------------------------------------------

    /// Every column of row 21 is a dash, so the whole snapshot must come back
    /// byte-identical — including the timestamps, because nothing was polled.
    #[test]
    fn row_21_quarantined_is_never_polled() {
        let quarantined = failing(HealthState::Quarantined, 20, 2);

        for (outcome, failure) in [
            (PollOutcome::Success, None),
            (PollOutcome::NotModified, None),
            (PollOutcome::Transient, Some(TRANSIENT)),
            (PollOutcome::Hard, Some(HARD)),
            (PollOutcome::RateLimited, Some(RATE)),
        ] {
            let got = next(&quarantined, outcome, true, failure, now());

            assert_row(&got, HealthState::Quarantined, 20, 2, None);
            assert_eq!(got.0, quarantined, "a quarantined source is not polled");
        }
    }

    /// `DISABLED` has no poll row in §8.1 because §8 stops polling there too. The
    /// completion must be the same no-op: a source the owner switched off cannot be
    /// made to alert by a stale GSI1 hint.
    #[test]
    fn disabled_is_never_polled_either() {
        let disabled = clean(HealthState::Disabled);

        let got = next(&disabled, PollOutcome::Hard, true, Some(HARD), now());

        assert_row(&got, HealthState::Disabled, 0, 0, None);
        assert_eq!(got.0, disabled);
    }

    #[test]
    fn row_22_disable_from_any_state() {
        for state in [
            HealthState::Initializing,
            HealthState::Healthy,
            HealthState::Degraded,
            HealthState::Failed,
            HealthState::Quarantined,
            HealthState::Disabled,
        ] {
            let got = disable(&failing(state, 7, 2));

            assert_row(&got, HealthState::Disabled, 0, 0, None);
            assert_outage_coherent(&got.0);
            // The last known fault survives; `admin` has no other record of it.
            assert_eq!(got.0.failure_kind, Some(FailureKind::Timeout));
        }
    }

    #[test]
    fn row_23_enable_from_quarantined_or_disabled_reinitializes() {
        for state in [HealthState::Quarantined, HealthState::Disabled] {
            let got = enable(&failing(state, 20, 2));

            assert_row(&got, HealthState::Initializing, 0, 0, None);
            assert_outage_coherent(&got.0);
            assert_eq!(got.0.failure_kind, None, "a re-enabled source starts clean");
        }
    }

    /// Re-enabling a running source must not restart it: `bootstrap_state` is
    /// already `complete`, so row 2 would fire on the next poll and emit a second
    /// `SOURCE_BOOTSTRAPPED` for a source that bootstrapped long ago.
    #[test]
    fn enable_is_a_no_op_for_a_source_that_is_already_enabled() {
        for state in [
            HealthState::Initializing,
            HealthState::Healthy,
            HealthState::Degraded,
            HealthState::Failed,
        ] {
            let running = failing(state, 3, 1);

            assert_eq!(enable(&running), (running.clone(), None));
        }
    }

    // -----------------------------------------------------------------------
    // first_failure_at — the event discriminator (§8.1, §13.2.3)
    // -----------------------------------------------------------------------

    #[test]
    fn first_failure_at_is_set_on_the_first_failure_of_an_outage() {
        let (snapshot, event) = next(
            &clean(HealthState::Healthy),
            PollOutcome::Transient,
            true,
            Some(TRANSIENT),
            now(),
        );

        assert_eq!(snapshot.first_failure_at, Some(now()));
        assert_eq!(
            event.expect("row 8 emits an event").first_failure_at,
            Some(now()),
            "the event is keyed on the instant the outage began"
        );
        assert_outage_coherent(&snapshot);
    }

    /// Every poll of one outage must derive the same key per event type, however
    /// many polls it spans (§13.2.3), which requires the timestamp to survive them.
    #[test]
    fn first_failure_at_is_preserved_for_the_length_of_the_outage() {
        let mut snapshot = clean(HealthState::Healthy);
        let mut clock = at("2026-08-17T10:05:07Z");

        for _ in 0..5 {
            snapshot = next(
                &snapshot,
                PollOutcome::Transient,
                true,
                Some(TRANSIENT),
                clock,
            )
            .0;
            clock += chrono::TimeDelta::minutes(1);

            assert_eq!(snapshot.first_failure_at, Some(at(OUTAGE_START)));
            assert_outage_coherent(&snapshot);
        }
    }

    #[test]
    fn first_failure_at_is_cleared_by_any_success() {
        for state in [
            HealthState::Initializing,
            HealthState::Degraded,
            HealthState::Failed,
        ] {
            for bootstrap_complete in [false, true] {
                let (snapshot, _) = next(
                    &failing(state, 4, 1),
                    PollOutcome::Success,
                    bootstrap_complete,
                    None,
                    now(),
                );

                assert_eq!(snapshot.first_failure_at, None);
                assert_outage_coherent(&snapshot);
            }
        }
    }

    /// The recovery event names the outage that just ended, so it must be built from
    /// the value the same call clears. Post-clear, every recovery in the system
    /// would share one identity.
    #[test]
    fn source_recovered_carries_the_pre_clear_first_failure_at() {
        for state in [HealthState::Degraded, HealthState::Failed] {
            let (snapshot, event) = next(
                &failing(state, 4, 1),
                PollOutcome::Success,
                true,
                None,
                now(),
            );
            let event = event.expect("a completed-bootstrap recovery emits an event");

            assert_eq!(event.event_type, EventType::SourceRecovered);
            assert_eq!(event.first_failure_at, Some(at(OUTAGE_START)));
            assert_eq!(snapshot.first_failure_at, None, "the snapshot still clears");
        }
    }

    /// A snapshot no sequence of calls here can produce, but one a bad migration or
    /// a hand edit could: `cf > 0` with no outage start. Inheriting the `None` would
    /// give this outage the same event key as every other outage's.
    #[test]
    fn a_missing_first_failure_at_is_repaired_rather_than_inherited() {
        let inconsistent = HealthSnapshot {
            consecutive_failures: 4,
            first_failure_at: None,
            ..failing(HealthState::Failed, 4, 1)
        };

        let (snapshot, _) = next(
            &inconsistent,
            PollOutcome::Transient,
            true,
            Some(TRANSIENT),
            now(),
        );

        assert_eq!(snapshot.first_failure_at, Some(now()));
        assert_outage_coherent(&snapshot);
    }

    // -----------------------------------------------------------------------
    // Bootstrap recovery (INV-10, §13.6, §30.2)
    // -----------------------------------------------------------------------

    /// §30.2: "A source that fails during bootstrap, then succeeds, returns to
    /// `INITIALIZING` and completes bootstrap without any `NEW_JOB` storm."
    ///
    /// The hazard is a recovered-but-unbootstrapped source being treated as
    /// `HEALTHY`, which would let the engine diff a full board against an empty
    /// index. `health` cannot cause that directly — it produces no job events at
    /// all — but returning `HEALTHY` here is what would authorize it.
    #[test]
    fn bootstrap_hazard_a_failed_initialization_recovers_to_initializing() {
        let mut emitted = Vec::new();
        let mut snapshot = clean(HealthState::Initializing);

        // A hard failure on the very first poll: row 6.
        let (next_snapshot, event) = next(&snapshot, PollOutcome::Hard, false, Some(HARD), now());
        snapshot = next_snapshot;
        emitted.extend(event);
        assert_eq!(snapshot.health_state, HealthState::Failed);

        // The upstream comes back, but the baseline load has not run: row 17.
        let (next_snapshot, event) = next(&snapshot, PollOutcome::Success, false, None, now());
        snapshot = next_snapshot;
        emitted.extend(event);

        assert_eq!(snapshot.health_state, HealthState::Initializing);
        assert_eq!(snapshot.consecutive_failures, 0);
        assert_eq!(snapshot.probe_attempts, 0);
        assert_eq!(snapshot.first_failure_at, None);
        assert_eq!(
            event, None,
            "recovering into bootstrap is not SOURCE_RECOVERED"
        );

        // Only the bootstrap commit may finish the job: row 2.
        let (snapshot, event) = next(&snapshot, PollOutcome::Success, true, None, now());
        emitted.extend(event);

        assert_eq!(snapshot.health_state, HealthState::Healthy);
        assert_eq!(
            event.map(|e| e.event_type),
            Some(EventType::SourceBootstrapped)
        );

        assert_eq!(
            emitted.iter().map(|e| e.event_type).collect::<Vec<_>>(),
            vec![EventType::SourceFailed, EventType::SourceBootstrapped],
            "no SOURCE_RECOVERED anywhere on this path"
        );
        assert_no_job_events(&emitted);
    }

    /// The same hazard reached through a 429 rather than a hard failure: §8.1 sends
    /// an initializing source that is rate-limited to `DEGRADED`, and the recovery
    /// row out of `DEGRADED` is the one that must not fire `SOURCE_RECOVERED`.
    #[test]
    fn bootstrap_hazard_a_rate_limited_initialization_recovers_to_initializing() {
        let (degraded, event) = next(
            &clean(HealthState::Initializing),
            PollOutcome::RateLimited,
            false,
            Some(RATE),
            now(),
        );
        assert_row(
            &(degraded.clone(), event),
            HealthState::Degraded,
            1,
            0,
            Some(EventType::SourceDegraded),
        );

        let (recovering, event) = next(&degraded, PollOutcome::Success, false, None, now());

        assert_row(
            &(recovering.clone(), event),
            HealthState::Initializing,
            0,
            0,
            None,
        );
        assert_eq!(recovering.first_failure_at, None);

        let (bootstrapped, event) = next(&recovering, PollOutcome::Success, true, None, now());

        assert_row(
            &(bootstrapped, event),
            HealthState::Healthy,
            0,
            0,
            Some(EventType::SourceBootstrapped),
        );
    }

    /// This module has no diff output and no job-event vocabulary; the assertion
    /// exists so that a future change which gives it one fails loudly here (INV-10).
    #[track_caller]
    fn assert_no_job_events(emitted: &[HealthEvent]) {
        for event in emitted {
            assert!(
                matches!(
                    event.event_type,
                    EventType::SourceBootstrapped
                        | EventType::SourceDegraded
                        | EventType::SourceFailed
                        | EventType::SourceRecovered
                        | EventType::SourceQuarantined
                ),
                "core::health emitted a non-health event: {event:?}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // 429 quarantine (INV-16, §30.2)
    // -----------------------------------------------------------------------

    /// §30.2: "A 25-poll run of pure 429 responses never reaches `FAILED` and
    /// reaches `QUARANTINED` at the 20th."
    #[test]
    fn twenty_five_consecutive_429s_quarantine_once_and_never_fail() {
        let mut snapshot = clean(HealthState::Healthy);
        let mut quarantined_at = Vec::new();
        let mut degraded_at = Vec::new();

        for poll in 1..=25_u32 {
            let (next_snapshot, event) =
                next(&snapshot, PollOutcome::RateLimited, true, Some(RATE), now());
            snapshot = next_snapshot;

            assert_ne!(
                snapshot.health_state,
                HealthState::Failed,
                "poll {poll}: rate limiting is the upstream working as designed"
            );
            assert_eq!(
                snapshot.probe_attempts, 0,
                "poll {poll}: a 429 never spends a probe (§10.4)"
            );

            match event.map(|e| e.event_type) {
                Some(EventType::SourceDegraded) => degraded_at.push(poll),
                Some(EventType::SourceQuarantined) => quarantined_at.push(poll),
                Some(other) => panic!("poll {poll}: unexpected event {other:?}"),
                None => {}
            }
        }

        assert_eq!(degraded_at, vec![1], "one notice on entry, then suppressed");
        assert_eq!(quarantined_at, vec![20], "one final message, exactly once");
        assert_eq!(snapshot.health_state, HealthState::Quarantined);
        // Polls 21–25 changed nothing at all: the counter is still the one that
        // quarantined it.
        assert_eq!(snapshot.consecutive_failures, 20);
        // The run began at `now()`, and every poll of it kept that instant — so the
        // `SOURCE_DEGRADED` of poll 1 and the `SOURCE_QUARANTINED` of poll 20 are
        // keyed on one outage (§13.2.3).
        assert_eq!(snapshot.first_failure_at, Some(now()));
    }

    /// The other route to the same place, and the only one that existed at v1.1:
    /// twenty failures through `FAILED`. `SOURCE_FAILED` fires once on entry; the
    /// eighteen polls between it and quarantine are silent here because their
    /// re-alert is a §15 delivery decision over the same durable event.
    #[test]
    fn twenty_consecutive_hard_failures_quarantine_through_failed() {
        let mut snapshot = clean(HealthState::Healthy);
        let mut events = Vec::new();

        for poll in 1..=20_u32 {
            let (next_snapshot, event) =
                next(&snapshot, PollOutcome::Hard, true, Some(HARD), now());
            snapshot = next_snapshot;

            assert_eq!(snapshot.consecutive_failures, poll);
            events.extend(event.map(|e| (poll, e.event_type)));
        }

        assert_eq!(
            events,
            vec![
                (1, EventType::SourceFailed),
                (20, EventType::SourceQuarantined),
            ]
        );
        assert_eq!(snapshot.health_state, HealthState::Quarantined);
    }

    /// Both quarantine events must be derivable from the outage that produced them,
    /// which is the same outage the `SOURCE_FAILED` or `SOURCE_DEGRADED` named.
    #[test]
    fn quarantine_events_carry_the_outage_start() {
        for (state, outcome, failure) in [
            (HealthState::Degraded, PollOutcome::RateLimited, RATE),
            (HealthState::Failed, PollOutcome::Hard, HARD),
        ] {
            let (_, event) = next(&failing(state, 19, 0), outcome, true, Some(failure), now());
            let event = event.expect("the 20th failure quarantines");

            assert_eq!(event.event_type, EventType::SourceQuarantined);
            assert_eq!(event.first_failure_at, Some(at(OUTAGE_START)));
        }
    }

    /// A counter that somehow starts past the threshold must still quarantine.
    /// Leaving it `FAILED` forever is the "silently forgotten" state INV-16 forbids.
    #[test]
    fn a_counter_past_the_threshold_still_quarantines() {
        let got = next(
            &failing(HealthState::Failed, 41, 2),
            PollOutcome::Hard,
            true,
            Some(HARD),
            now(),
        );

        assert_row(
            &got,
            HealthState::Quarantined,
            42,
            2,
            Some(EventType::SourceQuarantined),
        );
    }

    // -----------------------------------------------------------------------
    // Derived snapshot fields
    // -----------------------------------------------------------------------

    #[test]
    fn a_failing_poll_records_this_polls_failure_triple() {
        let (snapshot, _) = next(
            &clean(HealthState::Healthy),
            PollOutcome::Hard,
            true,
            Some(HARD),
            now(),
        );

        assert_eq!(snapshot.failure_stage, Some(Stage::Schema));
        assert_eq!(snapshot.failure_domain, Some(FaultDomain::Adapter));
        assert_eq!(
            snapshot.failure_kind,
            Some(FailureKind::RequiredFieldMissing)
        );
        assert_eq!(snapshot.last_attempt_at, Some(now()));
        assert_eq!(snapshot.last_success_at, None, "nothing succeeded");
    }

    #[test]
    fn a_success_clears_the_failure_triple_and_stamps_the_success() {
        let (snapshot, _) = next(
            &failing(HealthState::Failed, 5, 2),
            PollOutcome::Success,
            true,
            None,
            now(),
        );

        assert_eq!(snapshot.failure_stage, None);
        assert_eq!(snapshot.failure_domain, None);
        assert_eq!(snapshot.failure_kind, None);
        assert_eq!(snapshot.last_attempt_at, Some(now()));
        assert_eq!(snapshot.last_success_at, Some(now()));
    }

    /// Delivery is §15's, not a transition's: nothing here may advance the throttle
    /// clock, or the 6 h re-alert would silently reset on every poll.
    #[test]
    fn last_health_alert_at_is_never_touched() {
        let alerted = at(OUTAGE_START);

        for (outcome, failure) in [
            (PollOutcome::Success, None),
            (PollOutcome::Transient, Some(TRANSIENT)),
            (PollOutcome::Hard, Some(HARD)),
            (PollOutcome::RateLimited, Some(RATE)),
        ] {
            let (snapshot, _) = next(
                &failing(HealthState::Degraded, 1, 1),
                outcome,
                true,
                failure,
                now(),
            );

            assert_eq!(snapshot.last_health_alert_at, Some(alerted));
        }
    }

    // -----------------------------------------------------------------------
    // outcome_for (§10.4, INV-6, INV-11)
    // -----------------------------------------------------------------------

    #[test]
    fn transient_kinds_map_to_transient() {
        for kind in [
            FailureKind::Timeout,
            FailureKind::ConnectFailed,
            FailureKind::DnsFailed,
            FailureKind::TlsError,
            FailureKind::ServerError,
        ] {
            assert_eq!(outcome_for(kind), Some(PollOutcome::Transient), "{kind}");
        }
    }

    #[test]
    fn rate_limited_maps_to_its_own_class() {
        assert_eq!(
            outcome_for(FailureKind::RateLimited),
            Some(PollOutcome::RateLimited)
        );
    }

    #[test]
    fn hard_kinds_map_to_hard() {
        for kind in [
            FailureKind::NotFound,
            FailureKind::Gone,
            FailureKind::Forbidden,
            FailureKind::BotChallenge,
            FailureKind::AuthRequired,
            FailureKind::WrongMediaType,
            FailureKind::MalformedBody,
            FailureKind::EmptyBody,
            FailureKind::ParseFailed,
            FailureKind::RequiredFieldMissing,
            FailureKind::ArrayPathMissing,
            FailureKind::NormalizeFailed,
            FailureKind::PlausibilityFailed,
        ] {
            assert_eq!(outcome_for(kind), Some(PollOutcome::Hard), "{kind}");
        }
    }

    /// INV-11: a changed response shape is not a failure.
    #[test]
    fn shape_changed_is_not_a_source_failure() {
        assert_eq!(outcome_for(FailureKind::ShapeChanged), None);
    }

    /// §13.5: both of these mean *someone else already did the work*.
    #[test]
    fn success_signals_are_not_failures() {
        assert_eq!(outcome_for(FailureKind::LeaseContention), None);
        assert_eq!(outcome_for(FailureKind::DbConditionalCheckFailed), None);
    }

    /// INV-6: our own infrastructure being broken says nothing about the upstream,
    /// and charging it to the source would hide one system fault behind N source
    /// alerts.
    #[test]
    fn infra_kinds_never_reach_source_health() {
        for kind in [
            FailureKind::DbThrottled,
            FailureKind::DbAccessDenied,
            FailureKind::DbFailed,
            FailureKind::TickTimeout,
            FailureKind::ConfigInvalid,
            FailureKind::SecretUnavailable,
        ] {
            assert_eq!(outcome_for(kind), None, "{kind}");
        }
    }

    /// INV-6, stated almost verbatim: "a Telegram outage must not mark a source
    /// unhealthy".
    #[test]
    fn notify_kinds_never_reach_source_health() {
        for kind in [
            FailureKind::NotifySendFailed,
            FailureKind::NotifyRateLimited,
            FailureKind::NotifyAuthFailed,
        ] {
            assert_eq!(outcome_for(kind), None, "{kind}");
        }
    }

    /// INV-6 corollary: a failed S3 PUT degrades the archive subsystem and nothing
    /// else. The poll still succeeded.
    #[test]
    fn archive_failure_never_reaches_source_health() {
        assert_eq!(outcome_for(FailureKind::ArchivePutFailed), None);
    }
}
