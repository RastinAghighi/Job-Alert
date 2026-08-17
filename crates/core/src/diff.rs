//! `(JobIndex, Vec<NormalizedJob>) -> Vec<Transition>` (§13.3, INV-13).
//!
//! One poll's complete business decision about every job of one source: which
//! transitions fire, what each one writes, and which plain Phase-B writes happen
//! without an event at all.
//!
//! # At most one transition per job (INV-13)
//!
//! `TransactWriteItems` cannot target the same item twice in one transaction, and
//! §13.4 pairs each job mutation with its durable event inside one transaction. A
//! job that simultaneously reappears *and* newly matches the filter would
//! therefore need two `Update`s on one `JOB#` item, which is not expressible.
//! §13.3 resolves it by strict precedence — [`diff`] emits the
//! highest-precedence applicable event and every other changed fact rides along
//! in that transition's [`JobWrite`] and `after` block. Nothing is lost, because
//! a reposted job that is also relevant still notifies through `JOB_REPOSTED`.
//!
//! # Absence is computed, not accumulated (§13.8)
//!
//! Storing `last_seen_at` and an `absent_ticks` counter on every job on every poll
//! is ~864,000 writes/day at V1 scale, and a crashed increment double-counts on
//! retry. Instead a single sparse `absent_since_poll` marker holds the
//! `current_poll_seq` at which a present job first went missing, and removal is a
//! comparison against it rather than a counter. The steady state — an unchanged,
//! present, unmarked job — produces **no write of any kind**, which is what
//! `unchanged_present_jobs_never_write` pins across 100 polls.
//!
//! `current_poll_seq` is `stored_poll_seq + 1` and is stable across a crash and
//! retry, because META only advances in §13.4's Phase C commit marker. That is the
//! whole reason absence tracking is idempotent, so [`diff`] takes the value as an
//! argument and [`NonTransitionWrites::current_poll_seq`] hands the same one to the
//! repository rather than letting it recompute.
//!
//! # What this module deliberately does not do
//!
//! No chunking, no `ClientRequestToken`, no DynamoDB expressions: §13.4's
//! transaction protocol is the engine's and the repository's, and [`JobWrite`] is
//! the boundary that keeps persistence free of business rules (§17.3.1). Nor does
//! it build events — the full `Event` envelope is a Phase-3 type, and
//! [`Transition`] plus [`JobWrite`] is Phase 1's authoritative transition payload.

use crate::model::{
    EventType, ExternalId, FilterReclassify, JobFacts, JobIndex, JobState, JobWrite,
    NonTransitionWrites, NormalizedJob, Transition,
};
use crate::shape::content_hash;
use chrono::{DateTime, TimeDelta, Utc};
use std::collections::BTreeMap;

/// The `transition_seq` of a job's first transition. A stored job's next sequence
/// is always `stored + 1`, so `NEW_JOB` starting at 1 leaves 0 meaning
/// *never written*.
const FIRST_TRANSITION_SEQ: u64 = 1;

/// How long an inactive job survives before DynamoDB deletes it (§13.8, §16.1).
///
/// Set by `JOB_REMOVED` and cleared by a later `JOB_REPOSTED` in the same update
/// that reactivates the job — otherwise the TTL would delete a job that is back on
/// the board.
const INACTIVE_TTL: TimeDelta = TimeDelta::days(180);

/// Compares one poll's fetched jobs against the stored index (§13.3, §13.8,
/// §17.3.1).
///
/// `fetched` is each normalized job paired with the relevance decision
/// [`crate::filter::is_relevant`] already made for it. `content_hash` is **not** a
/// parameter: it is computed here with §21.1.1's encoder, so no caller can supply
/// one derived a different way and fabricate or hide a `JOB_UPDATED`.
///
/// Returns transitions sorted by [`ExternalId`] byte order, which is what §13.4
/// requires before chunking.
///
/// # Precedence (§13.3)
///
/// At most one transition per job per poll, chosen in this order:
///
/// 1. `JOB_REPOSTED` — present, stored state inactive
/// 2. `NEW_JOB` — no stored entry
/// 3. `BECAME_RELEVANT` — relevance false → true
/// 4. `BECAME_IRRELEVANT` — relevance true → false
/// 5. `JOB_UPDATED` — `content_hash` changed and nothing above applies
/// 6. `JOB_REMOVED` — absent, and `current_poll_seq - absent_since_poll >= 1`
///
/// The first two are mutually exclusive: one needs a stored entry, the other needs
/// there to be none.
///
/// # Filter-version suppression (§21.3, INV-15)
///
/// When a job's stored `filter_version` differs from `current_filter_version`, its
/// relevance was computed under different code and a change in the flag is not
/// evidence that the world changed. `BECAME_RELEVANT` and `BECAME_IRRELEVANT` are
/// therefore suppressed for that job. The new relevance is never dropped: it rides
/// in whatever other transition fires, and if none does the job lands in
/// [`NonTransitionWrites::filter_reclassify`] as a plain write with no event.
/// Without this, editing the filter fabricates hundreds of `BECAME_RELEVANT`
/// alerts for pre-existing jobs and buries the channel.
///
/// # Panics
///
/// If a stored `transition_seq` is `u64::MAX`. §17.3.1 forbids saturating or
/// wrapping a sequence number — reusing one would mint a duplicate event key and
/// break INV-2 — and this signature is canonical and returns no `Result`, so the
/// only remaining option is to refuse loudly. Reaching it requires a corrupted
/// stored value, not a long-lived job.
#[must_use]
pub fn diff(
    index: &JobIndex,
    fetched: &[(NormalizedJob, bool)],
    now: DateTime<Utc>,
    current_poll_seq: u64,
    current_filter_version: u32,
) -> (Vec<Transition>, NonTransitionWrites) {
    // Keyed so the absence scan below can ask "was this id in the response?" and
    // so a duplicated external id in one response collapses to one decision rather
    // than two conflicting writes on one item.
    let present: BTreeMap<&ExternalId, (&NormalizedJob, bool)> = fetched
        .iter()
        .map(|(job, relevant)| (&job.external_id, (job, *relevant)))
        .collect();

    let mut transitions = Vec::new();
    let mut writes = NonTransitionWrites {
        current_poll_seq,
        absence_markers: Vec::new(),
        absence_clears: Vec::new(),
        filter_reclassify: Vec::new(),
    };

    for (&external_id, &(job, relevant)) in &present {
        match index.get(external_id) {
            None => transitions.push(new_job(job, relevant, now, current_filter_version)),
            Some(stored) => present_existing_job(
                job,
                relevant,
                stored,
                now,
                current_filter_version,
                &mut transitions,
                &mut writes,
            ),
        }
    }

    for (external_id, stored) in index.iter() {
        if present.contains_key(external_id) {
            continue;
        }
        absent_stored_job(
            external_id,
            stored,
            now,
            current_poll_seq,
            &mut transitions,
            &mut writes,
        );
    }

    // Present jobs and absent ones are found by two separate passes, so neither
    // pass's own ordering makes the concatenation sorted. §13.4 sorts before
    // chunking; doing it here means no caller can forget to.
    transitions.sort_by(|left, right| left.external_id.cmp(&right.external_id));

    (transitions, writes)
}

/// Precedence 2: an id that is not in the stored index at all (§13.3).
///
/// `first_seen_at == last_seen_at == now`, `bootstrapped = false` and no TTL —
/// `bootstrapped` is false because §13.6's baseline load is not this path, and a
/// brand-new job has nothing to expire.
fn new_job(
    job: &NormalizedJob,
    relevant: bool,
    now: DateTime<Utc>,
    current_filter_version: u32,
) -> Transition {
    let hash = content_hash(job);

    Transition {
        external_id: job.external_id.clone(),
        event_type: EventType::NewJob,
        // `None` is what lets §13.4's conditional write choose
        // `attribute_not_exists(SK)` over `transition_seq = :old`.
        prev_transition_seq: None,
        new_transition_seq: FIRST_TRANSITION_SEQ,
        before: None,
        after: JobFacts {
            state: JobState::Active,
            relevant,
            content_hash: hash.clone(),
            transition_seq: FIRST_TRANSITION_SEQ,
            absent_since_poll: None,
            filter_version: current_filter_version,
            first_seen_at: now,
            last_seen_at: now,
            bootstrapped: false,
            ttl: None,
        },
        job_write: JobWrite::PutNew {
            job: job.clone(),
            relevant,
            content_hash: hash,
            first_seen_at: now,
            last_seen_at: now,
            transition_seq: FIRST_TRANSITION_SEQ,
            filter_version: current_filter_version,
            bootstrapped: false,
        },
        notify_worthy: EventType::NewJob.notify_worthy(relevant),
    }
}

/// Precedences 1, 3, 4 and 5, plus the two write-only outcomes for a job that is
/// in the response and already stored.
fn present_existing_job(
    job: &NormalizedJob,
    relevant: bool,
    stored: &JobFacts,
    now: DateTime<Utc>,
    current_filter_version: u32,
    transitions: &mut Vec<Transition>,
    writes: &mut NonTransitionWrites,
) {
    let hash = content_hash(job);
    // §21.3: under a version mismatch a flipped `relevant` cannot be told apart
    // from a reclassification, so neither relevance event may fire for this job.
    let reclassified = stored.filter_version != current_filter_version;

    let event_type = if stored.state == JobState::Inactive {
        Some(EventType::JobReposted)
    } else if !reclassified && !stored.relevant && relevant {
        Some(EventType::BecameRelevant)
    } else if !reclassified && stored.relevant && !relevant {
        Some(EventType::BecameIrrelevant)
    } else if stored.content_hash != hash {
        Some(EventType::JobUpdated)
    } else {
        None
    };

    let Some(event_type) = event_type else {
        // No event, so any pending fact has to be carried by a plain Phase-B
        // write. Both can apply at once — Phase B is not a transaction, so unlike
        // Phase A it may touch one item twice (INV-13 constrains only Phase A).
        if stored.absent_since_poll.is_some() {
            writes.absence_clears.push(job.external_id.clone());
        }
        if reclassified {
            writes.filter_reclassify.push(FilterReclassify {
                external_id: job.external_id.clone(),
                relevant,
                filter_version: current_filter_version,
            });
        }
        return;
    };

    let new_transition_seq = next_transition_seq(stored.transition_seq);

    transitions.push(Transition {
        external_id: job.external_id.clone(),
        event_type,
        prev_transition_seq: Some(stored.transition_seq),
        new_transition_seq,
        before: Some(stored.clone()),
        // §17.3.1: every present-job transition preserves `first_seen_at` and
        // `bootstrapped`, and carries the *current* relevance, content and filter
        // version whether or not those are what triggered the event. That is how
        // §13.3's collapsed facts still reach storage, and how a reclassification
        // rides along instead of needing its own write.
        after: JobFacts {
            state: JobState::Active,
            relevant,
            content_hash: hash.clone(),
            transition_seq: new_transition_seq,
            absent_since_poll: None,
            filter_version: current_filter_version,
            first_seen_at: stored.first_seen_at,
            last_seen_at: now,
            bootstrapped: stored.bootstrapped,
            ttl: None,
        },
        job_write: JobWrite::UpdateActive {
            job: job.clone(),
            relevant,
            content_hash: hash,
            last_seen_at: now,
            transition_seq: new_transition_seq,
            filter_version: current_filter_version,
            // Both are read off the stored facts rather than off `event_type`:
            // the repository must never infer business semantics from the event
            // (§17.3.1). For `JOB_REPOSTED` both are true, which is what clears
            // the original absence marker and the inactive TTL in the same update
            // that reactivates the job — without the TTL clear, DynamoDB deletes
            // the job we just brought back (§16.1).
            clear_absent_since_poll: stored.absent_since_poll.is_some(),
            clear_ttl: stored.ttl.is_some(),
        },
        notify_worthy: event_type.notify_worthy(relevant),
    });
}

/// Precedence 6 and the absence marker, for a stored id missing from the response
/// (§13.8).
fn absent_stored_job(
    external_id: &ExternalId,
    stored: &JobFacts,
    now: DateTime<Utc>,
    current_poll_seq: u64,
    transitions: &mut Vec<Transition>,
    writes: &mut NonTransitionWrites,
) {
    // §13.8's last line: once the stored state is inactive, continued absence
    // causes no further write at all. Without it the marker would be recreated on
    // every poll for the rest of the job's TTL.
    if stored.state == JobState::Inactive {
        return;
    }

    let Some(absent_since_poll) = stored.absent_since_poll else {
        writes.absence_markers.push(external_id.clone());
        return;
    };

    // §13.8's `current_poll_seq - absent_since_poll >= 1`, written as a comparison
    // so a marker somehow ahead of the current poll cannot underflow. Equality is
    // the crash-retry case — the marker was written by this same poll number
    // before the crash — and correctly produces no second write.
    if current_poll_seq <= absent_since_poll {
        return;
    }

    let new_transition_seq = next_transition_seq(stored.transition_seq);
    let ttl = now + INACTIVE_TTL;

    transitions.push(Transition {
        external_id: external_id.clone(),
        event_type: EventType::JobRemoved,
        prev_transition_seq: Some(stored.transition_seq),
        new_transition_seq,
        before: Some(stored.clone()),
        after: JobFacts {
            state: JobState::Inactive,
            // There is no fetched job, so relevance, content and filter version
            // cannot be recomputed and are carried through unchanged.
            relevant: stored.relevant,
            content_hash: stored.content_hash.clone(),
            transition_seq: new_transition_seq,
            // RETAINED, not cleared and not refreshed: §13.8 keeps the original
            // marker for auditability, and it is what a later `JOB_REPOSTED`
            // clears.
            absent_since_poll: Some(absent_since_poll),
            filter_version: stored.filter_version,
            first_seen_at: stored.first_seen_at,
            // Deliberately *not* `now`. The job was not seen this poll, and
            // overwriting the real last-seen time would erase the only record of
            // when it was actually on the board.
            last_seen_at: stored.last_seen_at,
            bootstrapped: stored.bootstrapped,
            ttl: Some(ttl),
        },
        job_write: JobWrite::MarkInactive {
            transition_seq: new_transition_seq,
            absent_since_poll,
            ttl,
        },
        notify_worthy: EventType::JobRemoved.notify_worthy(stored.relevant),
    });
}

/// `stored + 1`, refusing to wrap or saturate.
///
/// §17.3.1 forbids both for sequence numbers: a reused `transition_seq` mints a
/// duplicate event key for a distinct logical transition, which is an INV-2
/// violation that surfaces nowhere. See [`diff`]'s panic note for why this is not
/// a `Result`.
fn next_transition_seq(stored: u64) -> u64 {
    stored.checked_add(1).expect(
        "transition_seq overflowed u64; §17.3.1 forbids wrapping or saturating it because a \
         reused sequence number breaks INV-2 event identity",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_key::job_event_key;
    use crate::model::{CountryClass, EmploymentType};
    use jobmon_errors::SourceId;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    const FILTER_VERSION: u32 = 3;
    const POLL: u64 = 42;
    const STORED_SEQ: u64 = 7;
    const NOW: &str = "2026-08-17T10:00:00Z";
    const FIRST_SEEN: &str = "2026-05-01T08:30:00Z";
    const LAST_SEEN: &str = "2026-08-16T09:00:00Z";

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .expect("the fixture timestamp is valid RFC 3339")
            .with_timezone(&Utc)
    }

    fn now() -> DateTime<Utc> {
        at(NOW)
    }

    fn id(raw: &str) -> ExternalId {
        ExternalId::new(raw).expect("a plain upstream id is valid")
    }

    fn job(external_id: &str) -> NormalizedJob {
        titled(external_id, "Software Engineering Intern")
    }

    /// A second fixture differing only in `title`, so `content_hash` moves and
    /// nothing else does.
    fn titled(external_id: &str, title: &str) -> NormalizedJob {
        NormalizedJob {
            external_id: id(external_id),
            title: title.to_owned(),
            location_raw: "Toronto, ON".to_owned(),
            country: Some(CountryClass::Ca),
            region: Some("ON".to_owned()),
            city: Some("Toronto".to_owned()),
            remote: false,
            employment_type: EmploymentType::Internship,
            url: "https://example.invalid/jobs/1".to_owned(),
            posted_at: None,
        }
    }

    /// Stored facts describing `job` exactly as it was last written: active,
    /// unmarked, no TTL, current filter version. Every test below starts here and
    /// mutates the one field its case is about.
    fn stored(job: &NormalizedJob, relevant: bool) -> JobFacts {
        JobFacts {
            state: JobState::Active,
            relevant,
            content_hash: content_hash(job),
            transition_seq: STORED_SEQ,
            absent_since_poll: None,
            filter_version: FILTER_VERSION,
            first_seen_at: at(FIRST_SEEN),
            last_seen_at: at(LAST_SEEN),
            bootstrapped: false,
            ttl: None,
        }
    }

    fn index_of(entries: impl IntoIterator<Item = (ExternalId, JobFacts)>) -> JobIndex {
        let mut index = JobIndex::new();
        for (external_id, facts) in entries {
            index.insert(external_id, facts);
        }
        index
    }

    /// Runs one poll at [`POLL`] and [`FILTER_VERSION`].
    fn run(
        index: &JobIndex,
        fetched: &[(NormalizedJob, bool)],
    ) -> (Vec<Transition>, NonTransitionWrites) {
        diff(index, fetched, now(), POLL, FILTER_VERSION)
    }

    fn only(transitions: &[Transition]) -> &Transition {
        assert_eq!(
            transitions.len(),
            1,
            "INV-13 allows at most one transition per job per poll"
        );
        &transitions[0]
    }

    fn assert_no_plain_writes(writes: &NonTransitionWrites) {
        assert!(writes.absence_markers.is_empty(), "unexpected marker");
        assert!(writes.absence_clears.is_empty(), "unexpected clear");
        assert!(
            writes.filter_reclassify.is_empty(),
            "unexpected reclassification"
        );
    }

    // -----------------------------------------------------------------------
    // The six transitions, one test each (§13.3)
    // -----------------------------------------------------------------------

    #[test]
    fn new_job_fires_for_an_id_absent_from_the_index() {
        let fetched = vec![(job("a"), true)];

        let (transitions, writes) = run(&JobIndex::new(), &fetched);

        let transition = only(&transitions);
        assert_eq!(transition.event_type, EventType::NewJob);
        assert_eq!(transition.prev_transition_seq, None);
        assert_eq!(transition.new_transition_seq, FIRST_TRANSITION_SEQ);
        assert!(transition.before.is_none());
        assert!(transition.notify_worthy, "§14 notifies a relevant NEW_JOB");
        assert_no_plain_writes(&writes);
    }

    #[test]
    fn job_reposted_fires_when_a_present_job_was_stored_inactive() {
        let posting = job("a");
        let mut facts = stored(&posting, true);
        facts.state = JobState::Inactive;
        facts.absent_since_poll = Some(POLL - 4);
        facts.ttl = Some(at(FIRST_SEEN) + INACTIVE_TTL);
        let index = index_of([(id("a"), facts)]);

        let (transitions, writes) = run(&index, &[(posting, true)]);

        let transition = only(&transitions);
        assert_eq!(transition.event_type, EventType::JobReposted);
        assert_eq!(transition.prev_transition_seq, Some(STORED_SEQ));
        assert_eq!(transition.new_transition_seq, STORED_SEQ + 1);
        assert!(
            transition.notify_worthy,
            "§14 notifies a relevant JOB_REPOSTED"
        );
        assert_no_plain_writes(&writes);
    }

    #[test]
    fn became_relevant_fires_when_relevance_goes_false_to_true() {
        let posting = job("a");
        let index = index_of([(id("a"), stored(&posting, false))]);

        let (transitions, writes) = run(&index, &[(posting, true)]);

        let transition = only(&transitions);
        assert_eq!(transition.event_type, EventType::BecameRelevant);
        assert!(
            transition.notify_worthy,
            "§14 notifies BECAME_RELEVANT unconditionally"
        );
        assert!(transition.after.relevant);
        assert_no_plain_writes(&writes);
    }

    #[test]
    fn became_irrelevant_fires_when_relevance_goes_true_to_false() {
        let posting = job("a");
        let index = index_of([(id("a"), stored(&posting, true))]);

        let (transitions, writes) = run(&index, &[(posting, false)]);

        let transition = only(&transitions);
        assert_eq!(transition.event_type, EventType::BecameIrrelevant);
        assert!(!transition.notify_worthy, "§14 does not notify it");
        assert!(!transition.after.relevant);
        assert_no_plain_writes(&writes);
    }

    #[test]
    fn job_updated_fires_when_only_the_content_hash_moved() {
        let index = index_of([(id("a"), stored(&job("a"), true))]);
        let edited = titled("a", "Software Engineering Intern (Fall 2026)");

        let (transitions, writes) = run(&index, &[(edited.clone(), true)]);

        let transition = only(&transitions);
        assert_eq!(transition.event_type, EventType::JobUpdated);
        assert!(!transition.notify_worthy, "§14 does not notify it");
        assert_eq!(transition.after.content_hash, content_hash(&edited));
        assert_no_plain_writes(&writes);
    }

    #[test]
    fn job_removed_fires_one_poll_after_the_marker() {
        let posting = job("a");
        let mut facts = stored(&posting, true);
        facts.absent_since_poll = Some(POLL - 1);
        let index = index_of([(id("a"), facts)]);

        let (transitions, writes) = run(&index, &[]);

        let transition = only(&transitions);
        assert_eq!(transition.event_type, EventType::JobRemoved);
        assert_eq!(transition.prev_transition_seq, Some(STORED_SEQ));
        assert_eq!(transition.new_transition_seq, STORED_SEQ + 1);
        assert!(
            !transition.notify_worthy,
            "§14 does not notify JOB_REMOVED even for a relevant job"
        );
        assert_no_plain_writes(&writes);
    }

    // -----------------------------------------------------------------------
    // Exact canonical shapes (§17.3.1, §30.2)
    // -----------------------------------------------------------------------

    #[test]
    fn new_job_writes_the_exact_canonical_facts() {
        let posting = job("a");
        let hash = content_hash(&posting);

        let (transitions, _) = run(&JobIndex::new(), &[(posting.clone(), true)]);

        let transition = only(&transitions);
        assert_eq!(
            transition.after,
            JobFacts {
                state: JobState::Active,
                relevant: true,
                content_hash: hash.clone(),
                transition_seq: 1,
                absent_since_poll: None,
                filter_version: FILTER_VERSION,
                first_seen_at: now(),
                last_seen_at: now(),
                bootstrapped: false,
                ttl: None,
            }
        );
        assert_eq!(
            transition.job_write,
            JobWrite::PutNew {
                job: posting,
                relevant: true,
                content_hash: hash,
                first_seen_at: now(),
                last_seen_at: now(),
                transition_seq: 1,
                filter_version: FILTER_VERSION,
                bootstrapped: false,
            }
        );
    }

    /// §30.2: `JOB_REPOSTED` clears the absence marker **and** the inactive TTL.
    /// Missing the TTL clear means DynamoDB deletes the job that just came back.
    #[test]
    fn job_reposted_writes_update_active_clearing_the_marker_and_the_ttl() {
        let posting = job("a");
        let hash = content_hash(&posting);
        let mut facts = stored(&posting, true);
        facts.state = JobState::Inactive;
        facts.absent_since_poll = Some(POLL - 4);
        facts.ttl = Some(at(LAST_SEEN) + INACTIVE_TTL);
        facts.bootstrapped = true;
        let index = index_of([(id("a"), facts)]);

        let (transitions, _) = run(&index, &[(posting.clone(), true)]);

        let transition = only(&transitions);
        assert_eq!(
            transition.job_write,
            JobWrite::UpdateActive {
                job: posting,
                relevant: true,
                content_hash: hash.clone(),
                last_seen_at: now(),
                transition_seq: STORED_SEQ + 1,
                filter_version: FILTER_VERSION,
                clear_absent_since_poll: true,
                clear_ttl: true,
            }
        );
        assert_eq!(
            transition.after,
            JobFacts {
                state: JobState::Active,
                relevant: true,
                content_hash: hash,
                transition_seq: STORED_SEQ + 1,
                absent_since_poll: None,
                filter_version: FILTER_VERSION,
                // Preserved from the stored facts, not reset.
                first_seen_at: at(FIRST_SEEN),
                last_seen_at: now(),
                bootstrapped: true,
                ttl: None,
            }
        );
    }

    /// §30.2 and §13.8: the marker is **retained**, `last_seen_at` is **not**
    /// advanced, and the TTL is exactly `now + 180 days`.
    #[test]
    fn job_removed_retains_the_marker_preserves_last_seen_at_and_sets_a_180_day_ttl() {
        let posting = job("a");
        let hash = content_hash(&posting);
        let mut facts = stored(&posting, true);
        facts.absent_since_poll = Some(POLL - 1);
        facts.bootstrapped = true;
        let index = index_of([(id("a"), facts)]);

        let (transitions, _) = run(&index, &[]);

        let transition = only(&transitions);
        let expected_ttl = at("2027-02-13T10:00:00Z");
        assert_eq!(
            expected_ttl,
            now() + TimeDelta::days(180),
            "the fixture states the 180-day horizon independently of the constant"
        );
        assert_eq!(
            transition.after,
            JobFacts {
                state: JobState::Inactive,
                relevant: true,
                content_hash: hash,
                transition_seq: STORED_SEQ + 1,
                absent_since_poll: Some(POLL - 1),
                filter_version: FILTER_VERSION,
                first_seen_at: at(FIRST_SEEN),
                last_seen_at: at(LAST_SEEN),
                bootstrapped: true,
                ttl: Some(expected_ttl),
            }
        );
        assert_eq!(
            transition.job_write,
            JobWrite::MarkInactive {
                transition_seq: STORED_SEQ + 1,
                absent_since_poll: POLL - 1,
                ttl: expected_ttl,
            }
        );
    }

    // -----------------------------------------------------------------------
    // Absence tracking (§13.8)
    // -----------------------------------------------------------------------

    #[test]
    fn a_newly_absent_active_job_gets_one_marker_carrying_the_current_poll_seq() {
        let index = index_of([(id("a"), stored(&job("a"), true))]);

        let (transitions, writes) = run(&index, &[]);

        assert!(transitions.is_empty(), "absence alone is not a transition");
        assert_eq!(writes.absence_markers, vec![id("a")]);
        assert_eq!(
            writes.current_poll_seq, POLL,
            "the repository writes this value rather than recomputing it (§13.4)"
        );
        assert!(writes.absence_clears.is_empty());
    }

    /// §13.8's last line. Without it the marker is recreated on every poll for the
    /// whole 180-day TTL, which is exactly the write amplification the sparse
    /// marker exists to avoid.
    #[test]
    fn continued_absence_of_an_inactive_job_writes_nothing_at_all() {
        let posting = job("a");
        let mut facts = stored(&posting, true);
        facts.state = JobState::Inactive;
        facts.absent_since_poll = Some(POLL - 5);
        facts.ttl = Some(at(LAST_SEEN) + INACTIVE_TTL);
        let index = index_of([(id("a"), facts)]);

        let (transitions, writes) = run(&index, &[]);

        assert!(transitions.is_empty());
        assert_no_plain_writes(&writes);
    }

    /// A job that went missing for one poll and came back before the removal
    /// threshold: still active, so no `JOB_REPOSTED`, and the only thing to do is
    /// drop the marker.
    #[test]
    fn a_returning_unchanged_job_produces_one_absence_clear_and_nothing_else() {
        let posting = job("a");
        let mut facts = stored(&posting, true);
        facts.absent_since_poll = Some(POLL - 1);
        let index = index_of([(id("a"), facts)]);

        let (transitions, writes) = run(&index, &[(posting, true)]);

        assert!(transitions.is_empty());
        assert_eq!(writes.absence_clears, vec![id("a")]);
        assert!(writes.absence_markers.is_empty());
        assert!(writes.filter_reclassify.is_empty());
    }

    /// The steady state, which §13.8 requires to cost nothing: 100 polls of an
    /// unchanged, present, unmarked job produce no transition and no write.
    #[test]
    fn unchanged_present_jobs_never_write() {
        let posting = job("a");
        let index = index_of([(id("a"), stored(&posting, true))]);
        let fetched = vec![(posting, true)];

        for poll in POLL..POLL + 100 {
            let (transitions, writes) = diff(&index, &fetched, now(), poll, FILTER_VERSION);
            assert!(transitions.is_empty(), "poll {poll} produced a transition");
            assert_no_plain_writes(&writes);
        }
    }

    // -----------------------------------------------------------------------
    // Precedence (§13.3, INV-13)
    // -----------------------------------------------------------------------

    /// `JOB_REPOSTED` outranks `BECAME_RELEVANT`, and nothing is lost: the new
    /// relevance rides in the same `after` block and job write, and §14 notifies a
    /// relevant repost anyway.
    #[test]
    fn a_repost_that_is_also_newly_relevant_emits_only_job_reposted() {
        let posting = job("a");
        let mut facts = stored(&posting, false);
        facts.state = JobState::Inactive;
        facts.absent_since_poll = Some(POLL - 9);
        facts.ttl = Some(at(LAST_SEEN) + INACTIVE_TTL);
        let index = index_of([(id("a"), facts)]);

        let (transitions, writes) = run(&index, &[(posting, true)]);

        let transition = only(&transitions);
        assert_eq!(transition.event_type, EventType::JobReposted);
        assert!(
            transition.after.relevant,
            "the collapsed relevance change still reaches storage"
        );
        assert!(transition.notify_worthy);
        assert_no_plain_writes(&writes);
    }

    /// `BECAME_IRRELEVANT` outranks `JOB_UPDATED`; the content change rides in the
    /// same write.
    #[test]
    fn a_became_irrelevant_job_whose_content_also_changed_emits_only_became_irrelevant() {
        let index = index_of([(id("a"), stored(&job("a"), true))]);
        let edited = titled("a", "Senior Software Engineer");

        let (transitions, writes) = run(&index, &[(edited.clone(), false)]);

        let transition = only(&transitions);
        assert_eq!(transition.event_type, EventType::BecameIrrelevant);
        assert_eq!(
            transition.after.content_hash,
            content_hash(&edited),
            "the collapsed content change still reaches storage"
        );
        assert_no_plain_writes(&writes);
    }

    // -----------------------------------------------------------------------
    // Filter versioning (§21.3, INV-15)
    // -----------------------------------------------------------------------

    #[test]
    fn a_filter_version_bump_suppresses_the_relevance_event_and_routes_a_reclassify() {
        let posting = job("a");
        let mut facts = stored(&posting, false);
        facts.filter_version = FILTER_VERSION - 1;
        let index = index_of([(id("a"), facts)]);

        let (transitions, writes) = run(&index, &[(posting, true)]);

        assert!(
            transitions.is_empty(),
            "INV-15: editing the filter must not fabricate BECAME_RELEVANT"
        );
        assert_eq!(
            writes.filter_reclassify,
            vec![FilterReclassify {
                external_id: id("a"),
                relevant: true,
                filter_version: FILTER_VERSION,
            }]
        );
        assert!(writes.absence_markers.is_empty());
        assert!(writes.absence_clears.is_empty());
    }

    /// Suppression removes the *event*, never the fact. When another transition
    /// fires anyway, the new relevance and version ride in it and no separate
    /// reclassification write is needed.
    #[test]
    fn a_reclassified_job_whose_content_changed_carries_the_new_relevance_in_job_updated() {
        let posting = job("a");
        let mut facts = stored(&posting, false);
        facts.filter_version = FILTER_VERSION - 1;
        let index = index_of([(id("a"), facts)]);
        let edited = titled("a", "Software Engineering Intern (Winter 2027)");

        let (transitions, writes) = run(&index, &[(edited, true)]);

        let transition = only(&transitions);
        assert_eq!(transition.event_type, EventType::JobUpdated);
        assert!(transition.after.relevant);
        assert_eq!(transition.after.filter_version, FILTER_VERSION);
        match &transition.job_write {
            JobWrite::UpdateActive {
                relevant,
                filter_version,
                ..
            } => {
                assert!(*relevant);
                assert_eq!(*filter_version, FILTER_VERSION);
            }
            other => panic!("a present-job transition writes UpdateActive, got {other:?}"),
        }
        assert!(
            writes.filter_reclassify.is_empty(),
            "the transition already carries it"
        );
    }

    // -----------------------------------------------------------------------
    // Ordering (§13.4) and identity (INV-2)
    // -----------------------------------------------------------------------

    /// §13.4 sorts transitions by external id before chunking. The two ids that
    /// sort between the fetched ones come from the *absent* pass, so this also
    /// pins that the two passes are merged rather than concatenated.
    #[test]
    fn transitions_are_sorted_by_external_id_byte_order() {
        let mut removed_a = stored(&job("a"), true);
        removed_a.absent_since_poll = Some(POLL - 1);
        let mut removed_c = stored(&job("c"), true);
        removed_c.absent_since_poll = Some(POLL - 1);
        let index = index_of([(id("a"), removed_a), (id("c"), removed_c)]);

        // Deliberately not in order, and neither id is first.
        let fetched = vec![(job("d"), true), (job("b"), true)];

        let (transitions, _) = run(&index, &fetched);

        let order: Vec<&str> = transitions.iter().map(|t| t.external_id.as_str()).collect();
        assert_eq!(order, vec!["a", "b", "c", "d"]);
    }

    /// §30.2's INV-2 row, driven end to end: two reposts of the same job must
    /// produce two *distinct* durable event keys, and what makes them distinct is
    /// the `transition_seq` this module increments.
    #[test]
    fn repeat_reposts_feed_job_event_key_and_produce_distinct_keys() {
        let source_id = SourceId::new("cohere-greenhouse").expect("a plain slug is valid");
        let posting = job("a");

        let mut facts = stored(&posting, true);
        facts.state = JobState::Inactive;
        facts.absent_since_poll = Some(POLL - 3);
        facts.ttl = Some(at(LAST_SEEN) + INACTIVE_TTL);

        let first_index = index_of([(id("a"), facts)]);
        let (first, _) = run(&first_index, &[(posting.clone(), true)]);
        let first = only(&first).clone();

        // The job goes absent again and is reposted a second time. Its stored
        // facts are the first repost's `after`, pushed back to inactive.
        let mut second_facts = first.after.clone();
        second_facts.state = JobState::Inactive;
        second_facts.absent_since_poll = Some(POLL + 5);
        second_facts.ttl = Some(now() + INACTIVE_TTL);

        let second_index = index_of([(id("a"), second_facts)]);
        let (second, _) = diff(
            &second_index,
            &[(posting, true)],
            now(),
            POLL + 7,
            FILTER_VERSION,
        );
        let second = only(&second);

        assert_eq!(first.event_type, EventType::JobReposted);
        assert_eq!(second.event_type, EventType::JobReposted);
        assert_eq!(first.new_transition_seq, STORED_SEQ + 1);
        assert_eq!(second.new_transition_seq, STORED_SEQ + 2);

        let first_key = job_event_key(
            &source_id,
            &first.external_id,
            first.event_type,
            first.new_transition_seq,
        );
        let second_key = job_event_key(
            &source_id,
            &second.external_id,
            second.event_type,
            second.new_transition_seq,
        );
        assert_ne!(
            first_key, second_key,
            "INV-2: a repeated JOB_REPOSTED is a distinct logical event"
        );
    }
}
