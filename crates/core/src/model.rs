//! The Phase-1 domain model (§17.3, §17.3.1).
//!
//! `jobmon-core` owns every data type that crosses a crate boundary; the other
//! crates own only behaviour (§17.3). `ports` may depend only on `core` and
//! `errors`, and `adapters` likewise, so anything a port signature or an adapter
//! signature names has to live here. That rule is what fixes this module's
//! contents rather than any independent judgement about cohesion.
//!
//! # Wire names are durable schema
//!
//! Every enum below exposes `as_str()` returning its canonical wire name and
//! derives serde with a `rename_all` rule chosen so that the serialized form and
//! `as_str()` agree *by construction* rather than by review — the same discipline
//! `jobmon-errors` applies to its three axes. The spellings are not uniform
//! across enums because the specification is not: §16.2 stores `state` as
//! `active`/`inactive` and `outcome` as `SUCCESS`/`NOT_MODIFIED`, §8 names health
//! states in capitals, §20 spells `bootstrap_mode` in snake case. Each enum here
//! reproduces the spelling of the section that owns it.
//!
//! Three of them are load-bearing beyond storage:
//!
//! - [`EventType`] — §13.2.3 hashes the wire name into every durable event key,
//!   so a rename breaks INV-2 for every key minted before the change.
//! - [`EmploymentType`] — §21.1.1 hashes the wire name into `content_hash`, so a
//!   rename silently re-hashes every stored job and fabricates `JOB_UPDATED`
//!   transitions for all of them.
//! - [`CountryClass`] and [`PollOutcome`] — persisted verbatim as the `country`
//!   and `outcome` attributes of §16.2.
//!
//! # What is deliberately absent
//!
//! `Event` is a Phase-3 type (§17.3). Phase 1 stops at [`Transition`] +
//! [`JobWrite`], which is the authoritative job-transition payload, precisely so
//! that this phase cannot freeze an incomplete §16.2 event envelope before the
//! engine and the notification-recovery path need the whole of it. `HttpRequest`,
//! `HttpResponse` and `CacheValidators` are Phase 2, added when `build_request`
//! first needs them.

use chrono::{DateTime, Utc};
use jobmon_errors::{FailureKind, FaultDomain, PipelineError, SourceId, Stage};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, btree_map};
use std::fmt;

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// An upstream job identifier, validated at construction.
///
/// # Why validation lives in the constructor
///
/// `external_id` is a component of every job event key, and §13.2.1 forbids any
/// component from containing `0x1F` or `0x1E` — the identity separators. An
/// upstream id carrying one could shift the field boundaries of a durable key, so
/// rejecting at the boundary is what keeps INV-2 unconditional instead of
/// dependent on upstream data hygiene. The rule is deliberately wider than the two
/// separators: every ASCII control byte is rejected, which is the same stance
/// [`SourceId`] takes for operator-supplied identities.
///
/// The supplied bytes are otherwise preserved exactly — no trimming, no case
/// folding, no Unicode normalization (§21.1). Classification and tokenization
/// operate on derived views; the stored value stays the upstream string.
///
/// [`Ord`] is derived and is byte-lexicographic over UTF-8, which is what §13.4
/// requires when sorting transitions before chunking and what gives [`JobIndex`]
/// its iteration order.
///
/// # Serde
///
/// [`Serialize`] is transparent. [`Deserialize`] is written by hand so that it
/// routes through [`ExternalId::new`]: a derived implementation would let an id
/// containing `0x1F` re-enter the system from stored data and defeat the whole
/// point of validating at all.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ExternalId(String);

impl ExternalId {
    /// Validates `raw` and wraps it.
    ///
    /// # Errors
    ///
    /// Rejects an empty string, a string that is entirely ASCII whitespace, and
    /// any string containing an ASCII control byte (`0x00..=0x1F` or `0x7F`).
    /// Rejection is `Stage::Normalize` / `FaultDomain::Adapter` /
    /// `FailureKind::NormalizeFailed` (§21.1): an unusable `external_id` is the
    /// upstream payload or the adapter's reading of it being wrong, observed at
    /// the normalization boundary.
    pub fn new(raw: &str) -> Result<Self, PipelineError> {
        let reject = |why: &str| -> Result<Self, PipelineError> {
            Err(PipelineError::new(
                Stage::Normalize,
                FaultDomain::Adapter,
                FailureKind::NormalizeFailed,
                // `{raw:?}` escapes control bytes as `\u{...}` rather than
                // emitting them raw into a log line.
                format!("invalid external_id ({why}): {raw:?}"),
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

impl fmt::Display for ExternalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ExternalId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::new(&raw).map_err(D::Error::custom)
    }
}

/// [`SourceId`] carries the same construction-time validation as [`ExternalId`]
/// but lives in `jobmon-errors`, which derives no serde for it. §16.2 persists it
/// as a plain string attribute, so [`SourceConfig`] routes the field through this
/// module to keep the validated constructor on the deserialization path.
mod source_id_serde {
    use serde::de::Error as _;

    use super::{Deserialize, Deserializer, Serializer, SourceId};

    pub(super) fn serialize<S>(id: &SourceId, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(id.as_str())
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<SourceId, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        SourceId::new(&raw).map_err(D::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Enumerations
// ---------------------------------------------------------------------------

/// How a source delivers work (§20). V1 is always [`SourceKind::Pull`]; `Push`
/// exists so that adding a webhook source later is a data change rather than a
/// schema migration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum SourceKind {
    Pull,
    Push,
}

impl SourceKind {
    /// The durable wire name (§16.2).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pull => "Pull",
            Self::Push => "Push",
        }
    }
}

impl fmt::Display for SourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The declared blind spot for a source, expressed as a validated ceiling on its
/// polling interval (§10.2).
///
/// Criticality is deliberately not a second free-floating knob that can silently
/// contradict the interval. It is the ceiling, so an operator who needs to poll a
/// fragile API slowly has to downgrade criticality explicitly and thereby record
/// the reliability tradeoff as a decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum Criticality {
    Critical,
    Standard,
    Background,
}

impl Criticality {
    /// The largest `effective_interval` this criticality permits, in seconds
    /// (§10.2).
    ///
    /// The ceiling is enforced by `admin add-source` and re-checked at tick start;
    /// the check itself is `core::schedule`'s, because it produces a
    /// `Stage::Scheduler` / `FaultDomain::Infra` / `FailureKind::ConfigInvalid`
    /// failure rather than a value.
    #[must_use]
    pub fn max_interval_secs(&self) -> u32 {
        match self {
            Self::Critical => 300,
            Self::Standard => 600,
            Self::Background => 1800,
        }
    }

    /// The failure-detection SLA, in seconds — **derived, never stored** (§10.2,
    /// §20).
    ///
    /// It is numerically identical to [`Criticality::max_interval_secs`] by
    /// definition: the worst case for noticing that an upstream broke is one whole
    /// polling interval, and the interval may not exceed the ceiling. Both names
    /// exist because they answer different questions — one bounds configuration,
    /// the other is quoted to the owner as "Blind spot: N min" in a
    /// `SOURCE_FAILED` alert.
    #[must_use]
    pub fn failure_detection_sla_secs(&self) -> u32 {
        self.max_interval_secs()
    }

    /// The durable wire name (§16.2).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Critical => "Critical",
            Self::Standard => "Standard",
            Self::Background => "Background",
        }
    }
}

impl fmt::Display for Criticality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A source's position in the §8 health state machine.
///
/// The transitions between these states are §8.1's table and belong to
/// `core::health`, which is the only Phase-1 module permitted to mutate
/// `consecutive_failures`, `probe_attempts` and `first_failure_at`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HealthState {
    Initializing,
    Healthy,
    Degraded,
    Failed,
    Quarantined,
    Disabled,
}

impl HealthState {
    /// The durable wire name (§8, §16.2).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Initializing => "INITIALIZING",
            Self::Healthy => "HEALTHY",
            Self::Degraded => "DEGRADED",
            Self::Failed => "FAILED",
            Self::Quarantined => "QUARANTINED",
            Self::Disabled => "DISABLED",
        }
    }
}

impl fmt::Display for HealthState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a stored job is still on the board (§16.2).
///
/// A job leaves `Active` only through a `JOB_REMOVED` transition, which requires
/// two consecutive polls of absence (§13.8), and returns through `JOB_REPOSTED`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Active,
    Inactive,
}

impl JobState {
    /// The durable wire name (§16.2).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Inactive => "inactive",
        }
    }
}

impl fmt::Display for JobState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How far a source has progressed through its baseline load (§7, §13.6).
///
/// This — not [`HealthState`] — is authoritative for choosing the pipeline branch
/// (INV-10). While it is not [`BootstrapState::Complete`], normal diffing is
/// forbidden, because a recovered-but-unbootstrapped source would diff against an
/// empty index and produce a `NEW_JOB` storm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapState {
    Pending,
    InProgress,
    Complete,
}

impl BootstrapState {
    /// The durable wire name (§16.2).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Complete => "complete",
        }
    }
}

impl fmt::Display for BootstrapState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a source is allowed to say when its baseline load finishes (§20, §13.6).
///
/// The default is deliberately the middle option: a brand-new source's entire
/// current board is not news, but silently swallowing it hides the fact that
/// bootstrap ran at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapMode {
    /// No message at all.
    Silent,
    /// One summary naming how many existing postings are relevant (§20 default).
    #[default]
    RelevantSummary,
    /// One job alert per pre-existing relevant posting.
    NotifyExistingRelevant,
}

impl BootstrapMode {
    /// The durable wire name (§20, §16.2).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Silent => "silent",
            Self::RelevantSummary => "relevant_summary",
            Self::NotifyExistingRelevant => "notify_existing_relevant",
        }
    }
}

impl fmt::Display for BootstrapMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a durable event sits in the §15 notification delivery state machine.
///
/// [`NotifyState::Unsent`] and [`NotifyState::Claimed`] are the two states that
/// carry the `NOTIFY#OPEN` GSI2 membership, so they are exactly the states the
/// sweeper can see; [`NotifyState::Sent`] drops out of that view and
/// [`NotifyState::Na`] never enters it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyState {
    Unsent,
    Claimed,
    Sent,
    /// Not applicable — the event is durable but not notify-worthy (§14).
    Na,
}

impl NotifyState {
    /// The durable wire name (§16.2).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unsent => "unsent",
            Self::Claimed => "claimed",
            Self::Sent => "sent",
            Self::Na => "na",
        }
    }
}

impl fmt::Display for NotifyState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The normalized employment class of a posting (§21.1).
///
/// **The wire names below are hashed into `content_hash` (§21.1.1).** Renaming one
/// re-hashes every stored job carrying it and manufactures a `JOB_UPDATED`
/// transition for each — a stored-data migration, not a cosmetic refactor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmploymentType {
    Internship,
    CoOp,
    NewGrad,
    FullTime,
    PartTime,
    Contract,
    /// Neither `employment_type_raw` nor the title classified. Not a failure —
    /// the relevance predicate falls back to title matching (§21.2).
    Unknown,
}

impl EmploymentType {
    /// The canonical wire name, hashed into `content_hash` by §21.1.1.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Internship => "internship",
            Self::CoOp => "co_op",
            Self::NewGrad => "new_grad",
            Self::FullTime => "full_time",
            Self::PartTime => "part_time",
            Self::Contract => "contract",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for EmploymentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The two-way outcome of §21.1's location classification.
///
/// The third outcome — *unresolved* — is the **absence** of this value, which is
/// why `country` is an `Option` everywhere it appears and why §16.2 stores the
/// attribute sparsely. Encoding "unknown" as a variant would let it be confused
/// with a decision that was actually made, and §21.2's rule 4 exists precisely to
/// treat "we could not tell" differently from "we determined not Canada".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CountryClass {
    /// Wire name `CA`.
    Ca,
    /// Wire name `NOT_CA`.
    NotCa,
}

impl CountryClass {
    /// The durable wire name (§16.2).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ca => "CA",
            Self::NotCa => "NOT_CA",
        }
    }
}

impl fmt::Display for CountryClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What to do with a posting whose location resolved to neither Canada nor a
/// recognised non-Canadian marker (§21.2 rule 4).
///
/// The default is [`UnresolvedLocationPolicy::Relevant`] — fail open. §2 ranks
/// *never silently miss a relevant posting* above *avoid noise*, and a filter
/// false negative is invisible: nothing fails, nothing is logged, the posting
/// simply never arrives. Failing open converts that invisible error into noise the
/// owner can see, bounded by a narrow class, an explicit `⚠ location unparsed`
/// marker on the alert (§15.2), and the per-source counters of §16.2.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnresolvedLocationPolicy {
    /// §20 default: fail open.
    #[default]
    Relevant,
    NotRelevant,
}

impl UnresolvedLocationPolicy {
    /// The durable wire name (§20, §16.2).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Relevant => "relevant",
            Self::NotRelevant => "not_relevant",
        }
    }
}

impl fmt::Display for UnresolvedLocationPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The source-processing classification of one poll (§17.3.1), used by
/// `core::health`, `core::schedule` and §16.2 `POLL` telemetry.
///
/// Mapping from [`FailureKind`] is deliberately **partial**. The transient source
/// kinds map to [`PollOutcome::Transient`], `RateLimited` maps to
/// [`PollOutcome::RateLimited`], the §10.4 source hard-failure kinds map to
/// [`PollOutcome::Hard`] — and `ShapeChanged`, the lease/idempotency success
/// signals, system and infra faults, notification faults and archive faults have
/// **no** `PollOutcome` at all. That gap is INV-6 and INV-11: a schema change is
/// not a source failure, and our own database or Telegram being broken must never
/// be recorded against the upstream's health.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PollOutcome {
    Success,
    /// HTTP 304 — the conditional request was honoured and there is nothing to
    /// diff. A success for health purposes (§8.1).
    NotModified,
    Transient,
    Hard,
    RateLimited,
}

impl PollOutcome {
    /// The durable wire name of the §16.2 `outcome` attribute.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "SUCCESS",
            Self::NotModified => "NOT_MODIFIED",
            Self::Transient => "TRANSIENT",
            Self::Hard => "HARD",
            Self::RateLimited => "RATE_LIMITED",
        }
    }

    /// Whether the poll counts as a success for §8.1's state machine.
    ///
    /// A 304 is a success: the upstream answered, correctly, that nothing changed.
    /// Treating it as anything else would push a perfectly healthy, well-cached
    /// source towards `DEGRADED` for behaving exactly as intended.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success | Self::NotModified)
    }
}

impl fmt::Display for PollOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The sixteen durable event types of §14, in §14 order.
///
/// **These wire names are the event identity.** §13.2.3 hashes the canonical name
/// into every event key, and it is deliberately the same string this enum
/// serializes to and `as_str()` returns: v1.1 abbreviated six of them in §14
/// (`DEGRADED` for `SOURCE_DEGRADED`, `BOOTSTRAP` for `SOURCE_BOOTSTRAPPED`,
/// `NOTIFY_DEGRADED` for `NOTIFICATION_DEGRADED`), which meant an implementer
/// reaching for the obvious `as_str()` would silently mint the wrong key. Aligning
/// them makes the obvious implementation the correct one.
///
/// The full `Event` carrier — the §16.2 `EVT#` envelope with `detected_at`, `ulid`,
/// `before`/`after` blocks and the `notify_state` lifecycle — is a Phase-3 type
/// (§17.3). Only the type tag is needed in Phase 1, because diff and health
/// identity depend on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EventType {
    NewJob,
    BecameRelevant,
    JobReposted,
    JobUpdated,
    BecameIrrelevant,
    JobRemoved,
    SourceBootstrapped,
    SourceDegraded,
    SourceFailed,
    SourceRecovered,
    SourceQuarantined,
    ApiChanged,
    SystemDegraded,
    NotificationDegraded,
    NotificationRecovered,
    FilterChanged,
}

impl EventType {
    /// The canonical wire name — the string §13.2.3 hashes into the event key.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NewJob => "NEW_JOB",
            Self::BecameRelevant => "BECAME_RELEVANT",
            Self::JobReposted => "JOB_REPOSTED",
            Self::JobUpdated => "JOB_UPDATED",
            Self::BecameIrrelevant => "BECAME_IRRELEVANT",
            Self::JobRemoved => "JOB_REMOVED",
            Self::SourceBootstrapped => "SOURCE_BOOTSTRAPPED",
            Self::SourceDegraded => "SOURCE_DEGRADED",
            Self::SourceFailed => "SOURCE_FAILED",
            Self::SourceRecovered => "SOURCE_RECOVERED",
            Self::SourceQuarantined => "SOURCE_QUARANTINED",
            Self::ApiChanged => "API_CHANGED",
            Self::SystemDegraded => "SYSTEM_DEGRADED",
            Self::NotificationDegraded => "NOTIFICATION_DEGRADED",
            Self::NotificationRecovered => "NOTIFICATION_RECOVERED",
            Self::FilterChanged => "FILTER_CHANGED",
        }
    }

    /// Whether an event of this type must be delivered to a human, per §14's
    /// Notify column.
    ///
    /// `relevant` is the job's relevance decision and is consulted by exactly the
    /// three job types whose §14 entry reads "if `relevant`". It is ignored by
    /// every other type, including `BECAME_RELEVANT` — which notifies
    /// unconditionally because reaching it *is* the relevance transition.
    ///
    /// A notify-worthy event sets `GSI2PK = "NOTIFY#OPEN"` inside the creating
    /// transaction (INV-3); the others omit the GSI2 attributes entirely and never
    /// enter the sweeper's view. Throttling — `API_CHANGED` at most once per source
    /// per day, `SOURCE_FAILED` re-alerts at most every 6 h — is a §15 delivery
    /// policy applied later and does not make an event un-notify-worthy here.
    #[must_use]
    pub fn notify_worthy(&self, relevant: bool) -> bool {
        match self {
            Self::NewJob | Self::JobReposted => relevant,
            Self::JobUpdated | Self::BecameIrrelevant | Self::JobRemoved => false,
            Self::BecameRelevant
            | Self::SourceBootstrapped
            | Self::SourceDegraded
            | Self::SourceFailed
            | Self::SourceRecovered
            | Self::SourceQuarantined
            | Self::ApiChanged
            | Self::SystemDegraded
            | Self::NotificationDegraded
            | Self::NotificationRecovered
            | Self::FilterChanged => true,
        }
    }

    /// Whether the event lives in the fixed `SYS#EVT` partition rather than under
    /// a source (§16.1 note 4).
    ///
    /// These four are not attributable to one source, so they have no `SRC#<id>`
    /// partition to live in. v1.1 gave them identities in §14 but no home in the
    /// table, which left four notify-worthy types outside the sweeper's view and
    /// INV-3 unsatisfiable for them. System-scoped events use the literal scope
    /// component `"SYS"` in their key derivation (§13.2.3).
    #[must_use]
    pub fn is_system_scoped(&self) -> bool {
        matches!(
            self,
            Self::SystemDegraded
                | Self::NotificationDegraded
                | Self::NotificationRecovered
                | Self::FilterChanged
        )
    }
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// The adapter-specific half of a source's configuration (§20).
///
/// `fields` carries the keys one ATS family needs — `board`, `token`, `tenant`,
/// `site`, `url` — and `headers` carries request headers. Both are
/// [`BTreeMap`]s rather than [`std::collections::HashMap`]s so that iteration
/// order is deterministic: this is configuration that ends up inside request
/// construction and diagnostics, and a nondeterministic order would make two
/// identical configurations render differently.
///
/// This is what makes D10 work — twenty Greenhouse companies share one adapter and
/// differ only in here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointConfig {
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// Per-source relaxations of the §21.2 location gate (§20).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FilterOverrides {
    /// When true, a posting whose location is marked remote passes the location
    /// gate even when the resolved country class is `NOT_CA` — for employers whose
    /// remote roles are known to be Canada-eligible. §20 default `false`.
    pub accept_remote_canada: bool,
    /// Governs a posting whose location could not be classified at all. §20
    /// default [`UnresolvedLocationPolicy::Relevant`].
    pub unresolved_location: UnresolvedLocationPolicy,
}

/// The §22 sanity floor on a parsed job count (§20).
///
/// This is what stops an upstream that starts returning an empty array from being
/// read as "every job was removed" and emitting a `JOB_REMOVED` storm.
///
/// Not [`Eq`]: `min_ratio` is an `f32`, and a total equality relation over floats
/// would be a lie.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlausibilityConfig {
    /// Reject when `parsed < min_ratio * last_job_count`. §20 default `0.5`.
    pub min_ratio: f32,
    /// Never reject when the counts are this small — tiny boards legitimately
    /// fluctuate. §20 default `3`.
    pub min_abs: u32,
    /// When true, a zero parsed count is explicitly accepted even if the previous
    /// count was large; nonzero collapses still use `min_ratio`/`min_abs`. §20
    /// default `false`. Only for boards that legitimately empty out.
    pub allow_zero: bool,
}

impl Default for PlausibilityConfig {
    fn default() -> Self {
        Self {
            min_ratio: 0.5,
            min_abs: 3,
            allow_zero: false,
        }
    }
}

/// The global filter version under which relevance is being computed (§21.3).
///
/// Every job stores the version its `relevant` flag was computed under. On a bump,
/// jobs are re-evaluated at their next poll but the resulting changes are routed
/// into one `FILTER_CHANGED` summary rather than individual `BECAME_RELEVANT`
/// alerts (INV-15) — without that, editing the filter fabricates hundreds of fake
/// transitions and buries the channel.
///
/// The version covers normalization as well as the filter: the province table, the
/// city list, the country list and the employment-type patterns determine
/// `relevant` just as directly as the keyword lists do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FilterConfig {
    pub filter_version: u32,
}

/// What one adapter depends on structurally — its *own* dependencies, not the
/// upstream's whole schema (§18).
///
/// This distinction is INV-11. A new sibling field appearing changes `shape_hash`
/// and produces `API_CHANGED` telemetry while the poll succeeds normally; a
/// `required_path` disappearing is `RequiredFieldMissing` and an immediate
/// `SOURCE_FAILED`. Validating structural equality instead would alert on every
/// harmless upstream addition until the alerts were ignored.
///
/// # Serde
///
/// [`Serialize`] only. The `&'static str` fields are compile-time adapter metadata
/// — a contract is declared in code alongside the parser that depends on it, not
/// loaded from configuration — so a [`Deserialize`] impl would have nowhere to
/// borrow from and would imply an operator could weaken a parser's own
/// preconditions from the database.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct AdapterContract {
    /// JSON path to the array of postings: `"jobs"`, `"data.postings"`, or `""`
    /// when the document root is itself the array.
    pub array_path: &'static str,
    /// Paths the parser dereferences, relative to one array element.
    pub required_paths: &'static [&'static str],
    /// Sanity floor for a first or bootstrap poll, where §22's plausibility check
    /// has no previous count to compare against (§7).
    pub min_expected: usize,
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

/// One posting exactly as an adapter read it (§17.3.1).
///
/// Adapters produce `RawJob`; `core::normalize` produces [`NormalizedJob`]; the
/// filter is a single pure predicate over the latter. That layering is what makes
/// one filter work across Greenhouse, Lever, Ashby and every future adapter —
/// filtering never touches ATS-specific fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawJob {
    pub external_id: String,
    pub title: String,
    pub location_raw: String,
    pub employment_type_raw: Option<String>,
    pub url: String,
    pub posted_at: Option<DateTime<Utc>>,
}

/// One posting after §21.1's normalization contract has been applied.
///
/// `external_id`, `title`, `location_raw` and `url` are preserved exactly as
/// supplied — normalization classifies into new fields rather than rewriting the
/// originals, so the stored content is always the upstream string and
/// classification operates on derived views.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedJob {
    pub external_id: ExternalId,
    pub title: String,
    pub location_raw: String,
    /// `None` means *unresolved* — neither Canada nor a recognised non-Canadian
    /// marker matched (§21.1). It is not a synonym for `NOT_CA`.
    pub country: Option<CountryClass>,
    /// The matched two-letter province or territory code, present only when the
    /// Canada class was reached through a province match (§21.1).
    pub region: Option<String>,
    /// The leading comma-separated segment, when that segment is not itself a
    /// region or country token (§21.1).
    pub city: Option<String>,
    /// Derived on every poll from `location_raw` and **never persisted** (§21.1,
    /// §16.2): it is recomputable from a field that is stored, so storing it too
    /// would cost a write for nothing.
    pub remote: bool,
    pub employment_type: EmploymentType,
    pub url: String,
    pub posted_at: Option<DateTime<Utc>>,
}

/// The canonical representation of a persisted `JOB#` item (§16.2, §17.3.1).
///
/// `remote` is deliberately absent — see [`NormalizedJob::remote`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub external_id: ExternalId,
    pub title: String,
    pub location_raw: String,
    pub country: Option<CountryClass>,
    pub region: Option<String>,
    pub city: Option<String>,
    pub employment_type: EmploymentType,
    pub url: String,
    pub posted_at: Option<DateTime<Utc>>,
    pub first_seen_at: DateTime<Utc>,
    /// Display only, and updated only when the item is being written anyway
    /// (§13.8). For an active job with no absence marker the true last-seen time is
    /// the source's `last_success_at`, because an absent job would necessarily have
    /// been written.
    pub last_seen_at: DateTime<Utc>,
    pub state: JobState,
    pub relevant: bool,
    pub content_hash: String,
    pub transition_seq: u64,
    /// Sparse (§13.8): written when a present job first goes missing, removed when
    /// it reappears, retained by `JOB_REMOVED` for auditability.
    pub absent_since_poll: Option<u64>,
    pub filter_version: u32,
    pub bootstrapped: bool,
    /// Set to `now + 180 days` when the job goes inactive; cleared in the same
    /// update that reactivates it, or DynamoDB deletes a reposted job (§16.1).
    pub ttl: Option<DateTime<Utc>>,
}

/// The projection of a stored job that diffing actually needs (§17.3.1).
///
/// Intentionally not a whole [`Job`]. The diff index is loaded for every job of
/// every source on every poll, and it is compared, not displayed — carrying
/// `title`, `url` and the location fields through it would multiply the read
/// volume for data no comparison reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobFacts {
    pub state: JobState,
    pub relevant: bool,
    pub content_hash: String,
    pub transition_seq: u64,
    pub absent_since_poll: Option<u64>,
    pub filter_version: u32,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub bootstrapped: bool,
    pub ttl: Option<DateTime<Utc>>,
}

/// Every stored job of one source, keyed by [`ExternalId`] (§17.3.1).
///
/// The [`BTreeMap`] is the point: iteration is byte-lexicographic over UTF-8
/// external ids, which is the order §13.4 requires transitions to be sorted into
/// before chunking. Getting that ordering from the container rather than from a
/// sort call at the end means a caller cannot forget it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobIndex(BTreeMap<ExternalId, JobFacts>);

impl JobIndex {
    /// An empty index — the state a source bootstraps from.
    #[must_use]
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    #[must_use]
    pub fn get(&self, external_id: &ExternalId) -> Option<&JobFacts> {
        self.0.get(external_id)
    }

    /// Inserts `facts`, returning whatever was stored under `external_id` before.
    pub fn insert(&mut self, external_id: ExternalId, facts: JobFacts) -> Option<JobFacts> {
        self.0.insert(external_id, facts)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterates in external-id byte order.
    pub fn iter(&self) -> btree_map::Iter<'_, ExternalId, JobFacts> {
        self.0.iter()
    }

    /// Iterates the external ids in byte order.
    pub fn keys(&self) -> btree_map::Keys<'_, ExternalId, JobFacts> {
        self.0.keys()
    }
}

// ---------------------------------------------------------------------------
// Persistence decisions
// ---------------------------------------------------------------------------

/// The exact persistence mutation `core::diff` chose for one job (§17.3.1).
///
/// This enum is the boundary that keeps persistence free of business rules.
/// `repo_dynamo` may choose whatever DynamoDB expressions it likes, but it must
/// **not** inspect [`EventType`] to decide whether to set or clear the TTL, the
/// absence marker, the timestamps, relevance or the job fields — those choices are
/// already encoded here. An infra layer that re-derives them is a second copy of
/// the §13.8/§17.3.1 rules that can drift from the first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobWrite {
    /// A job seen for the first time. `first_seen_at == last_seen_at == now`,
    /// `bootstrapped = false`, no TTL.
    PutNew {
        job: NormalizedJob,
        relevant: bool,
        content_hash: String,
        first_seen_at: DateTime<Utc>,
        last_seen_at: DateTime<Utc>,
        transition_seq: u64,
        filter_version: u32,
        bootstrapped: bool,
    },
    /// A present existing job. Preserves `first_seen_at` and `bootstrapped`, sets
    /// state active, and clears the absence marker and inactive TTL — the latter in
    /// the same update, or DynamoDB deletes the job we just reactivated (§16.1).
    UpdateActive {
        job: NormalizedJob,
        relevant: bool,
        content_hash: String,
        last_seen_at: DateTime<Utc>,
        transition_seq: u64,
        filter_version: u32,
        clear_absent_since_poll: bool,
        clear_ttl: bool,
    },
    /// `JOB_REMOVED`. Preserves `first_seen_at`, `last_seen_at`, the
    /// relevance/content/filter facts and `bootstrapped`; **retains** the original
    /// `absent_since_poll` for auditability rather than clearing or refreshing it
    /// (§13.8).
    MarkInactive {
        transition_seq: u64,
        absent_since_poll: u64,
        ttl: DateTime<Utc>,
    },
}

/// One job transition: the complete business decision, ready for §13.4's atomic
/// pair of a job mutation and a durable event (§17.3.1).
///
/// At most one of these exists per job per poll (INV-13). `TransactWriteItems`
/// cannot target the same item twice in one transaction, so a job that
/// simultaneously reappears *and* newly matches the filter is collapsed by §13.3's
/// strict precedence to the single highest-precedence event — while every other
/// changed field still rides along in `job_write` and is recorded in `after`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    pub external_id: ExternalId,
    pub event_type: EventType,
    /// `None` for a job that did not exist before, which is what lets the
    /// conditional write choose between `attribute_not_exists(SK)` and
    /// `transition_seq = :old` (§13.4).
    pub prev_transition_seq: Option<u64>,
    pub new_transition_seq: u64,
    pub before: Option<JobFacts>,
    pub after: JobFacts,
    pub job_write: JobWrite,
    /// [`EventType::notify_worthy`] already resolved against this job's relevance,
    /// so the persistence layer never has to consult the filter (INV-3).
    pub notify_worthy: bool,
}

/// A relevance re-evaluation that produced no transition (§21.3, INV-15).
///
/// When a job's stored `filter_version` differs from the current one, `diff`
/// suppresses `BECAME_RELEVANT` and `BECAME_IRRELEVANT` for it. If some other
/// transition fires anyway the new relevance rides along in that transition's job
/// write; if none fires, the job lands here as a plain idempotent write with no
/// event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterReclassify {
    pub external_id: ExternalId,
    pub relevant: bool,
    pub filter_version: u32,
}

/// Everything §13.4 Phase B writes: plain, idempotent, event-free (§17.3.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NonTransitionWrites {
    /// The value written into every one of `absence_markers`. The repository does
    /// **not** recompute it: `current_poll_seq` is stable across a crash and retry
    /// only because META advances in Phase C alone (§13.4), and a repository that
    /// derived its own would break that.
    pub current_poll_seq: u64,
    /// Jobs absent this poll with no marker yet — one conditional `SET` each.
    pub absence_markers: Vec<ExternalId>,
    /// Jobs present again whose marker must be removed, and for which no
    /// transition already clears it.
    pub absence_clears: Vec<ExternalId>,
    pub filter_reclassify: Vec<FilterReclassify>,
}

// ---------------------------------------------------------------------------
// Source aggregate
// ---------------------------------------------------------------------------

/// The configuration half of `SRC#<id>/META` — data, not code (§16.2, §20).
///
/// Adding a company on an already-supported ATS is an insert here and nothing
/// else: no deployment, no code change (§19 case A).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceConfig {
    #[serde(with = "source_id_serde")]
    pub source_id: SourceId,
    /// Display name used in alerts.
    pub company: String,
    pub source_kind: SourceKind,
    /// Registry key. An unknown value is `ConfigInvalid` in the `Infra` domain —
    /// an operator error that is alerted and skips the source, never a crash
    /// (§18).
    pub adapter_type: String,
    /// Set by the registry at registration and bumped on any parsing-behaviour
    /// change, which forces an S3 snapshot on the next poll so the pre/post
    /// payloads are comparable (§18).
    pub adapter_version: u32,
    pub endpoint_config: EndpointConfig,
    /// `false` removes the source from GSI1 entirely (§16.1).
    pub enabled: bool,
    pub criticality: Criticality,
    /// Must satisfy `<= criticality.max_interval_secs()` (§10.2).
    pub base_interval_secs: u32,
    /// Temporary manual override; the same §10.2 validation applies.
    pub interval_override_secs: Option<u32>,
    pub bootstrap_mode: BootstrapMode,
    #[serde(default)]
    pub filter_overrides: FilterOverrides,
    #[serde(default)]
    pub plausibility: PlausibilityConfig,
    /// Grouping for digests.
    #[serde(default)]
    pub tags: Vec<String>,
}

impl SourceConfig {
    /// The interval actually used, in seconds — **derived, never stored** (§20).
    ///
    /// This only resolves the override; it does **not** check the §10.2 ceiling.
    /// That check belongs to `core::schedule::validate_interval`, because its
    /// result is a `Stage::Scheduler` / `FaultDomain::Infra` /
    /// `FailureKind::ConfigInvalid` failure rather than a number, and §10.2 runs it
    /// at registration *and* again at tick start.
    #[must_use]
    pub fn effective_interval_secs(&self) -> u32 {
        self.interval_override_secs
            .unwrap_or(self.base_interval_secs)
    }

    /// The blind spot quoted to the owner in a `SOURCE_FAILED` alert — **derived,
    /// never stored** (§10.2, §20).
    #[must_use]
    pub fn failure_detection_sla_secs(&self) -> u32 {
        self.criticality.failure_detection_sla_secs()
    }
}

/// The scheduling half of `SRC#<id>/META` (§16.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleState {
    /// Indexed by GSI1 as `DUE`/`<next_check_at>` while the source is enabled.
    pub next_check_at: DateTime<Utc>,
    /// `stored_poll_seq` — *the last poll that successfully committed* (§13.4).
    /// Use [`ScheduleState::current_poll_seq`] for the poll being processed now.
    pub poll_seq: u64,
    pub lease_until: Option<DateTime<Utc>>,
    pub lease_owner: Option<String>,
}

impl ScheduleState {
    /// The logical number of the poll being processed right now — `poll_seq + 1`
    /// (§13.4).
    ///
    /// This is the value written into `absent_since_poll`, the value compared
    /// against it (§13.8), and the value the Phase C commit marker writes. META
    /// advances **only** in Phase C, so a crash anywhere earlier leaves
    /// `stored_poll_seq` untouched and the retry recomputes an identical
    /// `current_poll_seq` — which is exactly what makes absence tracking idempotent
    /// across a crash.
    ///
    /// # Errors
    ///
    /// Overflow of `u64` is `Stage::Persist` / `FaultDomain::Infra` /
    /// `FailureKind::DbFailed`. Saturating or wrapping is forbidden even though the
    /// numerical limit is unreachable in practice: reusing a poll sequence would
    /// silently break every absence and idempotency comparison that assumes the
    /// number only ever moves forward, and a hard failure at the boundary is the
    /// only behaviour that cannot corrupt state.
    pub fn current_poll_seq(&self) -> Result<u64, PipelineError> {
        self.poll_seq.checked_add(1).ok_or_else(|| {
            PipelineError::new(
                Stage::Persist,
                FaultDomain::Infra,
                FailureKind::DbFailed,
                format!(
                    "poll_seq overflow: stored_poll_seq is u64::MAX ({}), so current_poll_seq \
                     cannot advance without reusing a sequence number (§13.4)",
                    self.poll_seq
                ),
            )
        })
    }
}

/// The health half of `SRC#<id>/META` (§16.2), mutated only by `core::health`
/// (§8.1).
///
/// `last_attempt_at` and `last_success_at` are modelled as optional even though
/// §16.2 does not mark them sparse: §7's registration write sets neither, so a
/// source that has never been polled — or has never once succeeded — genuinely has
/// no value for them, and the `SOURCE_FAILED` template of §8 renders "Last
/// success" as an optional line for exactly that case.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthSnapshot {
    pub health_state: HealthState,
    pub failure_stage: Option<Stage>,
    pub failure_domain: Option<FaultDomain>,
    pub failure_kind: Option<FailureKind>,
    pub consecutive_failures: u32,
    /// Priority-probe accounting (§10.3). `core::schedule` reads the *pre-poll*
    /// value and never changes it.
    pub probe_attempts: u32,
    /// Set when `consecutive_failures` moves from 0 to 1 and cleared on any
    /// success. It discriminates the `SOURCE_DEGRADED`/`SOURCE_FAILED`/
    /// `SOURCE_RECOVERED`/`SOURCE_QUARANTINED` event identities (§13.2.3), so one
    /// outage produces one identity per event type no matter how many polls it
    /// spans.
    pub first_failure_at: Option<DateTime<Utc>>,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    /// Throttles the §8 re-alert rules — `FAILED` at most every 6 h, `API_CHANGED`
    /// at most once per source per day.
    pub last_health_alert_at: Option<DateTime<Utc>>,
}

/// The contract/caching half of `SRC#<id>/META` (§16.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractState {
    pub last_etag: Option<String>,
    pub last_modified: Option<String>,
    /// Full 52-character Base32 SHA-256 of the raw body (§21.1.1) — not an event
    /// identity, so never truncated to 26.
    pub last_body_hash: Option<String>,
    /// Full 52-character Base32 SHA-256 over the §18 structured key paths.
    pub last_shape_hash: Option<String>,
    /// The previous parsed count, which §22's plausibility check compares against.
    pub last_job_count: usize,
    pub last_raw_put_at: Option<DateTime<Utc>>,
    pub bootstrap_state: BootstrapState,
    /// The version `relevant` was last computed under for this source (§21.3).
    pub filter_version: u32,
}

/// A whole `SRC#<id>/META` item — one source, all four §16.2 attribute groups.
///
/// The groups are kept as separate structs rather than one flat struct because
/// they have different owners: configuration is written by `admin`, schedule state
/// by `core::schedule`, health by `core::health`, and contract state by the poll
/// pipeline. Splitting them makes each function signature name only what it may
/// touch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub config: SourceConfig,
    pub schedule: ScheduleState,
    pub health: HealthSnapshot,
    pub contract: ContractState,
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// The wire name a variant serializes to and the one `as_str()` returns must
    /// be the same string, and must survive a round trip. §13.2.3 hashes these
    /// names into durable event keys and §21.1.1 hashes one of them into
    /// `content_hash`, so a disagreement is a silent key corruption.
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

    fn facts() -> JobFacts {
        let now = DateTime::parse_from_rfc3339("2026-08-16T10:05:07Z")
            .unwrap()
            .with_timezone(&Utc);
        JobFacts {
            state: JobState::Active,
            relevant: true,
            content_hash: "ABCDEFGH23456789ABCDEFGH23456789ABCDEFGH23456789ABCD".to_owned(),
            transition_seq: 1,
            absent_since_poll: None,
            filter_version: 1,
            first_seen_at: now,
            last_seen_at: now,
            bootstrapped: false,
            ttl: None,
        }
    }

    // -----------------------------------------------------------------------
    // Identifiers
    // -----------------------------------------------------------------------

    #[test]
    fn external_id_rejects_empty_whitespace_and_control_bytes() {
        // "   " covers the all-ASCII-whitespace rule; `\u{1f}` and `\u{1e}` are
        // §13.2.1's identity separators, and `\u{7f}` is DEL.
        for bad in ["", "   ", "40\u{1f}12345", "40\u{1e}12345", "40\u{7f}12345"] {
            let err = ExternalId::new(bad).expect_err("must be rejected");
            assert_eq!(err.stage, Stage::Normalize);
            assert_eq!(err.domain, FaultDomain::Adapter);
            assert_eq!(err.kind, FailureKind::NormalizeFailed);
        }

        let id = ExternalId::new("4012345").expect("a plain upstream id is valid");
        assert_eq!(id.as_str(), "4012345");
        assert_eq!(id.to_string(), "4012345");

        // The exact bytes survive: no trimming, no case folding (§21.1).
        assert_eq!(ExternalId::new(" Req-88 ").unwrap().as_str(), " Req-88 ");
    }

    /// A derived `Deserialize` would let an id carrying a §13.2.1 separator
    /// re-enter from stored data and shift the field boundaries of an event key,
    /// so the hand-written impl must route through the validating constructor.
    #[test]
    fn external_id_deserialization_validates() {
        let ok: ExternalId = serde_json::from_str("\"4012345\"").unwrap();
        assert_eq!(ok.as_str(), "4012345");
        assert_eq!(serde_json::to_string(&ok).unwrap(), "\"4012345\"");

        assert!(serde_json::from_str::<ExternalId>("\"40\\u001f12345\"").is_err());
        assert!(serde_json::from_str::<ExternalId>("\"\"").is_err());
    }

    // -----------------------------------------------------------------------
    // Enumerations
    // -----------------------------------------------------------------------

    #[test]
    fn criticality_ceilings_are_the_declared_blind_spots() {
        assert_eq!(Criticality::Critical.max_interval_secs(), 300);
        assert_eq!(Criticality::Standard.max_interval_secs(), 600);
        assert_eq!(Criticality::Background.max_interval_secs(), 1800);

        // §10.2: the SLA is derived from the ceiling, never stored separately —
        // two numbers that could disagree is the failure mode this prevents.
        for c in [
            Criticality::Critical,
            Criticality::Standard,
            Criticality::Background,
        ] {
            assert_eq!(c.failure_detection_sla_secs(), c.max_interval_secs());
        }
    }

    #[test]
    fn poll_outcome_wire_names_and_success_classification() {
        let all = [
            (PollOutcome::Success, "SUCCESS", true),
            (PollOutcome::NotModified, "NOT_MODIFIED", true),
            (PollOutcome::Transient, "TRANSIENT", false),
            (PollOutcome::Hard, "HARD", false),
            (PollOutcome::RateLimited, "RATE_LIMITED", false),
        ];

        for (outcome, wire, is_success) in all {
            // Exhaustiveness guard: a new variant fails to compile here until it
            // is also added to `all` above.
            match outcome {
                PollOutcome::Success
                | PollOutcome::NotModified
                | PollOutcome::Transient
                | PollOutcome::Hard
                | PollOutcome::RateLimited => {}
            }
            assert_eq!(outcome.as_str(), wire);
            assert_wire_name_agrees(outcome, wire);
            assert_eq!(
                outcome.is_success(),
                is_success,
                "{wire} classified on the wrong side of §8.1's success/failure split"
            );
        }
    }

    #[test]
    fn event_type_wire_names_agree_with_serde() {
        // §14 order.
        let all = [
            (EventType::NewJob, "NEW_JOB"),
            (EventType::BecameRelevant, "BECAME_RELEVANT"),
            (EventType::JobReposted, "JOB_REPOSTED"),
            (EventType::JobUpdated, "JOB_UPDATED"),
            (EventType::BecameIrrelevant, "BECAME_IRRELEVANT"),
            (EventType::JobRemoved, "JOB_REMOVED"),
            (EventType::SourceBootstrapped, "SOURCE_BOOTSTRAPPED"),
            (EventType::SourceDegraded, "SOURCE_DEGRADED"),
            (EventType::SourceFailed, "SOURCE_FAILED"),
            (EventType::SourceRecovered, "SOURCE_RECOVERED"),
            (EventType::SourceQuarantined, "SOURCE_QUARANTINED"),
            (EventType::ApiChanged, "API_CHANGED"),
            (EventType::SystemDegraded, "SYSTEM_DEGRADED"),
            (EventType::NotificationDegraded, "NOTIFICATION_DEGRADED"),
            (EventType::NotificationRecovered, "NOTIFICATION_RECOVERED"),
            (EventType::FilterChanged, "FILTER_CHANGED"),
        ];
        assert_eq!(all.len(), 16, "§14 defines sixteen event types");

        for (event_type, wire) in all {
            // Exhaustiveness guard: a new variant fails to compile here until it
            // is also added to `all` above.
            match event_type {
                EventType::NewJob
                | EventType::BecameRelevant
                | EventType::JobReposted
                | EventType::JobUpdated
                | EventType::BecameIrrelevant
                | EventType::JobRemoved
                | EventType::SourceBootstrapped
                | EventType::SourceDegraded
                | EventType::SourceFailed
                | EventType::SourceRecovered
                | EventType::SourceQuarantined
                | EventType::ApiChanged
                | EventType::SystemDegraded
                | EventType::NotificationDegraded
                | EventType::NotificationRecovered
                | EventType::FilterChanged => {}
            }
            assert_eq!(event_type.as_str(), wire);
            assert_wire_name_agrees(event_type, wire);
        }
    }

    /// §14's Notify column, verbatim. The three "if `relevant`" rows are the whole
    /// reason `notify_worthy` takes an argument.
    #[test]
    fn event_type_notify_worthy_matches_section_14() {
        // (event type, notify when relevant, notify when not relevant)
        let job_types = [
            (EventType::NewJob, true, false),
            (EventType::BecameRelevant, true, true),
            (EventType::JobReposted, true, false),
            (EventType::JobUpdated, false, false),
            (EventType::BecameIrrelevant, false, false),
            (EventType::JobRemoved, false, false),
        ];
        for (event_type, when_relevant, when_not) in job_types {
            assert_eq!(
                event_type.notify_worthy(true),
                when_relevant,
                "{event_type} with relevant=true"
            );
            assert_eq!(
                event_type.notify_worthy(false),
                when_not,
                "{event_type} with relevant=false"
            );
        }

        // The ten health/system types notify unconditionally; relevance is a
        // property of a job and means nothing to them.
        for event_type in [
            EventType::SourceBootstrapped,
            EventType::SourceDegraded,
            EventType::SourceFailed,
            EventType::SourceRecovered,
            EventType::SourceQuarantined,
            EventType::ApiChanged,
            EventType::SystemDegraded,
            EventType::NotificationDegraded,
            EventType::NotificationRecovered,
            EventType::FilterChanged,
        ] {
            assert!(event_type.notify_worthy(true), "{event_type}");
            assert!(event_type.notify_worthy(false), "{event_type}");
        }
    }

    /// §16.1 note 4: exactly four types have no `SRC#<id>` partition to live in.
    #[test]
    fn only_four_event_types_are_system_scoped() {
        for event_type in [
            EventType::SystemDegraded,
            EventType::NotificationDegraded,
            EventType::NotificationRecovered,
            EventType::FilterChanged,
        ] {
            assert!(event_type.is_system_scoped(), "{event_type}");
        }

        for event_type in [
            EventType::NewJob,
            EventType::BecameRelevant,
            EventType::JobReposted,
            EventType::JobUpdated,
            EventType::BecameIrrelevant,
            EventType::JobRemoved,
            EventType::SourceBootstrapped,
            EventType::SourceDegraded,
            EventType::SourceFailed,
            EventType::SourceRecovered,
            EventType::SourceQuarantined,
            EventType::ApiChanged,
        ] {
            assert!(!event_type.is_system_scoped(), "{event_type}");
        }
    }

    /// §21.1.1 hashes these into `content_hash`, so they are stored-data schema.
    #[test]
    fn employment_type_wire_names_agree_with_serde() {
        let all = [
            (EmploymentType::Internship, "internship"),
            (EmploymentType::CoOp, "co_op"),
            (EmploymentType::NewGrad, "new_grad"),
            (EmploymentType::FullTime, "full_time"),
            (EmploymentType::PartTime, "part_time"),
            (EmploymentType::Contract, "contract"),
            (EmploymentType::Unknown, "unknown"),
        ];
        for (employment_type, wire) in all {
            assert_eq!(employment_type.as_str(), wire);
            assert_wire_name_agrees(employment_type, wire);
        }
    }

    /// §16.2 stores `country` as `CA`/`NOT_CA`, and its absence is the third,
    /// unresolved class.
    #[test]
    fn country_class_wire_names_agree_with_serde() {
        assert_eq!(CountryClass::Ca.as_str(), "CA");
        assert_eq!(CountryClass::NotCa.as_str(), "NOT_CA");
        assert_wire_name_agrees(CountryClass::Ca, "CA");
        assert_wire_name_agrees(CountryClass::NotCa, "NOT_CA");
    }

    // -----------------------------------------------------------------------
    // Configuration defaults (§20)
    // -----------------------------------------------------------------------

    #[test]
    fn section_20_defaults() {
        assert_eq!(
            PlausibilityConfig::default(),
            PlausibilityConfig {
                min_ratio: 0.5,
                min_abs: 3,
                allow_zero: false,
            }
        );

        assert_eq!(BootstrapMode::default(), BootstrapMode::RelevantSummary);

        assert_eq!(
            FilterOverrides::default(),
            FilterOverrides {
                accept_remote_canada: false,
                // Fail open: §2 ranks a silent miss above noise (§21.2 rule 4).
                unresolved_location: UnresolvedLocationPolicy::Relevant,
            }
        );

        // A config that omits the optional blocks entirely must land on exactly
        // those defaults rather than failing to deserialize.
        assert_eq!(
            serde_json::from_str::<FilterOverrides>("{}").unwrap(),
            FilterOverrides::default()
        );
        assert_eq!(
            serde_json::from_str::<PlausibilityConfig>("{}").unwrap(),
            PlausibilityConfig::default()
        );
    }

    // -----------------------------------------------------------------------
    // JobIndex
    // -----------------------------------------------------------------------

    /// §13.4 sorts transitions by external id, byte-lexicographic over UTF-8,
    /// before chunking. [`JobIndex`] gets that order from its container so a
    /// caller cannot forget it — note that `"10"` precedes `"2"`, which is what
    /// makes this byte order rather than numeric order.
    #[test]
    fn job_index_iterates_in_external_id_byte_order() {
        let mut index = JobIndex::new();
        assert!(index.is_empty());
        assert_eq!(index.len(), 0);

        for raw in ["b", "2", "Z", "10", "a"] {
            assert!(
                index
                    .insert(ExternalId::new(raw).unwrap(), facts())
                    .is_none()
            );
        }

        let order: Vec<&str> = index.keys().map(ExternalId::as_str).collect();
        assert_eq!(order, ["10", "2", "Z", "a", "b"]);

        let via_iter: Vec<&str> = index.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(via_iter, order);

        assert_eq!(index.len(), 5);
        assert!(!index.is_empty());

        let two = ExternalId::new("2").unwrap();
        assert_eq!(index.get(&two), Some(&facts()));
        assert_eq!(index.get(&ExternalId::new("absent").unwrap()), None);

        // Re-inserting returns the displaced facts, so a caller can tell an update
        // from a first sighting.
        assert_eq!(index.insert(two, facts()), Some(facts()));
        assert_eq!(index.len(), 5);
    }

    // -----------------------------------------------------------------------
    // ScheduleState
    // -----------------------------------------------------------------------

    #[test]
    fn current_poll_seq_is_stored_plus_one() {
        let state = |poll_seq| ScheduleState {
            next_check_at: DateTime::parse_from_rfc3339("2026-08-16T10:05:07Z")
                .unwrap()
                .with_timezone(&Utc),
            poll_seq,
            lease_until: None,
            lease_owner: None,
        };

        // §7 registers a source with poll_seq = 0, so its first poll is poll 1.
        assert_eq!(state(0).current_poll_seq().unwrap(), 1);
        assert_eq!(state(41).current_poll_seq().unwrap(), 42);

        // Saturating here would reuse a sequence number and silently break every
        // absence comparison in §13.8, so overflow is a hard Persist failure.
        let err = state(u64::MAX)
            .current_poll_seq()
            .expect_err("u64::MAX must not saturate");
        assert_eq!(err.stage, Stage::Persist);
        assert_eq!(err.domain, FaultDomain::Infra);
        assert_eq!(err.kind, FailureKind::DbFailed);
    }
}
