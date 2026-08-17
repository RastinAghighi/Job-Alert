//! Three-axis failure taxonomy (spec §9).
//!
//! `Stage` answers *where*, `FaultDomain` answers *whose fault*, `FailureKind`
//! answers *what*. Alert policy keys on `(domain, kind)`; the human-facing
//! message leads with stage.
//!
//! **May depend on:** nothing (std + serde).
//! **Must not know about:** AWS, HTTP, tokio.
//!
//! # The wire names are durable schema
//!
//! Each of the three axes exposes `as_str()` returning its `SCREAMING_SNAKE_CASE`
//! variant name, and derives serde with `rename_all = "SCREAMING_SNAKE_CASE"` so
//! that the serialized form and `as_str()` agree by construction rather than by
//! review. This is not cosmetic: §13.2.3 hashes `stage` and `domain` into the
//! durable `SYSTEM_DEGRADED` event key by these exact strings, and §25/§16.2 build
//! the DynamoDB attribute names `fail_<STAGE>_<DOMAIN>`, `src_<STAGE>_<DOMAIN>` and
//! `fail_<kind>` from them. Renaming a variant therefore repartitions counters and
//! breaks INV-2 for every key minted before the change, with no error surfacing
//! anywhere.
//!
//! `clippy::result_large_err` is allowed workspace-wide, not here: `PipelineError`
//! carries §9's `Detail` by value and every §17.1 port signature returns it by
//! value. See the rationale next to `result_large_err` in the root
//! `[workspace.lints.clippy]`.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

/// An operator-supplied source identifier, validated at construction.
///
/// This lives in `jobmon-errors` and not in `jobmon-core` because
/// [`PipelineError`] carries an `Option<SourceId>` and §17 makes this crate the
/// dependency leaf: defining it in `core` would require an `errors -> core` edge
/// and produce a dependency cycle. `jobmon-core` re-exports it so downstream
/// crates can write `jobmon_core::SourceId` (§9). Typing the field as
/// `Option<String>` was rejected — that discards type safety at exactly the
/// boundary where a source id is most likely to be confused with an adapter name
/// or an external id.
///
/// # Why validation lives in the constructor
///
/// `source_id` is a component of every durable event key, and §13.2.1 requires
/// that no component may contain `0x1F` or `0x1E`. Validating at construction
/// makes INV-2 unconditional instead of dependent on operator hygiene. The rule is
/// deliberately stricter than banning just those two identity separators — every
/// ASCII control byte is rejected — which keeps every operator-supplied identity
/// component safe by construction.
///
/// The supplied bytes are otherwise preserved exactly: no trimming, no case
/// folding, no Unicode normalization (§13.2.1).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SourceId(String);

impl SourceId {
    /// Validates `raw` and wraps it.
    ///
    /// # Errors
    ///
    /// Rejects an empty string, a string that is entirely ASCII whitespace, and
    /// any string containing an ASCII control byte (`0x00..=0x1F` or `0x7F`).
    /// Rejection is `Stage::Scheduler` / `FaultDomain::Infra` /
    /// `FailureKind::ConfigInvalid` (§9): an invalid `source_id` can only reach us
    /// from the source registry, so it is a configuration fault observed before
    /// any source is claimed.
    pub fn new(raw: &str) -> Result<Self, PipelineError> {
        let reject = |why: &str| -> Result<Self, PipelineError> {
            Err(PipelineError::new(
                Stage::Scheduler,
                FaultDomain::Infra,
                FailureKind::ConfigInvalid,
                // `{raw:?}` escapes control bytes as `\u{...}` rather than emitting
                // them raw into a log line.
                format!("invalid source_id ({why}): {raw:?}"),
            ))
        };

        if raw.is_empty() || raw.bytes().all(|b| b.is_ascii_whitespace()) {
            return reject("empty or all ASCII whitespace");
        }
        if let Some(byte) = raw.bytes().find(u8::is_ascii_control) {
            return reject(&format!("contains ASCII control byte 0x{byte:02X}"));
        }
        Ok(Self(raw.to_owned()))
    }

    /// The identifier's exact bytes, as supplied.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whose fault the failure is — the *whose fault* axis (§9).
///
/// A flat stage list is insufficient because the same stage can belong to
/// different domains: a `Decode` failure is `Upstream` if they returned HTML and
/// `Adapter` if we sent the wrong `Accept` header. Alert policy keys on
/// `(domain, kind)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FaultDomain {
    Upstream,
    Adapter,
    Infra,
    Notify,
    Archive,
}

impl FaultDomain {
    /// The durable wire name. Agrees with the serde representation by
    /// construction; see the crate-level note on wire names.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Upstream => "UPSTREAM",
            Self::Adapter => "ADAPTER",
            Self::Infra => "INFRA",
            Self::Notify => "NOTIFY",
            Self::Archive => "ARCHIVE",
        }
    }
}

impl fmt::Display for FaultDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where in the pipeline the failure occurred — the *where* axis (§9).
///
/// Stages dropped from the original 17-stage list and why: `REQUEST_BUILD` is
/// validated at registration time rather than being a runtime failure, so
/// `ConfigInvalid` covers it; `CONTENT_TYPE` is merged into `Decode`; `FILTER` and
/// `DIFF` are pure predicates over already-validated data and cannot fail at
/// runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Stage {
    Scheduler,
    Claim,
    Connect,
    Http,
    Decode,
    Parse,
    Schema,
    Normalize,
    Plausibility,
    Persist,
    Archive,
    Notify,
    Heartbeat,
}

impl Stage {
    /// The durable wire name. Agrees with the serde representation by
    /// construction; see the crate-level note on wire names.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Scheduler => "SCHEDULER",
            Self::Claim => "CLAIM",
            Self::Connect => "CONNECT",
            Self::Http => "HTTP",
            Self::Decode => "DECODE",
            Self::Parse => "PARSE",
            Self::Schema => "SCHEMA",
            Self::Normalize => "NORMALIZE",
            Self::Plausibility => "PLAUSIBILITY",
            Self::Persist => "PERSIST",
            Self::Archive => "ARCHIVE",
            Self::Notify => "NOTIFY",
            Self::Heartbeat => "HEARTBEAT",
        }
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What went wrong — the *what* axis (§9).
///
/// Grouped by the domain each kind normally belongs to, but the grouping is
/// documentation only: the authoritative domain is the [`FaultDomain`] the call
/// site pairs it with.
///
/// Four kinds are deliberately not failures despite living in this enum; see the
/// note on each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FailureKind {
    // ── Upstream ──
    NotFound,
    Gone,
    Forbidden,
    BotChallenge,
    AuthRequired,
    RateLimited,
    ServerError,
    Timeout,
    ConnectFailed,
    DnsFailed,
    TlsError,
    WrongMediaType,
    MalformedBody,
    EmptyBody,

    // ── Adapter ──
    ParseFailed,
    RequiredFieldMissing,
    ArrayPathMissing,
    NormalizeFailed,
    PlausibilityFailed,
    /// **Not a failure** (INV-11). Emitted as `API_CHANGED` telemetry; the poll
    /// succeeds.
    ShapeChanged,

    // ── Infra ──
    DbThrottled,
    /// On a transition this means *already applied by a prior attempt*, which is a
    /// **success signal**, not a failure (§13.5).
    DbConditionalCheckFailed,
    DbAccessDenied,
    DbFailed,
    /// **Not an error.** Another invocation legitimately owns the source. Log at
    /// debug and do not count it as a failure.
    LeaseContention,
    TickTimeout,
    ConfigInvalid,
    SecretUnavailable,

    // ── Notify ──
    NotifySendFailed,
    NotifyRateLimited,
    NotifyAuthFailed,

    // ── Archive ──
    /// Never invalidates a poll (INV-6 corollary). It degrades the archive
    /// subsystem only.
    ArchivePutFailed,
}

impl FailureKind {
    /// The durable wire name. Agrees with the serde representation by
    /// construction; see the crate-level note on wire names.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotFound => "NOT_FOUND",
            Self::Gone => "GONE",
            Self::Forbidden => "FORBIDDEN",
            Self::BotChallenge => "BOT_CHALLENGE",
            Self::AuthRequired => "AUTH_REQUIRED",
            Self::RateLimited => "RATE_LIMITED",
            Self::ServerError => "SERVER_ERROR",
            Self::Timeout => "TIMEOUT",
            Self::ConnectFailed => "CONNECT_FAILED",
            Self::DnsFailed => "DNS_FAILED",
            Self::TlsError => "TLS_ERROR",
            Self::WrongMediaType => "WRONG_MEDIA_TYPE",
            Self::MalformedBody => "MALFORMED_BODY",
            Self::EmptyBody => "EMPTY_BODY",
            Self::ParseFailed => "PARSE_FAILED",
            Self::RequiredFieldMissing => "REQUIRED_FIELD_MISSING",
            Self::ArrayPathMissing => "ARRAY_PATH_MISSING",
            Self::NormalizeFailed => "NORMALIZE_FAILED",
            Self::PlausibilityFailed => "PLAUSIBILITY_FAILED",
            Self::ShapeChanged => "SHAPE_CHANGED",
            Self::DbThrottled => "DB_THROTTLED",
            Self::DbConditionalCheckFailed => "DB_CONDITIONAL_CHECK_FAILED",
            Self::DbAccessDenied => "DB_ACCESS_DENIED",
            Self::DbFailed => "DB_FAILED",
            Self::LeaseContention => "LEASE_CONTENTION",
            Self::TickTimeout => "TICK_TIMEOUT",
            Self::ConfigInvalid => "CONFIG_INVALID",
            Self::SecretUnavailable => "SECRET_UNAVAILABLE",
            Self::NotifySendFailed => "NOTIFY_SEND_FAILED",
            Self::NotifyRateLimited => "NOTIFY_RATE_LIMITED",
            Self::NotifyAuthFailed => "NOTIFY_AUTH_FAILED",
            Self::ArchivePutFailed => "ARCHIVE_PUT_FAILED",
        }
    }
}

impl fmt::Display for FailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Structured facts rendered into the alert (§9).
///
/// Every field is optional or defaults to empty and the struct derives [`Default`],
/// so a call site fills only the fields it actually has:
///
/// ```
/// use jobmon_errors::Detail;
///
/// let d = Detail { http_status: Some(404), ..Detail::default() };
/// ```
///
/// **There is deliberately no field for a raw response body.** INV-14 forbids raw
/// upstream bodies reaching CloudWatch Logs; a body belongs in the S3 snapshot and
/// is referenced here only by [`Detail::snapshot_key`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Detail {
    pub http_status: Option<u16>,
    pub content_type: Option<String>,
    pub response_bytes: Option<usize>,
    /// A `std::time::Duration`, not a `chrono::Duration`: §17 confines this crate
    /// to `std` and `serde`, so `chrono` is not permitted here.
    pub retry_after: Option<Duration>,
    pub prev_job_count: Option<usize>,
    pub parsed_count: Option<usize>,
    pub shape_hash_prev: Option<String>,
    pub shape_hash_new: Option<String>,
    pub missing_paths: Vec<String>,
    /// `(adapter name, adapter version)`, exactly as §9 writes it.
    pub adapter: Option<(&'static str, u32)>,
    pub snapshot_key: Option<String>,
    pub aws_error_code: Option<String>,
    pub message: String,
}

/// A failure located on all three §9 axes, plus the structured facts an alert
/// needs and the source it happened to.
///
/// `source_id` is `None` for failures that are not attributable to one source —
/// `TickTimeout`, a `Persist` failure during tick close, a `SourceId` that failed
/// validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PipelineError {
    pub stage: Stage,
    pub domain: FaultDomain,
    pub kind: FailureKind,
    pub detail: Detail,
    pub source_id: Option<SourceId>,
}

impl PipelineError {
    /// Builds an error whose [`Detail`] carries only `message`.
    #[must_use]
    pub fn new(
        stage: Stage,
        domain: FaultDomain,
        kind: FailureKind,
        message: impl Into<String>,
    ) -> Self {
        Self {
            stage,
            domain,
            kind,
            detail: Detail {
                message: message.into(),
                ..Detail::default()
            },
            source_id: None,
        }
    }

    /// Attributes the error to a source.
    #[must_use]
    pub fn with_source_id(mut self, source_id: SourceId) -> Self {
        self.source_id = Some(source_id);
        self
    }

    /// Replaces the [`Detail`].
    ///
    /// An empty `detail.message` keeps the message already set by
    /// [`PipelineError::new`], so the obvious call site —
    /// `new(..., "404 from endpoint").with_detail(Detail { http_status: Some(404),
    /// ..Default::default() })` — does not silently discard it.
    #[must_use]
    pub fn with_detail(mut self, detail: Detail) -> Self {
        let message = self.detail.message;
        self.detail = detail;
        if self.detail.message.is_empty() {
            self.detail.message = message;
        }
        self
    }
}

/// Renders `STAGE/DOMAIN/KIND: message` — the three wire names plus
/// [`Detail::message`].
///
/// It renders nothing else on purpose. INV-14 forbids raw upstream response bodies
/// from reaching CloudWatch Logs, and [`Detail`] deliberately has no body field, so
/// nothing reachable from this impl can carry one. Any future field able to hold
/// upstream bytes must stay out of it.
impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{}/{}: {}",
            self.stage.as_str(),
            self.domain.as_str(),
            self.kind.as_str(),
            self.detail.message
        )
    }
}

// Implemented by hand: §17 forbids any dependency here beyond `std` and `serde`,
// so no `thiserror`, no `anyhow`. There is no wrapped cause to return from
// `source()` — a `PipelineError` is the taxonomy's terminal classification of a
// failure, not a link in a chain.
impl std::error::Error for PipelineError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire name a variant serializes to and the one `as_str()` returns must be
    /// the same string, and must survive a round trip: §13.2.3 hashes these names
    /// into durable event keys, so a disagreement is a silent key corruption.
    fn assert_wire_name_agrees<T>(value: T, as_str: &str)
    where
        T: Serialize + serde::de::DeserializeOwned + PartialEq + fmt::Debug + Copy,
    {
        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(
            json,
            format!("\"{as_str}\""),
            "serde name disagrees with as_str()"
        );
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(back, value, "{as_str} does not round-trip");
    }

    #[test]
    fn stage_wire_names_agree_with_serde() {
        let all = [
            Stage::Scheduler,
            Stage::Claim,
            Stage::Connect,
            Stage::Http,
            Stage::Decode,
            Stage::Parse,
            Stage::Schema,
            Stage::Normalize,
            Stage::Plausibility,
            Stage::Persist,
            Stage::Archive,
            Stage::Notify,
            Stage::Heartbeat,
        ];
        for stage in all {
            // Exhaustiveness guard: a new `Stage` variant fails to compile here
            // until it is also added to `all` above.
            match stage {
                Stage::Scheduler
                | Stage::Claim
                | Stage::Connect
                | Stage::Http
                | Stage::Decode
                | Stage::Parse
                | Stage::Schema
                | Stage::Normalize
                | Stage::Plausibility
                | Stage::Persist
                | Stage::Archive
                | Stage::Notify
                | Stage::Heartbeat => {}
            }
            assert_wire_name_agrees(stage, stage.as_str());
        }
    }

    #[test]
    fn fault_domain_wire_names_agree_with_serde() {
        let all = [
            FaultDomain::Upstream,
            FaultDomain::Adapter,
            FaultDomain::Infra,
            FaultDomain::Notify,
            FaultDomain::Archive,
        ];
        for domain in all {
            // Exhaustiveness guard: a new `FaultDomain` variant fails to compile
            // here until it is also added to `all` above.
            match domain {
                FaultDomain::Upstream
                | FaultDomain::Adapter
                | FaultDomain::Infra
                | FaultDomain::Notify
                | FaultDomain::Archive => {}
            }
            assert_wire_name_agrees(domain, domain.as_str());
        }
    }

    #[test]
    fn failure_kind_wire_names_agree_with_serde() {
        let all = [
            FailureKind::NotFound,
            FailureKind::Gone,
            FailureKind::Forbidden,
            FailureKind::BotChallenge,
            FailureKind::AuthRequired,
            FailureKind::RateLimited,
            FailureKind::ServerError,
            FailureKind::Timeout,
            FailureKind::ConnectFailed,
            FailureKind::DnsFailed,
            FailureKind::TlsError,
            FailureKind::WrongMediaType,
            FailureKind::MalformedBody,
            FailureKind::EmptyBody,
            FailureKind::ParseFailed,
            FailureKind::RequiredFieldMissing,
            FailureKind::ArrayPathMissing,
            FailureKind::NormalizeFailed,
            FailureKind::PlausibilityFailed,
            FailureKind::ShapeChanged,
            FailureKind::DbThrottled,
            FailureKind::DbConditionalCheckFailed,
            FailureKind::DbAccessDenied,
            FailureKind::DbFailed,
            FailureKind::LeaseContention,
            FailureKind::TickTimeout,
            FailureKind::ConfigInvalid,
            FailureKind::SecretUnavailable,
            FailureKind::NotifySendFailed,
            FailureKind::NotifyRateLimited,
            FailureKind::NotifyAuthFailed,
            FailureKind::ArchivePutFailed,
        ];
        for kind in all {
            // Exhaustiveness guard: a new `FailureKind` variant fails to compile
            // here until it is also added to `all` above.
            match kind {
                FailureKind::NotFound
                | FailureKind::Gone
                | FailureKind::Forbidden
                | FailureKind::BotChallenge
                | FailureKind::AuthRequired
                | FailureKind::RateLimited
                | FailureKind::ServerError
                | FailureKind::Timeout
                | FailureKind::ConnectFailed
                | FailureKind::DnsFailed
                | FailureKind::TlsError
                | FailureKind::WrongMediaType
                | FailureKind::MalformedBody
                | FailureKind::EmptyBody
                | FailureKind::ParseFailed
                | FailureKind::RequiredFieldMissing
                | FailureKind::ArrayPathMissing
                | FailureKind::NormalizeFailed
                | FailureKind::PlausibilityFailed
                | FailureKind::ShapeChanged
                | FailureKind::DbThrottled
                | FailureKind::DbConditionalCheckFailed
                | FailureKind::DbAccessDenied
                | FailureKind::DbFailed
                | FailureKind::LeaseContention
                | FailureKind::TickTimeout
                | FailureKind::ConfigInvalid
                | FailureKind::SecretUnavailable
                | FailureKind::NotifySendFailed
                | FailureKind::NotifyRateLimited
                | FailureKind::NotifyAuthFailed
                | FailureKind::ArchivePutFailed => {}
            }
            assert_wire_name_agrees(kind, kind.as_str());
        }
    }

    #[test]
    fn source_id_rejects_empty_and_control_bytes() {
        // "   " covers §9's all-ASCII-whitespace rule; the other three cover the
        // §13.2.1 separators and DEL.
        for bad in ["", "   ", "src\u{1f}id", "src\u{1e}id", "src\u{7f}id"] {
            let err = SourceId::new(bad).expect_err("must be rejected");
            assert_eq!(err.stage, Stage::Scheduler);
            assert_eq!(err.domain, FaultDomain::Infra);
            assert_eq!(err.kind, FailureKind::ConfigInvalid);
        }
        assert!(SourceId::new("cohere-greenhouse").is_ok());
    }

    #[test]
    fn accepted_source_id_round_trips_through_as_str() {
        let id = SourceId::new("cohere-greenhouse").unwrap();
        assert_eq!(id.as_str(), "cohere-greenhouse");
        assert_eq!(id.to_string(), "cohere-greenhouse");

        // The exact bytes survive: no trimming, no case folding (§13.2.1).
        let preserved = SourceId::new(" Cohere-Greenhouse ").unwrap();
        assert_eq!(preserved.as_str(), " Cohere-Greenhouse ");
    }
}
