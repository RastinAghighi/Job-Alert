//! Deterministic event identity (§13.2, INV-2).
//!
//! Every durable identity in this system is a digest over concatenated
//! components, which makes **the byte encoding of each component durable
//! schema**. Changing one silently changes every key derived after the change:
//! keys minted before and after a deploy stop matching, retries stop
//! deduplicating, and INV-2 fails without any error surfacing. §13.2.1 therefore
//! freezes the encoding, and requires it to be implemented **exactly once** and
//! reused everywhere — which is why [`encode_component`], [`encode_components`]
//! and [`digest_base32`] are public rather than private helpers of
//! [`event_key_from_components`].
//!
//! # Why `transition_seq` and not a content hash
//!
//! A content-based key collides on genuine repeat transitions: a job removed →
//! reposted → removed → reposted with unchanged content produces two identical
//! keys, so the *second* real repost is deduplicated away and never notified —
//! the silent miss of §13.1. `transition_seq` gives both required properties at
//! once: a failed transaction leaves the job at seq `k`, so the retry recomputes
//! `k + 1` and derives the identical key (stable under retry), while the second
//! repost occurs at a later seq and derives a different one (distinct across
//! genuine repeats). `tests/event_key_regression.rs` pins both halves.
//!
//! # Scope of this module
//!
//! Key **derivation** only. The `"EVT#"` sort-key prefix §13.2.2 mentions is
//! repository serialization and belongs to Phase 4, so it appears nowhere here;
//! a key is not a DynamoDB sort key until the repository says it is.
//!
//! §13.4.1's `ClientRequestToken` reuses [`encode_components`] for its outer
//! wrapper, but its *request fingerprint* is a different, length-prefixed
//! encoding — arbitrary upstream-derived values such as titles may contain
//! `0x1F` or `0x1E`, so separator framing is not unambiguous for transaction
//! parameters. That encoding is Phase 4 and is deliberately not implemented
//! here.

use crate::model::{EventType, ExternalId};
use chrono::{DateTime, Datelike, Timelike, Utc};
use data_encoding::BASE32_NOPAD;
use jobmon_errors::{FaultDomain, SourceId, Stage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

// ---------------------------------------------------------------------------
// Frozen constants (§13.2.1, §13.2.2)
// ---------------------------------------------------------------------------

/// Separator between components — ASCII Unit Separator (§13.2.1).
///
/// No component may contain this byte. Only `external_id` originates upstream,
/// and [`ExternalId::new`] rejects every ASCII control byte, which is what keeps
/// field boundaries unambiguous without an escaping scheme and makes INV-2
/// unconditional rather than dependent on what an upstream API returns.
pub const COMPONENT_SEPARATOR: u8 = 0x1F;

/// Separator between elements of a [`Component::List`] — ASCII Record Separator
/// (§13.2.1).
pub const RECORD_SEPARATOR: u8 = 0x1E;

/// Length of an event key in Base32 characters (§13.2.2).
///
/// Twenty-six characters is 130 bits: ample for a per-source keyspace and short
/// enough to read in a log line.
pub const EVENT_KEY_LEN: usize = 26;

/// The scope component of a system-scoped event (§13.2.3, §16.1).
///
/// Source-scoped events use their `source_id`; the four types that are not
/// attributable to one source use this literal, matching the fixed `SYS#EVT`
/// partition they live in.
pub const SYS_SCOPE: &str = "SYS";

/// Marks an absent optional (§13.2.1).
const ABSENT: u8 = 0x00;

/// Marks a present optional, and prefixes the inner value's encoding (§13.2.1).
const PRESENT: u8 = 0x01;

/// Nanoseconds per second, for splitting chrono's leap-second representation.
const NANOS_PER_SECOND: u32 = 1_000_000_000;

// ---------------------------------------------------------------------------
// EventKey
// ---------------------------------------------------------------------------

/// A derived event identity: [`EVENT_KEY_LEN`] characters of RFC 4648 *standard*
/// Base32 (§13.2.2).
///
/// There is no public constructor from a string. Every key in the system comes
/// from one of the §13.2.3 typed constructors below, so the only way to hold one
/// is to have derived it from a component sequence the specification names — a
/// free-form `EventKey::new(&str)` would let a caller invent an identity that no
/// component sequence maps to.
///
/// [`Ord`] is derived and is byte-lexicographic over the Base32 text, which is
/// the order the `EVT#` sort key will scan in once Phase 4 serializes it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventKey(String);

impl EventKey {
    /// The key's Base32 text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EventKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// Canonical component encoding (§13.2.1)
// ---------------------------------------------------------------------------

/// One component of an identity, in the frozen §13.2.1 wire format.
///
/// The variants are the complete set of encodable shapes, not a convenience
/// subset: [`Component::Bool`], [`Component::Opt`] and [`Component::List`] are
/// unused by the seven §13.2.3 identity shapes but are part of the frozen
/// encoding that §13.4.1 reuses, so they are specified and tested here rather
/// than invented later by whoever needs them first.
#[derive(Clone, Debug)]
pub enum Component<'a> {
    /// UTF-8 bytes exactly as given.
    Str(&'a str),
    /// Unsigned 64-bit big-endian, always 8 bytes. Narrower integers widen first.
    Int(u64),
    /// RFC 3339 in UTC, exactly nine fractional digits, literal `Z`.
    Timestamp(DateTime<Utc>),
    /// The ASCII string `true` or `false`.
    Bool(bool),
    /// `0x00` when absent; `0x01` followed by the inner encoding when present.
    Opt(Option<Box<Component<'a>>>),
    /// Elements joined by [`RECORD_SEPARATOR`], with no leading or trailing one.
    List(Vec<Component<'a>>),
}

/// Appends `component`'s §13.2.1 encoding to `out`.
///
/// Strings are written verbatim: no BOM, no Unicode normalization, no case
/// folding, no trimming, no length prefix. Integers are fixed-width, which
/// removes any leading-zero or radix ambiguity. Timestamps are fixed-width for
/// the same reason: exactly nine fractional digits, so one instant has exactly
/// one encoding however its fraction was spelled upstream.
pub fn encode_component(component: &Component<'_>, out: &mut Vec<u8>) {
    match component {
        Component::Str(text) => out.extend_from_slice(text.as_bytes()),
        Component::Int(value) => out.extend_from_slice(&value.to_be_bytes()),
        Component::Timestamp(at) => out.extend_from_slice(encode_timestamp(*at).as_bytes()),
        Component::Bool(true) => out.extend_from_slice(b"true"),
        Component::Bool(false) => out.extend_from_slice(b"false"),
        Component::Opt(None) => out.push(ABSENT),
        Component::Opt(Some(inner)) => {
            out.push(PRESENT);
            encode_component(inner, out);
        }
        Component::List(elements) => {
            for (index, element) in elements.iter().enumerate() {
                if index > 0 {
                    out.push(RECORD_SEPARATOR);
                }
                encode_component(element, out);
            }
        }
    }
}

/// Encodes a component sequence, joined by exactly one [`COMPONENT_SEPARATOR`]
/// between adjacent components (§13.2.2).
///
/// `N` components therefore produce `N - 1` separators, and there is no trailing
/// separator — a trailing one would make a two-component sequence encode
/// identically to a three-component sequence whose last component is empty.
#[must_use]
pub fn encode_components(components: &[Component<'_>]) -> Vec<u8> {
    let mut out = Vec::new();
    for (index, component) in components.iter().enumerate() {
        if index > 0 {
            out.push(COMPONENT_SEPARATOR);
        }
        encode_component(component, &mut out);
    }
    out
}

/// The §13.2.1 text form of a digest: SHA-256, then RFC 4648 **standard** Base32
/// (`A`–`Z`, `2`–`7`), uppercase and unpadded, truncated to the leading `len`
/// characters.
///
/// Standard Base32, not `BASE32HEX` — the two use different alphabets and would
/// produce different text for the same digest.
///
/// A SHA-256 digest encodes to 52 characters, so a `len` above that yields the
/// whole encoding. Truncation is always on a character boundary because the
/// alphabet is ASCII.
#[must_use]
pub fn digest_base32(bytes: &[u8], len: usize) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = BASE32_NOPAD.encode(digest.as_slice());
    encoded.truncate(len);
    encoded
}

/// Derives an event key from a §13.2.3 component sequence (§13.2.2).
///
/// Prefer the typed constructors below. This is public because §13.4.1 and later
/// phases derive identities from sequences this module does not enumerate, and
/// §13.2.1 requires them to reuse this encoder rather than re-implement it.
#[must_use]
pub fn event_key_from_components(components: &[Component<'_>]) -> EventKey {
    EventKey(digest_base32(&encode_components(components), EVENT_KEY_LEN))
}

/// Formats an instant as §13.2.1's fixed-width RFC 3339: UTC, exactly nine
/// fractional digits, literal `Z`, as in `2026-08-16T10:06:04.000000000Z`.
///
/// The fraction is fixed-width because `…:04Z` and `…:04.000Z` are the same
/// instant and must not be able to produce two different keys — a variable-width
/// fraction would let one outage emit two `SOURCE_FAILED` events with different
/// identities. The digits are written here rather than delegated to a chrono
/// format string so that no formatter setting, present or future, can elide a
/// trailing zero or substitute a numeric offset for the `Z`.
///
/// chrono represents a leap second as a nanosecond field at or above one second,
/// so the carry is applied explicitly: the second becomes `60` and the fraction
/// stays nine digits, instead of the fraction silently widening to ten.
fn encode_timestamp(at: DateTime<Utc>) -> String {
    let second = at.second() + at.nanosecond() / NANOS_PER_SECOND;
    let nanosecond = at.nanosecond() % NANOS_PER_SECOND;

    // `{:04}` is a minimum width: a year outside 0..=9999 widens the field
    // rather than truncating, so the encoding stays injective on the year.
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}Z",
        at.year(),
        at.month(),
        at.day(),
        at.hour(),
        at.minute(),
        second,
        nanosecond,
    )
}

// ---------------------------------------------------------------------------
// The seven §13.2.3 identity shapes
// ---------------------------------------------------------------------------
//
// The type component is always `EventType::as_str()` — the canonical wire name,
// never an abbreviation. §13.2.3 exists because v1.1's §14 abbreviated six of
// the sixteen (`DEGRADED` for `SOURCE_DEGRADED`, `BOOTSTRAP` for
// `SOURCE_BOOTSTRAPPED`, `NOTIFY_DEGRADED` for `NOTIFICATION_DEGRADED`), which
// made the obvious implementation silently mint the wrong key. Even where the
// type is fixed by the constructor, the string comes from `as_str()` rather than
// a literal, so the two cannot drift apart.
//
// The event-type-restricted constructors guard their row with `debug_assert!`
// rather than returning `Result`: these are internal calls from `core::diff` and
// `core::health`, a misrouted type is a programming error and not a runtime
// condition, and a `Result` would push a `?` and an unreachable error arm into
// every caller.

/// Job-lifecycle events: `source_id`, `external_id`, `event_type`,
/// `transition_seq` (§13.2.3 row 1).
///
/// Valid for `NEW_JOB`, `BECAME_RELEVANT`, `JOB_REPOSTED`, `JOB_UPDATED`,
/// `BECAME_IRRELEVANT` and `JOB_REMOVED`.
///
/// The v1.1 component order is kept deliberately: reordering the formula this
/// specification calls its most correctness-critical is churn with no benefit,
/// and the order is load-bearing in §13.7's crash walkthroughs.
///
/// # Panics
///
/// Debug builds assert that `event_type` is one of the six job types.
#[must_use]
pub fn job_event_key(
    source_id: &SourceId,
    external_id: &ExternalId,
    event_type: EventType,
    transition_seq: u64,
) -> EventKey {
    debug_assert!(
        matches!(
            event_type,
            EventType::NewJob
                | EventType::BecameRelevant
                | EventType::JobReposted
                | EventType::JobUpdated
                | EventType::BecameIrrelevant
                | EventType::JobRemoved
        ),
        "job_event_key called with {event_type}, which §13.2.3 gives a different component sequence"
    );

    event_key_from_components(&[
        Component::Str(source_id.as_str()),
        Component::Str(external_id.as_str()),
        Component::Str(event_type.as_str()),
        Component::Int(transition_seq),
    ])
}

/// `SOURCE_BOOTSTRAPPED`: `source_id`, `event_type`, `poll_seq` (§13.2.3 row 2).
///
/// `current_poll_seq` is §13.4's *current* poll number — `stored_poll_seq + 1` —
/// not the stored one. That is what keeps INV-10 true across retries: META
/// advances only in the Phase C commit marker, so a crash mid-bootstrap leaves
/// `stored_poll_seq` unchanged, the retry recomputes the identical
/// `current_poll_seq`, derives the identical key, and the conditional `Put`
/// deduplicates the summary event instead of emitting a second one.
#[must_use]
pub fn source_bootstrapped_key(source_id: &SourceId, current_poll_seq: u64) -> EventKey {
    event_key_from_components(&[
        Component::Str(source_id.as_str()),
        Component::Str(EventType::SourceBootstrapped.as_str()),
        Component::Int(current_poll_seq),
    ])
}

/// Source-health events: `source_id`, `event_type`, `first_failure_at`
/// (§13.2.3 row 3).
///
/// Valid for `SOURCE_DEGRADED`, `SOURCE_FAILED`, `SOURCE_RECOVERED` and
/// `SOURCE_QUARANTINED`.
///
/// §8.1 sets `first_failure_at` when `consecutive_failures` goes from 0 to 1 and
/// clears it on any success, which makes it the discriminator that gives one
/// outage one identity per event type however many polls it spans.
///
/// **For `SOURCE_RECOVERED` the caller must pass the `first_failure_at` of the
/// outage that is ending** — the value being cleared, read before the success is
/// applied — not the post-success `None`. Passing anything else detaches the
/// recovery from its outage and breaks the one-identity-per-outage property that
/// deduplicates a re-derived recovery event.
///
/// # Panics
///
/// Debug builds assert that `event_type` is one of the four health types.
#[must_use]
pub fn source_health_event_key(
    source_id: &SourceId,
    event_type: EventType,
    first_failure_at: DateTime<Utc>,
) -> EventKey {
    debug_assert!(
        matches!(
            event_type,
            EventType::SourceDegraded
                | EventType::SourceFailed
                | EventType::SourceRecovered
                | EventType::SourceQuarantined
        ),
        "source_health_event_key called with {event_type}, which §13.2.3 gives a different \
         component sequence"
    );

    event_key_from_components(&[
        Component::Str(source_id.as_str()),
        Component::Str(event_type.as_str()),
        Component::Timestamp(first_failure_at),
    ])
}

/// `API_CHANGED`: `source_id`, `event_type`, `new_shape_hash` (§13.2.3 row 4).
///
/// The *new* shape hash, so one identity covers one shape change however many
/// polls observe it before the adapter is fixed.
#[must_use]
pub fn api_changed_key(source_id: &SourceId, new_shape_hash: &str) -> EventKey {
    event_key_from_components(&[
        Component::Str(source_id.as_str()),
        Component::Str(EventType::ApiChanged.as_str()),
        Component::Str(new_shape_hash),
    ])
}

/// `SYSTEM_DEGRADED`: `"SYS"`, `event_type`, `stage`, `domain`, `window_id`
/// (§13.2.3 row 5).
///
/// `stage` and `domain` encode as their §9 `SCREAMING_SNAKE_CASE` wire names.
/// `window_id` is §25's correlation bucket, `epoch_minute / 10`, encoded as an
/// integer — so a window that keeps failing the same way emits one event, and
/// the next window emits its own.
#[must_use]
pub fn system_degraded_key(stage: Stage, domain: FaultDomain, window_id: u64) -> EventKey {
    event_key_from_components(&[
        Component::Str(SYS_SCOPE),
        Component::Str(EventType::SystemDegraded.as_str()),
        Component::Str(stage.as_str()),
        Component::Str(domain.as_str()),
        Component::Int(window_id),
    ])
}

/// Notification-health events: `"SYS"`, `event_type`, `window_id`
/// (§13.2.3 row 6).
///
/// Valid for `NOTIFICATION_DEGRADED` and `NOTIFICATION_RECOVERED`. `window_id`
/// is the §25 correlation bucket, as in [`system_degraded_key`].
///
/// # Panics
///
/// Debug builds assert that `event_type` is one of the two notification types.
#[must_use]
pub fn notification_event_key(event_type: EventType, window_id: u64) -> EventKey {
    debug_assert!(
        matches!(
            event_type,
            EventType::NotificationDegraded | EventType::NotificationRecovered
        ),
        "notification_event_key called with {event_type}, which §13.2.3 gives a different \
         component sequence"
    );

    event_key_from_components(&[
        Component::Str(SYS_SCOPE),
        Component::Str(event_type.as_str()),
        Component::Int(window_id),
    ])
}

/// `FILTER_CHANGED`: `"SYS"`, `event_type`, `filter_version` (§13.2.3 row 7).
///
/// `filter_version` is a `u32` on the model and widens to §13.2.1's fixed 8-byte
/// big-endian integer here, so it hashes identically to any other integer
/// component of the same value.
#[must_use]
pub fn filter_changed_key(filter_version: u32) -> EventKey {
    event_key_from_components(&[
        Component::Str(SYS_SCOPE),
        Component::Str(EventType::FilterChanged.as_str()),
        Component::Int(u64::from(filter_version)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(component: &Component<'_>) -> Vec<u8> {
        let mut out = Vec::new();
        encode_component(component, &mut out);
        out
    }

    /// Pins the four §13.2.1 rules that no other Phase-1 test reaches.
    ///
    /// `tests/event_key_regression.rs` covers strings, integers, timestamps and
    /// the separator framing through the seven identity shapes, but none of those
    /// shapes contains a boolean, an optional or a list. Those three rules are
    /// nonetheless frozen wire format, and §13.4.1 reuses this encoder in Phase 4
    /// — so they are pinned here, byte for byte, before a caller exists to notice
    /// if they drifted.
    #[test]
    fn frozen_primitive_encodings() {
        assert_eq!(encoded(&Component::Bool(true)), b"true".as_slice());
        assert_eq!(encoded(&Component::Bool(false)), b"false".as_slice());

        assert_eq!(encoded(&Component::Opt(None)), [ABSENT].as_slice());

        let present: [u8; 9] = [PRESENT, 0, 0, 0, 0, 0, 0, 0, 1];
        assert_eq!(
            encoded(&Component::Opt(Some(Box::new(Component::Int(1))))),
            present.as_slice(),
            "a present optional is 0x01 followed by the inner value's encoding"
        );

        assert_eq!(
            encoded(&Component::List(vec![
                Component::Str("a"),
                Component::Str("b"),
            ])),
            b"a\x1Eb".as_slice(),
            "list elements are joined by one 0x1E, with none leading or trailing"
        );

        // Three components produce exactly two separators and none trailing.
        // None of the three encodings can contain a 0x1F of its own, so the
        // count is a count of separators and nothing else.
        let sequence = encode_components(&[
            Component::Str("a"),
            Component::Int(1),
            Component::Bool(false),
        ]);
        assert_eq!(
            sequence
                .iter()
                .filter(|byte| **byte == COMPONENT_SEPARATOR)
                .count(),
            2,
            "N components must produce N-1 separators"
        );
        assert_ne!(
            sequence.last().copied(),
            Some(COMPONENT_SEPARATOR),
            "a trailing separator would make this sequence collide with a four-component \
             sequence ending in an empty component"
        );
    }
}
