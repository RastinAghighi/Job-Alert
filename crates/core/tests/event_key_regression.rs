//! Event-key repeat-transition regression test — INV-2, spec §13.2 and §30.2.
//!
//! This is the first test in the project by mandate (§38 FIRST ACTION): it exists before any other
//! test and before any implementation. It pins two properties of `jobmon_core::event_key` that pull
//! in opposite directions, and that a content-hash-based key cannot satisfy simultaneously:
//!
//! - a **replayed** identical transition yields the SAME event key, so the retry after a failed
//!   transaction deduplicates instead of emitting a second event;
//! - a **genuine repeat** transition yields DISTINCT event keys, so the second real repost of an
//!   otherwise unchanged job is still delivered rather than silently deduped away.
//!
//! `transition_seq` is what makes both true at once (§13.2.3). A content hash satisfies the first
//! and fails the second, which is the silent-miss bug of §13.1.
//!
//! The golden vectors pin §13.2.1's frozen byte encoding. They were derived from the spec
//! independently of this repository, so they record the specification rather than whatever the
//! implementation happens to produce: if the implementation disagrees with them, the implementation
//! is wrong.
//!
//! What this file deliberately does NOT assert is injectivity of the key. §30.1 forbids it — the
//! key is a 130-bit truncation of SHA-256 (§13.2.2), so injectivity over an unbounded input space
//! is false, and a test claiming it would be asserting something untrue that happens to pass.

use chrono::{DateTime, Utc};
use jobmon_core::event_key::{
    Component, EVENT_KEY_LEN, api_changed_key, encode_components, filter_changed_key,
    job_event_key, notification_event_key, source_bootstrapped_key, source_health_event_key,
    system_degraded_key,
};
use jobmon_core::model::{EventType, ExternalId};
use jobmon_errors::{FaultDomain, SourceId, Stage};

// ---------------------------------------------------------------------------
// Fixture identities. One source, one job, content unchanged throughout —
// `transition_seq` is the only variable in play anywhere in this file.
// ---------------------------------------------------------------------------

const SOURCE: &str = "cohere-greenhouse";
const EXTERNAL_ID: &str = "4012345";

/// §25's correlation bucket: `epoch_minute / 10`.
const WINDOW_ID: u64 = 2_978_124;

/// A stand-in `shape_hash`. §21.1.1 non-identity hashes are full 52-character Base32.
const SHAPE_HASH: &str = "ABCDEFGH23456789ABCDEFGH23456789ABCDEFGH23456789ABCD";

// ---------------------------------------------------------------------------
// GOLDEN VECTORS — derived from §13.2.1 outside this repository. Do not
// recompute them from the implementation; that would make the test record the
// implementation instead of the spec.
// ---------------------------------------------------------------------------

/// Vector 1 — job event. Components per §13.2.3 row 1: `source_id`, `external_id`, `event_type`,
/// `transition_seq`.
const JOB_EVENT_PRE_DIGEST_HEX: &str = "636f686572652d677265656e686f7573651f343031323334351f4a4f425f5245504f535445441f0000000000000005";
const JOB_EVENT_KEY: &str = "2QCHPYZEPF34PNRWYNNFOGQD5P";

/// Vector 2 — source health event. Components: `source_id`, `event_type`, `first_failure_at`.
const SOURCE_HEALTH_PRE_DIGEST_HEX: &str = "636f686572652d677265656e686f7573651f534f555243455f4641494c45441f323032362d30382d31365431303a30353a30372e3030303030303030305a";
const SOURCE_HEALTH_KEY: &str = "CTTVNMCFM5XC6LKGRGJ5HMSQQ4";

/// Vector 3 — system event. Components: `"SYS"`, `event_type`, `stage`, `domain`, `window_id`.
const SYSTEM_EVENT_PRE_DIGEST_HEX: &str =
    "5359531f53595354454d5f44454752414445441f504552534953541f494e4652411f00000000002d714c";
const SYSTEM_EVENT_KEY: &str = "XBXEE5RFB75ICZUETK75KB7ZMA";

// ---------------------------------------------------------------------------
// 1. The §38-mandated first test.
// ---------------------------------------------------------------------------

/// A job that is removed and reposted twice, with unchanged content, must produce two DIFFERENT
/// `JOB_REPOSTED` keys (§13.2.3, §13.7 "Multiple genuine transitions over time").
#[test]
fn repeat_transition_produces_distinct_keys() {
    let src = source_id();
    let ext = external_id();

    // The lifecycle. Content is identical from the first sighting to the last; only the
    // transition sequence advances.
    //
    //   NEW_JOB @ 1 -> JOB_REMOVED @ 2 -> JOB_REPOSTED @ 3 -> JOB_REMOVED @ 4 -> JOB_REPOSTED @ 5
    let _new_job = job_event_key(&src, &ext, EventType::NewJob, 1);
    let _first_removal = job_event_key(&src, &ext, EventType::JobRemoved, 2);
    let first_repost = job_event_key(&src, &ext, EventType::JobReposted, 3);
    let _second_removal = job_event_key(&src, &ext, EventType::JobRemoved, 4);
    let second_repost = job_event_key(&src, &ext, EventType::JobReposted, 5);

    assert_ne!(
        first_repost.as_str(),
        second_repost.as_str(),
        "two genuine reposts of the same unchanged job must derive different event keys; \
         if they collide the second repost is deduplicated away and never notified (INV-2)"
    );

    // The control §38 requires. Hold `transition_seq` CONSTANT across the two reposts — every
    // other component is already identical, because the job's content never changed.
    //
    // The keys collide. That collision is exactly what a content-hash-based key would produce for
    // BOTH genuine reposts, since a content hash sees no difference between them: the second real
    // repost would be treated as a duplicate of the first and the phone notification would be
    // silently and permanently lost (§13.1). That silent miss is the bug `transition_seq` exists
    // to prevent, and this pair of assertions is the whole argument for it — `transition_seq` is
    // the only variable in play.
    let frozen_seq_first = job_event_key(&src, &ext, EventType::JobReposted, 3);
    let frozen_seq_second = job_event_key(&src, &ext, EventType::JobReposted, 3);

    assert_eq!(
        frozen_seq_first.as_str(),
        frozen_seq_second.as_str(),
        "with transition_seq held constant the two reposts are indistinguishable — this is the \
         collision a content-hash key produces for every genuine repeat transition"
    );
}

// ---------------------------------------------------------------------------
// 2-3. The other half of INV-2, and the full lifecycle.
// ---------------------------------------------------------------------------

/// The retry-stability half of INV-2 (§13.2.3). A failed transaction leaves the job at seq `k`, so
/// the retry recomputes `k + 1` and derives the identical key; the conditional `Put` then
/// deduplicates instead of creating a second durable event.
#[test]
fn replayed_transition_produces_identical_key() {
    let src = source_id();
    let ext = external_id();

    let first_attempt = job_event_key(&src, &ext, EventType::JobReposted, 5);
    let retry_after_crash = job_event_key(&src, &ext, EventType::JobReposted, 5);

    assert_eq!(
        first_attempt.as_str(),
        retry_after_crash.as_str(),
        "a replayed identical transition must derive the same event key, or a crashed poll \
         duplicates every event it had already written (INV-2)"
    );
}

/// Every transition in the §13.7 lifecycle is a distinct durable event, so all five keys must be
/// pairwise distinct — not merely the two reposts.
#[test]
fn full_lifecycle_yields_five_distinct_keys() {
    let src = source_id();
    let ext = external_id();

    let labels = [
        "NEW_JOB @ seq 1",
        "JOB_REMOVED @ seq 2",
        "JOB_REPOSTED @ seq 3",
        "JOB_REMOVED @ seq 4",
        "JOB_REPOSTED @ seq 5",
    ];
    let keys = [
        job_event_key(&src, &ext, EventType::NewJob, 1),
        job_event_key(&src, &ext, EventType::JobRemoved, 2),
        job_event_key(&src, &ext, EventType::JobReposted, 3),
        job_event_key(&src, &ext, EventType::JobRemoved, 4),
        job_event_key(&src, &ext, EventType::JobReposted, 5),
    ];

    for i in 0..keys.len() {
        for j in (i + 1)..keys.len() {
            assert_ne!(
                keys[i].as_str(),
                keys[j].as_str(),
                "{} and {} collide on {}",
                labels[i],
                labels[j],
                keys[i].as_str()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 4-6. Golden vectors against §13.2.1's frozen encoding.
// ---------------------------------------------------------------------------

/// Pins §13.2.3 row 1, UTF-8 string components, and 8-byte big-endian integers.
#[test]
fn golden_vector_job_event() {
    let components = [
        Component::Str(SOURCE),
        Component::Str(EXTERNAL_ID),
        Component::Str("JOB_REPOSTED"),
        Component::Int(5),
    ];

    assert_eq!(
        hex(&encode_components(&components)),
        JOB_EVENT_PRE_DIGEST_HEX,
        "the pre-digest byte encoding of a job event has changed; §13.2.1 is durable schema"
    );

    let src = source_id();
    let ext = external_id();
    assert_eq!(
        job_event_key(&src, &ext, EventType::JobReposted, 5).as_str(),
        JOB_EVENT_KEY,
        "job_event_key no longer matches the §13.2.1 golden vector"
    );
}

/// Pins the timestamp rule: RFC 3339 in UTC, exactly nine fractional digits, literal `Z`.
#[test]
fn golden_vector_source_health_event() {
    let first_failure_at = utc("2026-08-16T10:05:07Z");
    let components = [
        Component::Str(SOURCE),
        Component::Str("SOURCE_FAILED"),
        Component::Timestamp(first_failure_at),
    ];

    assert_eq!(
        hex(&encode_components(&components)),
        SOURCE_HEALTH_PRE_DIGEST_HEX,
        "timestamps must encode as 2026-08-16T10:05:07.000000000Z — nine fractional digits, \
         literal Z, never a numeric offset (§13.2.1)"
    );

    let src = source_id();
    assert_eq!(
        source_health_event_key(&src, EventType::SourceFailed, first_failure_at).as_str(),
        SOURCE_HEALTH_KEY,
        "source_health_event_key no longer matches the §13.2.1 golden vector"
    );
}

/// Pins the `"SYS"` scope literal, the `SCREAMING_SNAKE_CASE` wire names of `Stage` and
/// `FaultDomain` (§9), and the widening of `window_id` to 8 big-endian bytes.
#[test]
fn golden_vector_system_event() {
    let components = [
        Component::Str("SYS"),
        Component::Str("SYSTEM_DEGRADED"),
        Component::Str("PERSIST"),
        Component::Str("INFRA"),
        Component::Int(WINDOW_ID),
    ];

    assert_eq!(
        hex(&encode_components(&components)),
        SYSTEM_EVENT_PRE_DIGEST_HEX,
        "the pre-digest byte encoding of a system event has changed; §13.2.1 is durable schema"
    );

    assert_eq!(
        system_degraded_key(Stage::Persist, FaultDomain::Infra, WINDOW_ID).as_str(),
        SYSTEM_EVENT_KEY,
        "system_degraded_key no longer matches the §13.2.1 golden vector"
    );
}

// ---------------------------------------------------------------------------
// 7-8. Encoding normalisation and key shape.
// ---------------------------------------------------------------------------

/// §13.2.1: `…:07Z` and `…:07.000Z` are the same instant and must never be able to produce two
/// different keys. A variable-width fraction would let one outage emit two `SOURCE_FAILED` events.
#[test]
fn timestamp_fraction_is_normalised() {
    let implicit_fraction = utc("2026-08-16T10:05:07Z");
    let explicit_fraction = utc("2026-08-16T10:05:07.000Z");

    // Precondition: the two spellings really are the same instant, so the assertion below is
    // about the encoding and nothing else.
    assert_eq!(implicit_fraction, explicit_fraction);

    let src = source_id();
    assert_eq!(
        source_health_event_key(&src, EventType::SourceFailed, implicit_fraction).as_str(),
        source_health_event_key(&src, EventType::SourceFailed, explicit_fraction).as_str(),
        "one instant must derive one key regardless of how its fraction was spelled (§13.2.1)"
    );
}

/// §13.2.2: every key, from every one of the seven §13.2.3 identity shapes, is 26 characters of
/// RFC 4648 *standard* Base32 — uppercase `A`-`Z` and `2`-`7`, unpadded.
#[test]
fn event_key_shape() {
    assert_eq!(
        EVENT_KEY_LEN, 26,
        "§13.2.2 truncates to 26 Base32 characters (130 bits)"
    );

    let src = source_id();
    let ext = external_id();
    let keys = [
        (
            "job event",
            job_event_key(&src, &ext, EventType::JobReposted, 5),
        ),
        ("source bootstrapped", source_bootstrapped_key(&src, 7)),
        (
            "source health",
            source_health_event_key(&src, EventType::SourceFailed, utc("2026-08-16T10:05:07Z")),
        ),
        ("api changed", api_changed_key(&src, SHAPE_HASH)),
        (
            "system degraded",
            system_degraded_key(Stage::Persist, FaultDomain::Infra, WINDOW_ID),
        ),
        (
            "notification",
            notification_event_key(EventType::NotificationDegraded, WINDOW_ID),
        ),
        ("filter changed", filter_changed_key(3)),
    ];

    for (label, key) in &keys {
        let key = key.as_str();
        assert_eq!(
            key.len(),
            EVENT_KEY_LEN,
            "{label} key {key} is not {EVENT_KEY_LEN} characters"
        );
        for c in key.chars() {
            assert!(
                matches!(c, 'A'..='Z' | '2'..='7'),
                "{label} key {key} contains {c:?}, which is outside RFC 4648 standard Base32 \
                 (A-Z, 2-7, uppercase, unpadded)"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn source_id() -> SourceId {
    SourceId::new(SOURCE).expect("the fixture source id is valid")
}

fn external_id() -> ExternalId {
    ExternalId::new(EXTERNAL_ID).expect("the fixture external id is valid")
}

fn utc(rfc3339: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(rfc3339)
        .expect("the fixture timestamps are valid RFC 3339")
        .with_timezone(&Utc)
}

/// Lowercase hex, for comparing an encoded component sequence against a golden vector.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(out, "{byte:02x}").expect("writing to a String cannot fail");
    }
    out
}
