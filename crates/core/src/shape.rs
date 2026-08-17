//! The three non-identity hashes (§21.1.1) and adapter-contract validation
//! (§18, §22).
//!
//! # None of these is an identity, and none of them is truncated
//!
//! [`body_hash`], [`content_hash`] and [`shape_hash`] answer *did this change?*,
//! not *which thing is this?*. §13.2.2 truncates an event key to 26 characters
//! because a key is read in log lines and lives in a per-source keyspace; these
//! three are compared by machine, stored once per source or per job, and have no
//! such budget. They are therefore the **whole** SHA-256 digest rendered as
//! [`FULL_SHA256_BASE32_LEN`] characters of RFC 4648 standard Base32. Truncating
//! one to the event-key length would silently narrow a change detector from 256
//! bits to 130 for no benefit at all.
//!
//! # Why length prefixes here and separators there
//!
//! §13.2.1 frames identity components with `0x1F` and forbids any component from
//! containing that byte, which is enforceable because the only component that
//! originates upstream — `external_id` — is validated at the boundary
//! ([`crate::model::ExternalId::new`]).
//!
//! `content_hash` covers `title`, `location_raw` and `url`, and **none of those
//! can be validated that way**: they are arbitrary upstream prose that the system
//! is required to preserve exactly (§21.1). Rejecting a posting whose title
//! contains a control byte would discard a real job, and rewriting the title
//! would corrupt the stored content. So this module cannot use separator framing;
//! it uses §21.1.1's `LP(x) = u64_be(len(x)) || x` instead, which is unambiguous
//! for *any* byte string. That is why nothing below calls
//! [`crate::event_key::encode_components`] — only the timestamp encoder is
//! shared, because that encoding is fixed-width and reused verbatim.
//!
//! Concretely, `title = "Intern\u{1f}Toronto"` with `location_raw = "ON"` and
//! `title = "Intern"` with `location_raw = "Toronto\u{1f}ON"` concatenate to the
//! same separator-framed bytes and must not hash the same. `hashes_are_unambiguous_across_separator_bytes`
//! pins exactly that pair.
//!
//! # Why a shape path is a typed sequence and not dotted text
//!
//! A JSON key may legally contain `.`, `[` or `]`, so flattening a path to
//! `location.name` is ambiguous: a document with the single key `"location.name"`
//! would produce the same text as a nested `name` inside `location`. §18
//! therefore makes a path a sequence of typed segments — a key segment carrying
//! its own byte length, or the array marker — which is injective for every
//! possible JSON document.
//!
//! The adapter contract's `array_path` and `required_paths` *are* dotted text,
//! and that is not a contradiction. Those are `&'static str` written by the
//! adapter author next to the parser that depends on them (§18), so their author
//! chooses paths the syntax can express; shape paths are derived from whatever
//! keys an upstream actually emits, which nobody chooses.
//!
//! # Contract validation is not schema comparison (INV-11)
//!
//! [`validate_contract`] checks the adapter's *own* dependencies. A new sibling
//! field changes [`shape_hash`] and produces `API_CHANGED` telemetry while the
//! poll succeeds normally; a `required_path` disappearing is
//! [`FailureKind::RequiredFieldMissing`] and an immediate `SOURCE_FAILED`.
//! Validating structural equality instead would alert on every harmless upstream
//! addition until the alerts were ignored — and an ignored channel is a
//! priority-(1) violation in its own right (§2).

use crate::event_key::{Component, digest_base32, encode_component};
use crate::model::{AdapterContract, NormalizedJob};
use jobmon_errors::{Detail, FailureKind, FaultDomain, PipelineError, Stage};
use serde_json::Value;
use std::collections::BTreeSet;

// ---------------------------------------------------------------------------
// Frozen constants (§21.1.1, §18)
// ---------------------------------------------------------------------------

/// The length of a full SHA-256 digest in unpadded RFC 4648 standard Base32
/// (§21.1.1).
///
/// Thirty-two bytes is 256 bits, and Base32 carries 5 bits per character, so the
/// encoding is `ceil(256 / 5) = 52` characters. This is deliberately **not**
/// [`crate::event_key::EVENT_KEY_LEN`]: these hashes are change detectors rather
/// than identities, so nothing is traded away by keeping all 256 bits.
pub const FULL_SHA256_BASE32_LEN: usize = 52;

/// Domain separator for [`content_hash`] (§21.1.1).
///
/// Length-prefixed like every other field, and versioned in its own text: a
/// future `V2` content encoding produces entirely different digests for the same
/// job, which is what makes the transition a visible stored-data migration
/// rather than a silent one.
const CONTENT_DOMAIN: &str = "JOBMON-CONTENT-V1";

/// Domain separator for [`shape_hash`] (§18).
const SHAPE_DOMAIN: &str = "JOBMON-SHAPE-V1";

/// Marks an absent `posted_at` (§21.1.1), matching §13.2.1's optional encoding.
const ABSENT: u8 = 0x00;

/// Marks a present `posted_at`, prefixing the length-prefixed timestamp
/// (§21.1.1).
const PRESENT: u8 = 0x01;

/// The leading byte of a key path segment — ASCII `K` (§18).
const KEY_SEGMENT: u8 = 0x4B;

/// A whole array path segment — ASCII `A` (§18).
///
/// One byte, with no payload, because an array contributes *that it is an array*
/// and never which index was traversed. That is the entire reason a board going
/// from 40 postings to 41 does not register as a schema change.
const ARRAY_SEGMENT: u8 = 0x41;

// ---------------------------------------------------------------------------
// Shared encoding helpers (§21.1.1)
// ---------------------------------------------------------------------------

/// A length as §21.1.1's fixed 8-byte big-endian field.
///
/// Every target this workspace supports has a `usize` no wider than 64 bits, so
/// the widening is lossless. A hypothetical wider one would need its own
/// encoding decision — the frozen wire format says eight bytes — rather than a
/// silent truncation here.
fn u64_be(value: usize) -> [u8; 8] {
    (value as u64).to_be_bytes()
}

/// Appends `LP(bytes) = u64_be(len(bytes)) || bytes` to `out` (§21.1.1).
///
/// This is the whole reason `content_hash` and `shape_hash` are unambiguous over
/// arbitrary upstream bytes: the reader of these bytes always knows where a field
/// ends before it starts reading it, so no value can impersonate a field
/// boundary.
fn push_length_prefixed(bytes: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&u64_be(bytes.len()));
    out.extend_from_slice(bytes);
}

// ---------------------------------------------------------------------------
// body_hash (§21.1.1)
// ---------------------------------------------------------------------------

/// The full-width digest of a raw response body (§21.1.1).
///
/// §23 compares it against `last_body_hash` to decide whether an archive PUT is
/// worth making, so it is taken over the bytes exactly as received — before
/// decoding, before parsing, and with no normalization of any kind. Anything else
/// would make two byte-different bodies look identical and skip an archive write
/// that the operator later needs.
#[must_use]
pub fn body_hash(body: &[u8]) -> String {
    digest_base32(body, FULL_SHA256_BASE32_LEN)
}

// ---------------------------------------------------------------------------
// content_hash (§21.1.1)
// ---------------------------------------------------------------------------

/// The full-width digest of the five job fields that constitute *content*
/// (§21.1.1).
///
/// Covered, in this order: `title`, `location_raw`, `employment_type`, `url`,
/// `posted_at`.
///
/// # What is excluded, and why the exclusions matter more than the inclusions
///
/// `relevant`, `filter_version`, `state`, `transition_seq`, `absent_since_poll`,
/// `first_seen_at` and `last_seen_at` are all deliberately outside. Every one of
/// them is a fact about *our processing* of a posting rather than about the
/// posting, and §13.3 fires `JOB_UPDATED` precisely when this hash changes.
/// Including any of them would manufacture an "the employer changed this job"
/// event out of a filter version bump, a removal-and-repost, or a poll that
/// merely refreshed a timestamp.
///
/// The derived location fields — `country`, `region`, `city`, `remote` — are
/// excluded for the same reason from the other direction: they are recomputed
/// from `location_raw` on every poll, so hashing them as well would make one
/// upstream change count twice while adding no detection power.
///
/// `employment_type` enters as its canonical wire name
/// ([`crate::model::EmploymentType::as_str`]), which is why renaming a variant
/// there is a stored-data migration: it re-hashes every stored job carrying it
/// and fabricates a `JOB_UPDATED` for each.
#[must_use]
pub fn content_hash(job: &NormalizedJob) -> String {
    let mut bytes = Vec::new();

    push_length_prefixed(CONTENT_DOMAIN.as_bytes(), &mut bytes);
    push_length_prefixed(job.title.as_bytes(), &mut bytes);
    push_length_prefixed(job.location_raw.as_bytes(), &mut bytes);
    push_length_prefixed(job.employment_type.as_str().as_bytes(), &mut bytes);
    push_length_prefixed(job.url.as_bytes(), &mut bytes);

    match job.posted_at {
        None => bytes.push(ABSENT),
        Some(posted_at) => {
            bytes.push(PRESENT);
            // The timestamp encoding is §13.2.1's and is implemented exactly
            // once, in `event_key`. It is reached through the component encoder
            // rather than copied here because a second copy of a fixed-width
            // RFC 3339 formatter is a second thing that can drift. Only the
            // *value* encoding is shared — the framing around it is this
            // module's length prefix, never §13.2.1's separator.
            let mut encoded = Vec::new();
            encode_component(&Component::Timestamp(posted_at), &mut encoded);
            push_length_prefixed(&encoded, &mut bytes);
        }
    }

    digest_base32(&bytes, FULL_SHA256_BASE32_LEN)
}

// ---------------------------------------------------------------------------
// shape_hash (§18)
// ---------------------------------------------------------------------------

/// The full-width digest of the union of structural key paths across **all**
/// array elements (§18).
///
/// `elements` is what [`validate_contract`] returned: the postings array itself,
/// already resolved out of whatever envelope wrapped it. Paths are therefore
/// relative to one element — the enclosing array and the document envelope are
/// the caller's business and contribute nothing, which is what lets an adapter
/// change its `array_path` without every stored `shape_hash` moving.
///
/// # Every element, never a sample
///
/// §18 says *union*, and it means it. Sampling the first element is the obvious
/// optimization and it is wrong twice over: an optional field that appears on the
/// tenth posting would flap the hash depending on which postings happened to be
/// returned, and a genuinely new field would go unnoticed until it reached
/// position zero. Reading every element makes the hash a function of the
/// response's structure alone.
///
/// # What it is invariant to, by construction
///
/// - **Element order.** Paths land in a [`BTreeSet`], so the output is a sorted,
///   deduplicated set rather than a traversal transcript.
/// - **Array length and index renumbering.** An array contributes a single
///   `ARRAY` segment and never an index, so 40 postings and 41 postings of the
///   same shape agree.
/// - **Values.** Only structure is collected; a title changing from one string to
///   another is a `content_hash` matter and must never look like a schema change.
/// - **Key order within an object**, which JSON does not define as meaningful.
///
/// It is *not* invariant to a structural key path appearing or disappearing —
/// that is the signal it exists to raise (INV-11).
#[must_use]
pub fn shape_hash(elements: &[Value]) -> String {
    let mut paths = BTreeSet::new();
    let mut segments = Vec::new();

    for element in elements {
        collect_paths(element, &mut segments, 0, &mut paths);
        debug_assert!(
            segments.is_empty(),
            "every pushed segment must be popped before the next element"
        );
    }

    let mut bytes = Vec::new();
    push_length_prefixed(SHAPE_DOMAIN.as_bytes(), &mut bytes);
    bytes.extend_from_slice(&u64_be(paths.len()));
    // `BTreeSet<Vec<u8>>` iterates in ascending byte-lexicographic order, which
    // is §18's required ordering, and it deduplicates on insert. Both properties
    // come from the container rather than from a sort call a caller could forget.
    for path in &paths {
        push_length_prefixed(path, &mut bytes);
    }

    digest_base32(&bytes, FULL_SHA256_BASE32_LEN)
}

/// Collects the encoded path of every object key and every array node reachable
/// from `value` (§18).
///
/// `segments` carries the encoded segments of the path *to* `value` and is
/// restored before returning, so one buffer serves the whole traversal. `depth`
/// is how many segments it holds; §18's encoded path is
/// `u64_be(segment_count) || segments`, and the count is a prefix rather than
/// something a reader could infer, because a key segment's payload may itself
/// contain any byte.
///
/// Recursion depth is bounded by the JSON parser: `serde_json` rejects documents
/// nested beyond its own limit long before this function sees them, so a
/// pathological response cannot drive this into the Lambda's stack.
fn collect_paths(
    value: &Value,
    segments: &mut Vec<u8>,
    depth: usize,
    paths: &mut BTreeSet<Vec<u8>>,
) {
    match value {
        Value::Object(members) => {
            for (key, child) in members {
                let mark = segments.len();
                segments.push(KEY_SEGMENT);
                segments.extend_from_slice(&u64_be(key.len()));
                segments.extend_from_slice(key.as_bytes());

                paths.insert(encode_path(segments, depth + 1));
                collect_paths(child, segments, depth + 1, paths);

                segments.truncate(mark);
            }
        }
        Value::Array(items) => {
            let mark = segments.len();
            segments.push(ARRAY_SEGMENT);

            // Recorded before descending, so an empty array still contributes
            // its own node (§18). Without this, `{"tags": []}` and
            // `{"tags": 0}` would be indistinguishable, and a board that
            // stopped populating an array would look structurally unchanged.
            paths.insert(encode_path(segments, depth + 1));
            for item in items {
                collect_paths(item, segments, depth + 1, paths);
            }

            segments.truncate(mark);
        }
        // Values are discarded (§18). A scalar's path was already recorded by
        // the key or array node that holds it; there is no scalar segment type,
        // and adding one would make every content edit a schema change.
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// One path in §18's encoded form: `u64_be(segment_count) || segments`.
fn encode_path(segments: &[u8], depth: usize) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(8 + segments.len());
    encoded.extend_from_slice(&u64_be(depth));
    encoded.extend_from_slice(segments);
    encoded
}

// ---------------------------------------------------------------------------
// validate_contract (§22)
// ---------------------------------------------------------------------------

/// Resolves the contract's `array_path` and checks every `required_path` on
/// every element (§22).
///
/// # The lifetime is explicit because it has to be
///
/// With two borrowed inputs, elision cannot tell that the returned slice borrows
/// from `root` rather than from `contract`, so the elided form does not compile.
/// §22 pins this signature for that reason.
///
/// # What this does *not* check
///
/// `min_expected` — deliberately. §22's four conditions are four distinct
/// behaviours, and conflating them is named there as the most common way this
/// class of system corrupts itself. A count that is implausible is
/// `core::plausibility`'s judgement, made against a stored baseline this function
/// cannot see; a contract that is violated is this function's. An empty array
/// therefore validates cleanly here and is rejected, if it should be, one stage
/// later.
///
/// Nor does it check *types*. `required_paths` are the paths an adapter
/// dereferences, so this asks only whether each one resolves. A path resolving to
/// JSON `null` is present: the adapter reads it, and normalization rejects the
/// resulting empty `title` at its own boundary with `NormalizeFailed` (§21.1).
/// Treating `null` as absent here would relabel that failure as a contract
/// violation and route it to the wrong §22 row.
///
/// # Errors
///
/// - [`FailureKind::ArrayPathMissing`] when `array_path` does not resolve, or
///   resolves to something that is not a JSON array. An empty `array_path` means
///   the document root must itself be the array.
/// - [`FailureKind::RequiredFieldMissing`] for the first required path that is
///   absent from any element, with that path in [`Detail::missing_paths`].
///
/// Both are [`Stage::Parse`] / [`FaultDomain::Adapter`]. **Not
/// [`Stage::Schema`]** — §31's acceptance criterion 2 requires a broken
/// required-field mapping to surface as stage `Parse`, and §22 says outright that
/// `Schema` stays reserved and unused in Phase 1. The variant's name is not
/// evidence about where these two failures belong.
pub fn validate_contract<'a>(
    root: &'a Value,
    contract: &AdapterContract,
) -> Result<&'a [Value], PipelineError> {
    let located = resolve(root, contract.array_path);
    let Some(Value::Array(elements)) = located else {
        return Err(array_path_missing(contract.array_path, located));
    };
    let elements = elements.as_slice();

    // Element-major: every required path is checked on every element, and the
    // first absence wins. Checking only the first element would let a partially
    // broken response through, which is the failure §31's criterion 2 exercises.
    for (index, element) in elements.iter().enumerate() {
        for &path in contract.required_paths {
            if resolve(element, path).is_none() {
                return Err(required_field_missing(
                    contract.array_path,
                    path,
                    index,
                    elements.len(),
                ));
            }
        }
    }

    Ok(elements)
}

/// Walks a dot-separated adapter-contract path, where the empty path is the value
/// itself (§18).
///
/// Every segment is an object key. A numeric segment does not index an array —
/// `serde_json` resolves a string index against objects only — so a contract that
/// tries to reach through an array by position fails as a missing path rather
/// than silently working on some documents and not others.
fn resolve<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() {
        return Some(value);
    }

    let mut current = value;
    for segment in path.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}

/// `Stage::Parse` / `Adapter` / [`FailureKind::ArrayPathMissing`] (§22).
///
/// `located` is what the path resolved to, so the message can distinguish "that
/// key is gone" from "that key is now an object" — two different upstream changes
/// that need two different adapter fixes. Only the JSON *type* is named: INV-14
/// keeps upstream bytes out of logs, and the body belongs in the §23 snapshot.
fn array_path_missing(array_path: &str, located: Option<&Value>) -> PipelineError {
    let found = match located {
        None => "nothing".to_owned(),
        Some(value) => format!("a JSON {}", json_type_name(value)),
    };
    let message = if array_path.is_empty() {
        format!(
            "array_path is empty, so the document root must itself be the postings array, but the \
             root is {found}"
        )
    } else {
        format!("array_path `{array_path}` must resolve to a JSON array, but found {found}")
    };

    PipelineError::new(
        Stage::Parse,
        FaultDomain::Adapter,
        FailureKind::ArrayPathMissing,
        message,
    )
}

/// `Stage::Parse` / `Adapter` / [`FailureKind::RequiredFieldMissing`], carrying
/// the path in [`Detail::missing_paths`] (§22).
///
/// [`Detail::missing_paths`] holds required paths only, never the `array_path` —
/// the two failures are separate §22 rows with separate adapter fixes, and a
/// field that mixed them would have to be read alongside the kind to mean
/// anything.
fn required_field_missing(
    array_path: &str,
    path: &str,
    index: usize,
    count: usize,
) -> PipelineError {
    let location = if array_path.is_empty() {
        "the root array".to_owned()
    } else {
        format!("array_path `{array_path}`")
    };

    PipelineError::new(
        Stage::Parse,
        FaultDomain::Adapter,
        FailureKind::RequiredFieldMissing,
        format!(
            "required path `{path}` is absent from the element at index {index} of {count} in \
             {location}"
        ),
    )
    .with_detail(Detail {
        missing_paths: vec![path.to_owned()],
        ..Detail::default()
    })
}

/// The JSON type name of a value, for diagnostics.
fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CountryClass, EmploymentType, ExternalId};
    use chrono::{DateTime, Utc};
    use serde_json::json;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    const TITLE: &str = "Software Engineering Intern";
    const LOCATION: &str = "Toronto, ON";
    const URL: &str = "https://example.invalid/jobs/4012345";

    /// The fixture the golden vectors in [`frozen_golden_vectors`] were minted
    /// from.
    fn job() -> NormalizedJob {
        NormalizedJob {
            external_id: ExternalId::new("4012345").expect("a plain upstream id is valid"),
            title: TITLE.to_owned(),
            location_raw: LOCATION.to_owned(),
            country: Some(CountryClass::Ca),
            region: Some("ON".to_owned()),
            city: Some("Toronto".to_owned()),
            remote: false,
            employment_type: EmploymentType::Internship,
            url: URL.to_owned(),
            posted_at: None,
        }
    }

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .expect("the fixture timestamp is valid RFC 3339")
            .with_timezone(&Utc)
    }

    /// The §18 fixture the golden vector was minted from.
    fn elements() -> Vec<Value> {
        vec![json!({
            "id": "4012345",
            "title": "Software Engineering Intern",
            "location": { "name": "Toronto, ON" },
            "departments": [ { "name": "Engineering" } ],
        })]
    }

    fn assert_full_width_base32(hash: &str) {
        assert_eq!(
            hash.len(),
            FULL_SHA256_BASE32_LEN,
            "a non-identity hash is the whole digest, never truncated to an event key's 26"
        );
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_uppercase() || ('2'..='7').contains(&c)),
            "RFC 4648 *standard* Base32 is A-Z and 2-7, uppercase, unpadded: {hash}"
        );
    }

    // -----------------------------------------------------------------------
    // body_hash (§21.1.1)
    // -----------------------------------------------------------------------

    #[test]
    fn body_hash_is_full_width_deterministic_and_byte_sensitive() {
        let body = br#"{"jobs":[{"id":"1"}]}"#;

        let hash = body_hash(body);
        assert_full_width_base32(&hash);
        assert_eq!(hash, body_hash(body), "the same bytes must hash the same");

        assert_ne!(
            hash,
            body_hash(br#"{"jobs":[{"id":"2"}]}"#),
            "one changed byte must change the hash"
        );

        // §23 hashes the body exactly as received, so whitespace is a change:
        // two bodies that parse identically are still two different bodies, and
        // treating them as one would skip an archive write.
        assert_ne!(
            body_hash(br#"{"a":1}"#),
            body_hash(br#"{"a": 1}"#),
            "body_hash is over raw bytes, not over parsed JSON"
        );

        assert_full_width_base32(&body_hash(b""));
    }

    // -----------------------------------------------------------------------
    // content_hash (§21.1.1)
    // -----------------------------------------------------------------------

    #[test]
    fn content_hash_is_full_width_and_stable() {
        let hash = content_hash(&job());

        assert_full_width_base32(&hash);
        assert_eq!(
            hash,
            content_hash(&job()),
            "identical inputs must hash identically — §13.3 fires JOB_UPDATED off this comparison"
        );
    }

    /// All five covered fields, one at a time. A field that failed to move the
    /// hash would make its upstream edits invisible: no `JOB_UPDATED`, no alert,
    /// and no diagnostic saying so.
    #[test]
    fn content_hash_changes_for_every_covered_field() {
        let base = content_hash(&job());

        let mut title = job();
        title.title = "Machine Learning Intern".to_owned();
        assert_ne!(base, content_hash(&title), "title");

        let mut location = job();
        location.location_raw = "Vancouver, BC".to_owned();
        assert_ne!(base, content_hash(&location), "location_raw");

        let mut employment_type = job();
        employment_type.employment_type = EmploymentType::CoOp;
        assert_ne!(base, content_hash(&employment_type), "employment_type");

        let mut url = job();
        url.url = "https://example.invalid/jobs/4012346".to_owned();
        assert_ne!(base, content_hash(&url), "url");

        // Absent -> present, then present -> a different instant. The first
        // proves the 0x00/0x01 marker is read at all; the second proves the
        // timestamp's own bytes reach the digest.
        let mut posted = job();
        posted.posted_at = Some(at("2026-08-16T10:06:04Z"));
        let with_posted = content_hash(&posted);
        assert_ne!(base, with_posted, "posted_at absent vs present");

        posted.posted_at = Some(at("2026-08-16T10:06:05Z"));
        assert_ne!(with_posted, content_hash(&posted), "posted_at value");
    }

    /// §21.1.1 fixes the covered set at exactly five fields. The derived location
    /// classifications are recomputed from `location_raw` on every poll, so
    /// hashing them too would double-count one upstream change — and, worse,
    /// would let a §21.3 table edit masquerade as the employer editing the job.
    #[test]
    fn content_hash_ignores_fields_outside_the_covered_five() {
        let base = content_hash(&job());

        let mut derived = job();
        derived.country = Some(CountryClass::NotCa);
        derived.region = None;
        derived.city = None;
        derived.remote = true;

        assert_eq!(
            base,
            content_hash(&derived),
            "country, region, city and remote are derived from location_raw and are not content"
        );
    }

    /// The regression §21.1.1 introduces length prefixing for.
    ///
    /// Each pair is two *different* postings whose fields concatenate to the same
    /// bytes under §13.2.1's separator framing. The first assertion in each pair
    /// proves the pair really is confusable that way — without it the test could
    /// pass while testing nothing — and the second proves this module does not
    /// use that framing.
    #[test]
    fn hashes_are_unambiguous_across_separator_bytes() {
        for separator in ['\u{1f}', '\u{1e}'] {
            let mut left = job();
            left.title = format!("Intern{separator}Toronto");
            left.location_raw = "ON".to_owned();

            let mut right = job();
            right.title = "Intern".to_owned();
            right.location_raw = format!("Toronto{separator}ON");

            assert_eq!(
                format!("{}{separator}{}", left.title, left.location_raw),
                format!("{}{separator}{}", right.title, right.location_raw),
                "precondition: separator concatenation cannot tell these two postings apart"
            );
            assert_ne!(
                content_hash(&left),
                content_hash(&right),
                "length prefixing must keep two different postings apart even when their fields \
                 contain the identity separators"
            );
        }
    }

    // -----------------------------------------------------------------------
    // shape_hash (§18)
    // -----------------------------------------------------------------------

    #[test]
    fn shape_hash_is_full_width_and_stable() {
        let hash = shape_hash(&elements());

        assert_full_width_base32(&hash);
        assert_eq!(hash, shape_hash(&elements()));
    }

    /// The response is a set of postings, not a sequence of them. A board that
    /// reorders its results — which many do, by recency — must not look like a
    /// schema change.
    #[test]
    fn shape_hash_ignores_top_level_element_order() {
        let first = json!({ "id": "1", "title": "Intern" });
        let second = json!({ "id": "2", "title": "Intern", "location": { "name": "Toronto" } });

        assert_eq!(
            shape_hash(&[first.clone(), second.clone()]),
            shape_hash(&[second, first])
        );
    }

    /// The array-index rule of §18, at both levels: the number of top-level
    /// postings and the number of members of a nested array are both invisible.
    /// A board going from 40 postings to 41 is the single most ordinary thing
    /// that can happen to it.
    #[test]
    fn shape_hash_ignores_array_length_and_index_renumbering() {
        let posting = |id: &str, departments: usize| {
            json!({
                "id": id,
                "departments": (0..departments)
                    .map(|n| json!({ "name": format!("Team {n}") }))
                    .collect::<Vec<_>>(),
            })
        };

        let one = vec![posting("1", 1)];
        let many = vec![posting("7", 3), posting("8", 2), posting("9", 5)];

        assert_eq!(shape_hash(&one), shape_hash(&many));
    }

    /// Only structure is collected. If a value could move this hash, every
    /// ordinary content edit would emit `API_CHANGED` and the telemetry would be
    /// worthless within a day.
    #[test]
    fn shape_hash_discards_values() {
        assert_eq!(
            shape_hash(&[json!({ "id": 1, "title": "Intern" })]),
            shape_hash(&[json!({ "id": "4012345", "title": "Senior Staff Engineer" })])
        );
    }

    /// §18 records the path to every array node "even if the nested object/array
    /// is empty", so an emptied array is still visibly an array.
    ///
    /// An empty *object* is a different matter and is asserted here too: the
    /// segment alphabet is `KEY` and `ARRAY` with no object marker, so an object
    /// is represented purely by its keys and one with none leaves no trace. That
    /// is not a gap — the union across elements picks its keys up from the first
    /// element that populates it.
    #[test]
    fn shape_hash_records_empty_containers() {
        let scalar = shape_hash(&[json!({ "tags": 0 })]);
        let empty_array = shape_hash(&[json!({ "tags": [] })]);
        let empty_object = shape_hash(&[json!({ "tags": {} })]);

        assert_ne!(
            scalar, empty_array,
            "an array node contributes an ARRAY segment even when it holds nothing"
        );
        assert_eq!(
            scalar, empty_object,
            "an object contributes only its keys, and this one has none"
        );
    }

    /// Why §18 forbids flattening a path to dotted text. Each pair is two
    /// documents a dotted encoding would render identically.
    #[test]
    fn structural_paths_cannot_collide_with_punctuation_in_keys() {
        let collisions = [
            // `a.b` would flatten to the same text as a nested `b` under `a`.
            (json!({ "a.b": 1 }), json!({ "a": { "b": 1 } })),
            // `a[]` would flatten to the same text as an array under `a`.
            (json!({ "a[]": 1 }), json!({ "a": [1] })),
            // And the key length prefix is what keeps a segment boundary from
            // sliding: `ab` + `c` must not read as `a` + `bc`.
            (json!({ "ab": { "c": 1 } }), json!({ "a": { "bc": 1 } })),
        ];

        for (left, right) in collisions {
            assert_ne!(
                shape_hash(std::slice::from_ref(&left)),
                shape_hash(std::slice::from_ref(&right)),
                "{left} and {right} are structurally different documents"
            );
        }
    }

    // -----------------------------------------------------------------------
    // INV-11 — shape versus contract
    // -----------------------------------------------------------------------

    /// The whole of INV-11 in one test: the sibling field moves the shape hash
    /// (so `API_CHANGED` telemetry fires) **and** the contract still validates
    /// (so the poll succeeds and no `SOURCE_FAILED` is raised).
    #[test]
    fn a_new_sibling_field_changes_the_shape_but_not_the_contract() {
        const CONTRACT: AdapterContract = AdapterContract {
            array_path: "jobs",
            required_paths: &["id", "title"],
            min_expected: 1,
        };

        let before = json!({ "jobs": [ { "id": "1", "title": "Intern" } ] });
        let after = json!({
            "jobs": [ { "id": "1", "title": "Intern", "department": { "name": "AI" } } ],
        });

        let before_elements = validate_contract(&before, &CONTRACT).expect("contract holds");
        let after_elements =
            validate_contract(&after, &CONTRACT).expect("a new sibling field is not a violation");

        assert_ne!(
            shape_hash(before_elements),
            shape_hash(after_elements),
            "a new structural key path must be visible as a shape change"
        );
    }

    // -----------------------------------------------------------------------
    // validate_contract (§22)
    // -----------------------------------------------------------------------

    /// An empty `array_path` means the root is the array, and required paths are
    /// resolved into each element — including through nested objects.
    #[test]
    fn validates_a_root_array_with_nested_required_paths() {
        const CONTRACT: AdapterContract = AdapterContract {
            array_path: "",
            required_paths: &["id", "location.name"],
            min_expected: 1,
        };

        let root = json!([
            { "id": "1", "location": { "name": "Toronto, ON" } },
            { "id": "2", "location": { "name": "Remote" } },
        ]);

        let elements = validate_contract(&root, &CONTRACT).expect("both elements satisfy it");
        assert_eq!(elements.len(), 2);
        assert_eq!(elements[0]["id"], json!("1"));
    }

    #[test]
    fn validates_a_nested_array_path() {
        const CONTRACT: AdapterContract = AdapterContract {
            array_path: "data.postings",
            required_paths: &["title"],
            min_expected: 1,
        };

        let root = json!({ "data": { "postings": [ { "title": "Intern" } ], "total": 1 } });

        let elements = validate_contract(&root, &CONTRACT).expect("the path resolves to the array");
        assert_eq!(elements.len(), 1);
    }

    /// §22's `array_path` row, in all three ways it can fail: the path is gone,
    /// the path now holds something that is not an array, and an empty path
    /// against a root that is not an array.
    #[test]
    fn a_missing_or_non_array_array_path_is_array_path_missing() {
        const NESTED: AdapterContract = AdapterContract {
            array_path: "data.postings",
            required_paths: &["title"],
            min_expected: 1,
        };
        const ROOT: AdapterContract = AdapterContract {
            array_path: "",
            required_paths: &[],
            min_expected: 1,
        };

        let cases = [
            (NESTED, json!({ "data": { "total": 0 } })),
            (NESTED, json!({ "data": { "postings": { "edges": [] } } })),
            (NESTED, json!({ "data": "unavailable" })),
            (ROOT, json!({ "jobs": [] })),
        ];

        for (contract, root) in cases {
            let err = validate_contract(&root, &contract).expect_err("must not validate");

            // §31 criterion 2 pins the stage: `Parse`, never `Schema`.
            assert_eq!(err.stage, Stage::Parse);
            assert_eq!(err.domain, FaultDomain::Adapter);
            assert_eq!(err.kind, FailureKind::ArrayPathMissing);
            assert!(
                err.detail.missing_paths.is_empty(),
                "missing_paths carries required paths only"
            );
        }
    }

    /// Every required path is checked on every element, so a response that is
    /// well-formed only at position zero is still a contract violation.
    #[test]
    fn a_required_path_missing_on_a_later_element_is_required_field_missing() {
        const CONTRACT: AdapterContract = AdapterContract {
            array_path: "jobs",
            required_paths: &["id", "title"],
            min_expected: 1,
        };

        let root = json!({
            "jobs": [
                { "id": "1", "title": "Intern" },
                { "id": "2" },
                { "id": "3", "title": "Co-op" },
            ],
        });

        let err = validate_contract(&root, &CONTRACT).expect_err("element 1 has no title");

        assert_eq!(err.stage, Stage::Parse);
        assert_eq!(err.domain, FaultDomain::Adapter);
        assert_eq!(err.kind, FailureKind::RequiredFieldMissing);
        assert_eq!(err.detail.missing_paths, vec!["title".to_owned()]);
        assert!(
            err.detail.message.contains("index 1"),
            "the message should name the offending element: {}",
            err.detail.message
        );
    }

    /// A nested required path counts as absent when any segment of it is, not
    /// only the last.
    #[test]
    fn a_nested_required_path_is_missing_when_its_parent_is() {
        const CONTRACT: AdapterContract = AdapterContract {
            array_path: "",
            required_paths: &["location.name"],
            min_expected: 1,
        };

        let err = validate_contract(&json!([{ "id": "1" }]), &CONTRACT)
            .expect_err("`location` itself is gone");

        assert_eq!(err.kind, FailureKind::RequiredFieldMissing);
        assert_eq!(err.detail.missing_paths, vec!["location.name".to_owned()]);
    }

    /// A required path that resolves to JSON `null` is present. §22 asks whether
    /// the adapter's dependencies exist, not whether their values are usable —
    /// an unusable value is `NormalizeFailed` at the §21.1 boundary, and the two
    /// failures are different rows with different fixes.
    #[test]
    fn a_null_required_path_is_present() {
        const CONTRACT: AdapterContract = AdapterContract {
            array_path: "",
            required_paths: &["title"],
            min_expected: 1,
        };

        assert!(validate_contract(&json!([{ "title": null }]), &CONTRACT).is_ok());
    }

    /// §22 keeps the count gate out of contract validation: an empty array is a
    /// perfectly valid contract result, and whether it is *plausible* is
    /// `core::plausibility`'s judgement against a baseline this function cannot
    /// see.
    #[test]
    fn contract_validation_does_not_check_min_expected() {
        const CONTRACT: AdapterContract = AdapterContract {
            array_path: "jobs",
            required_paths: &["id"],
            min_expected: 50,
        };

        let root = json!({ "jobs": [] });

        let elements =
            validate_contract(&root, &CONTRACT).expect("an empty array satisfies the contract");
        assert!(elements.is_empty());
    }

    // -----------------------------------------------------------------------
    // Frozen wire format
    // -----------------------------------------------------------------------

    /// §21.1.1 says changing any of these encodings is a stored-data migration,
    /// not a cosmetic refactor: `content_hash` decides `JOB_UPDATED` and
    /// `shape_hash` decides `API_CHANGED`, both against values already persisted
    /// under the old encoding. A refactor that silently re-hashed every stored
    /// job would fabricate a transition for each of them.
    ///
    /// These vectors were computed from the specification text by an independent
    /// implementation, not captured from this module's output, so they pin the
    /// spec rather than whatever this module happened to do first.
    #[test]
    fn frozen_golden_vectors() {
        assert_eq!(
            body_hash(b"{\"jobs\":[]}"),
            "BJLZN2J7TNL5356EL6DAJBOLW42T7QHNUPOKOU5EITIP2KQVOP7A"
        );
        assert_eq!(
            body_hash(b""),
            "4OYMIQUY7QOBJGX36TEJS35ZEQT24QPEMSNZGTFESWMRW6CSXBKQ"
        );

        assert_eq!(
            content_hash(&job()),
            "EERDZ7KI4PMJ4ZX325XAFDTHCEUPK4ZVBWP6IXVLAK3WEKC4XD7A"
        );

        // The same fixture with `posted_at`, which also pins §13.2.1's
        // fixed-width nine-digit fraction: this vector was minted from the text
        // `2026-08-16T10:06:04.000000000Z`, so a formatter that elided the
        // trailing zeros would fail here.
        let mut posted = job();
        posted.posted_at = Some(at("2026-08-16T10:06:04Z"));
        assert_eq!(
            content_hash(&posted),
            "BVXSZ6XNL3QHPCBE5SM652HCMAQ7WWIA7K55FT7M6E6XFFBU2TJA"
        );

        assert_eq!(
            shape_hash(&elements()),
            "DLOD3TIUOLKVRS6NAR25YBVZJGPP56UFRDWKUMYRKTJ3LIUX7RBQ"
        );
        assert_eq!(
            shape_hash(&[]),
            "Q2HJA6UAFPLPJE37AJMB4GBMUC6XDO3PHUATBKXCPC3KBZ5WK3TA"
        );
    }
}
