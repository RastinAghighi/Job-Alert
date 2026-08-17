//! `RawJob` → `NormalizedJob` — §21.1's normalization contract.
//!
//! Adapters produce [`RawJob`]; this module produces [`NormalizedJob`]; the
//! filter (§21.2) is a single pure predicate over the result. That layering is
//! what makes one filter work across Greenhouse, Lever, Ashby and every future
//! adapter: nothing downstream ever sees an ATS-specific field.
//!
//! # Accepted values are preserved exactly
//!
//! Normalization classifies into *new* fields rather than rewriting the
//! originals. `external_id`, `title`, `location_raw` and `url` are stored as
//! supplied — no trimming, no case folding, no Unicode normalization. Two
//! separate rules force this: `external_id` is identity and §13.2.1 forbids
//! touching it, while `title`, `location_raw` and `url` feed `content_hash`
//! (§21.1.1), so trimming them would change a durable hash for cosmetic reasons
//! and fabricate a `JOB_UPDATED` transition for every affected job.
//! Classification therefore operates on *derived views* — the token vector and
//! the raw segment split below — and never on the stored strings.
//!
//! # Location is a three-way judgement, not a country database
//!
//! The only question the filter asks is "is this Canadian?", so §21.1 answers
//! Canada / NotCanada / **Unresolved** rather than maintaining a world table.
//! Unresolved is the *absence* of a country, not a third variant, and §21.2's
//! rule 4 fails open on it — which is why an incomplete table is safe and an
//! over-eager one is not. The Canadian-city table carries the concrete
//! consequence: globally ambiguous names such as `London` are deliberately left
//! out, because an omitted Canadian city is still delivered while an included
//! `London` would quietly claim London UK as Canadian.

use crate::model::{CountryClass, EmploymentType, ExternalId, NormalizedJob, RawJob};
use jobmon_errors::{FailureKind, FaultDomain, PipelineError, Stage};

// ---------------------------------------------------------------------------
// Tokenization — §21.1's ONE tokenizer
// ---------------------------------------------------------------------------

/// Lowercases, collapses every run of non-alphanumeric characters, and splits
/// into whole tokens (§21.1).
///
/// `Co-op`, `Co op` and `co_op` all produce `["co", "op"]`, which is the point:
/// upstream punctuation is not signal, so it must not be able to change a match.
///
/// This is the *single* tokenizer §21.1 mandates, shared with [`crate::filter`],
/// and it is public for that reason. Two tokenizers would drift, and the two
/// places that would drift are the two places that decide whether a posting ever
/// reaches a human.
///
/// "Alphanumeric" is Unicode-aware, so `Montréal` and `Québec` stay single
/// tokens instead of splitting at the accent.
#[must_use]
pub fn tokenize(s: &str) -> Vec<String> {
    let lowered = s.to_lowercase();
    lowered
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(String::from)
        .collect()
}

/// Whether `tokens` contains `phrase` as a contiguous whole-token run.
///
/// **All matching in this crate goes through here.** `str::contains` would match
/// substrings, and §21.1 calls whole-token matching a required property rather
/// than a stylistic preference: it is the only thing that stops
/// `Internal Tools Engineer` from matching `intern` and mailing the owner a
/// staff-engineering posting every week until he stops reading the channel.
///
/// An empty `phrase` matches nothing — a table entry that tokenized to nothing
/// is a table bug, and returning `true` would make it match every posting.
#[must_use]
pub fn contains_phrase(tokens: &[String], phrase: &[&str]) -> bool {
    if phrase.is_empty() || phrase.len() > tokens.len() {
        return false;
    }
    tokens
        .windows(phrase.len())
        .any(|window| window.iter().map(String::as_str).eq(phrase.iter().copied()))
}

/// [`contains_phrase`] for a canonical table entry, which is stored as a
/// space-separated phrase such as `"british columbia"`.
fn contains_table_phrase(tokens: &[String], phrase: &str) -> bool {
    let words: Vec<&str> = phrase.split(' ').collect();
    contains_phrase(tokens, &words)
}

// ---------------------------------------------------------------------------
// The raw-code rule — §21.1
// ---------------------------------------------------------------------------

/// Characters that bound a standalone location segment (§21.1).
///
/// Changing this set changes which strings resolve to a country, so it carries
/// the same `filter_version` obligation as the tables below (§21.3).
const SEGMENT_DELIMITERS: &[char] = &[',', '/', '|', ';', '(', ')', '-'];

/// Whether `code` appears in `location_raw` as a standalone segment, spelled
/// exactly as given (§21.1).
///
/// **Two-letter region codes are matched on the raw string, never on lowercased
/// tokens.** `ON`, `IN`, `OR`, `ME` and `HI` are ordinary English words after
/// case folding, so `work in office` would become Indiana and `based on team`
/// would become Ontario. Requiring the original spelling to be uppercase ASCII
/// *and* to occupy a whole segment bounded by start/end or one of
/// [`SEGMENT_DELIMITERS`] removes the entire class: `Toronto, ON`,
/// `Kingston (ON)` and `Austin, TX` match, prose does not.
///
/// An unrecognized upstream format such as `Austin TX` falls through to the
/// other markers, and failing that to Unresolved — fail-open by §21.2 rule 4,
/// which surfaces the gap as noise the owner can see rather than as an invisible
/// false negative.
fn has_standalone_code(location_raw: &str, code: &str) -> bool {
    location_raw
        .split(SEGMENT_DELIMITERS)
        .any(|segment| segment.trim() == code)
}

// ---------------------------------------------------------------------------
// Canonical tables (§21.1)
// ---------------------------------------------------------------------------
//
// Every table in this section is CODE, not configuration, and every one of them
// determines `relevant` just as directly as §21.2's keyword lists do. §21.3 is
// explicit that `filter_version` covers normalization and not only the filter:
// changing an entry here without bumping `filter_version` re-classifies
// pre-existing jobs and fabricates individual `BECAME_RELEVANT` alerts for all
// of them, which is exactly what INV-15 forbids.
//
// Extend them from observed unresolved strings, not from intuition (§29).

/// The `canada` phrase itself. Changing it requires a `filter_version` bump
/// (§21.3).
const CANADA: &str = "canada";

/// Canadian province and territory codes, matched by the raw-code rule.
///
/// Changing this table requires a `filter_version` bump (§21.3).
const PROVINCE_CODES: &[&str] = &[
    "AB", "BC", "MB", "NB", "NL", "NS", "NT", "NU", "ON", "PE", "QC", "SK", "YT",
];

/// Province and territory full names, each paired with the two-letter code it
/// resolves to, so that a full-name match still yields a code for `region`
/// (§21.1). Matched case-insensitively through the tokenizer, which is why
/// `Québec` and `Quebec` both appear and both map to `QC`.
///
/// The first matching row wins, so a string naming two provinces reports the one
/// listed earlier here. That is deterministic rather than correct — a posting in
/// two provinces has no single region — and it does not affect the country
/// decision, which is Canada either way.
///
/// Changing this table requires a `filter_version` bump (§21.3).
const PROVINCE_FULL_NAMES: &[(&str, &str)] = &[
    ("alberta", "AB"),
    ("british columbia", "BC"),
    ("manitoba", "MB"),
    ("new brunswick", "NB"),
    ("newfoundland and labrador", "NL"),
    ("nova scotia", "NS"),
    ("northwest territories", "NT"),
    ("nunavut", "NU"),
    ("ontario", "ON"),
    ("prince edward island", "PE"),
    ("quebec", "QC"),
    ("québec", "QC"),
    ("saskatchewan", "SK"),
    ("yukon", "YT"),
];

/// Canadian city names, used only when the string carries no conflicting
/// country or region marker (§21.1).
///
/// # What is deliberately omitted, and why
///
/// `London`, `Victoria`, `Kingston`, `Windsor` and other names more strongly
/// associated with a non-Canadian city are **not** here. The omission is safe
/// in one direction and unsafe in the other, and the two directions are not
/// symmetric:
///
/// - An omitted Canadian city falls to Unresolved. §21.2 rule 4 fails open, so
///   the posting is still relevant under the default override and still reaches
///   the owner — carrying §15.2's `⚠ location unparsed` marker, and counted by
///   §16.2's `jobs_location_unresolved`. The error is visible and measured.
/// - Including `London` would classify London UK as Canadian. That failure is
///   silent noise in the channel, and §2 ranks a channel the owner stops reading
///   as a priority-(1) violation of its own.
///
/// Changing this table requires a `filter_version` bump (§21.3).
const CANADIAN_CITIES: &[&str] = &[
    "toronto",
    "montreal",
    "montréal",
    "vancouver",
    "calgary",
    "edmonton",
    "ottawa",
    "winnipeg",
    "mississauga",
    "brampton",
    "markham",
    "vaughan",
    "oakville",
    "burlington",
    "burnaby",
    "richmond hill",
    "kitchener",
    "waterloo",
    "guelph",
    "halifax",
    "saskatoon",
    "regina",
    "kanata",
    "gatineau",
    "laval",
    "surrey",
];

/// Uppercase codes that mark a posting as non-Canadian under the raw-code rule:
/// forty-nine US state codes, `DC`, and the country code `US` itself (§21.1).
///
/// **`CA` is deliberately absent** — see [`AMBIGUOUS_CA`]. Every code that *is*
/// here is unambiguous, so it decides the country on its own.
///
/// Changing this table requires a `filter_version` bump (§21.3).
const US_RAW_CODES: &[&str] = &[
    "AL", "AK", "AZ", "AR", "CO", "CT", "DE", "FL", "GA", "HI", "ID", "IL", "IN", "IA", "KS", "KY",
    "LA", "ME", "MD", "MA", "MI", "MN", "MS", "MO", "MT", "NE", "NV", "NH", "NJ", "NM", "NY", "NC",
    "ND", "OH", "OK", "OR", "PA", "RI", "SC", "SD", "TN", "TX", "UT", "VT", "VA", "WA", "WV", "WI",
    "WY", "DC", "US",
];

/// The one two-letter code that decides nothing: `CA` is California on a US
/// board and Canada on a Canadian one.
///
/// # Why it is not in [`US_RAW_CODES`]
///
/// Treating it as California is a *silent* error in the one direction §2 ranks
/// worst. `Toronto, CA` would classify `NOT_CA`, fail at §21.2 rule 3, and never
/// be alerted — nothing fails, nothing is logged, and §36 records exactly this
/// class as Medium-likelihood and **invisible**. Treating it as a non-signal
/// instead sends `San Francisco, CA` to Unresolved, where §21.2 rule 4 fails
/// open and delivers it carrying §15.2's `⚠ location unparsed` marker. That is
/// noise the owner can see, and the asymmetry between a visible false positive
/// and an invisible false negative is the whole argument.
///
/// # What it therefore does
///
/// Nothing on its own, in either direction. It is not a Canadian marker, so
/// `San Francisco, CA` stays Unresolved rather than becoming Canada; and it is
/// not a non-Canadian marker, so it no longer suppresses the Canadian-city rule
/// — which is what makes `Toronto, CA` and `Vancouver, CA` resolve to Canada on
/// the strength of the city alone. A decisive marker of either kind still wins:
/// `San Francisco, CA, USA` is NotCanada, and the full name `California` is
/// NotCanada because [`US_STATE_NAMES`] carries it unambiguously.
///
/// The one place it *is* consulted is city extraction: a segment spelled `CA` is
/// a region token whichever country it names, so it is never stored as a city.
///
/// Changing this requires a `filter_version` bump (§21.3).
const AMBIGUOUS_CA: &str = "CA";

/// The fifty US state full names, matched case-insensitively through the
/// tokenizer (§21.1).
///
/// Changing this table requires a `filter_version` bump (§21.3).
const US_STATE_NAMES: &[&str] = &[
    "alabama",
    "alaska",
    "arizona",
    "arkansas",
    "california",
    "colorado",
    "connecticut",
    "delaware",
    "florida",
    "georgia",
    "hawaii",
    "idaho",
    "illinois",
    "indiana",
    "iowa",
    "kansas",
    "kentucky",
    "louisiana",
    "maine",
    "maryland",
    "massachusetts",
    "michigan",
    "minnesota",
    "mississippi",
    "missouri",
    "montana",
    "nebraska",
    "nevada",
    "new hampshire",
    "new jersey",
    "new mexico",
    "new york",
    "north carolina",
    "north dakota",
    "ohio",
    "oklahoma",
    "oregon",
    "pennsylvania",
    "rhode island",
    "south carolina",
    "south dakota",
    "tennessee",
    "texas",
    "utah",
    "vermont",
    "virginia",
    "washington",
    "west virginia",
    "wisconsin",
    "wyoming",
];

/// Non-Canadian country and region phrases, matched case-insensitively through
/// the tokenizer (§21.1).
///
/// `u s` is the tokenization of `U.S.`; the bare uppercase `US` lives in
/// [`US_RAW_CODES`] instead, because lowercasing it would collide with the
/// English word `us`.
///
/// Changing this table requires a `filter_version` bump (§21.3).
const NON_CANADIAN_MARKERS: &[&str] = &[
    "united states",
    "usa",
    "u s",
    "united kingdom",
    "uk",
    "england",
    "scotland",
    "wales",
    "ireland",
    "germany",
    "france",
    "spain",
    "italy",
    "netherlands",
    "poland",
    "sweden",
    "switzerland",
    "israel",
    "india",
    "singapore",
    "japan",
    "china",
    "hong kong",
    "australia",
    "new zealand",
    "brazil",
    "mexico",
    "argentina",
    "emea",
    "apac",
    "latam",
    "europe",
    "asia",
    "africa",
    "south america",
];

/// Phrases that make a posting remote (§21.1).
///
/// Changing this table requires a `filter_version` bump (§21.3).
const REMOTE_MARKERS: &[&str] = &["remote", "work from home", "wfh"];

/// Employment-type patterns, in precedence order (§21.1).
///
/// The first matching row wins, which is why `internship` is listed first: a
/// posting titled `Software Engineering Intern, Full Time` is an internship that
/// happens to be full time, not a full-time role that happens to say intern, and
/// §21.2's role gate turns on that distinction.
///
/// Changing this table requires a `filter_version` bump (§21.3) — §21.3 names
/// the employment-type patterns explicitly.
const EMPLOYMENT_PATTERNS: &[(&[&str], EmploymentType)] = &[
    (
        &["intern", "interns", "internship"],
        EmploymentType::Internship,
    ),
    (&["co op", "coop"], EmploymentType::CoOp),
    (
        &["new grad", "new graduate", "university grad"],
        EmploymentType::NewGrad,
    ),
    (&["full time"], EmploymentType::FullTime),
    (&["part time"], EmploymentType::PartTime),
    (
        &["contract", "contractor", "temporary"],
        EmploymentType::Contract,
    ),
];

// ---------------------------------------------------------------------------
// Location classification
// ---------------------------------------------------------------------------

/// Classifies a raw location string per §21.1.
///
/// # Returns
///
/// `(country, region, city, remote)`:
///
/// - `country` is `Option<CountryClass>`. `None` is *Unresolved* — neither
///   Canada nor a recognised non-Canadian marker — and is **not** a synonym for
///   `NOT_CA`. §21.2 treats the two differently, which is the whole reason the
///   field is optional.
/// - `region` is `Option<String>`: the matched two-letter province or territory
///   code, present only when Canada was reached through a province match. A full
///   name still yields the code, so `British Columbia` gives `BC`.
/// - `city` is `Option<String>`: the leading comma-separated segment, trimmed.
/// - `remote` is derived here on every poll and never persisted (§16.2) — it is
///   recomputable from `location_raw`, which is stored.
///
/// # Order of judgement
///
/// The three rules are evaluated in §21.1's order, and **ambiguity resolves to
/// Canada**: `Toronto, ON / New York, NY` and `Remote — Canada & US` both carry
/// a Canadian and a non-Canadian marker and both classify as Canada, because a
/// multi-location posting that includes Canada is relevant and §2's priority (1)
/// settles it.
///
/// The city list is the one rule gated on the *absence* of a conflicting marker.
/// A bare `Ottawa` is Canada; `Ottawa, IL` is not, because the city names are
/// weaker evidence than an explicit country or region and must not override it.
///
/// One code is deliberately not a marker at all: `CA` abbreviates both
/// California and Canada, so it decides nothing in either direction and cannot
/// suppress the city rule. `Toronto, CA` is Canada on the city alone,
/// `San Francisco, CA` is Unresolved, and `San Francisco, CA, USA` is
/// NotCanada. The asymmetry is deliberate: an unresolved location fails open
/// into visible alert noise (§21.2 rule 4), whereas calling a Canadian posting
/// `NOT_CA` is a silent miss, and §2 ranks those in that order.
#[must_use]
pub fn classify_location(
    location_raw: &str,
) -> (Option<CountryClass>, Option<String>, Option<String>, bool) {
    let tokens = tokenize(location_raw);

    let province = province_full_name(&tokens).or_else(|| province_raw_code(location_raw));
    let named_canada = contains_table_phrase(&tokens, CANADA);
    let non_canadian = has_non_canadian_marker(location_raw, &tokens);
    let canadian_city = CANADIAN_CITIES
        .iter()
        .any(|city| contains_table_phrase(&tokens, city));

    let country = if named_canada || province.is_some() || (canadian_city && !non_canadian) {
        Some(CountryClass::Ca)
    } else if non_canadian {
        Some(CountryClass::NotCa)
    } else {
        None
    };

    // No filter on `country` is needed: a province match forces the Canada arm
    // above, so `province.is_some()` already implies the class was reached
    // through a province.
    let region = province.map(String::from);

    (
        country,
        region,
        leading_city(location_raw),
        is_remote(&tokens),
    )
}

/// The province code for the first full name in [`PROVINCE_FULL_NAMES`] that
/// `tokens` contains.
fn province_full_name(tokens: &[String]) -> Option<&'static str> {
    PROVINCE_FULL_NAMES
        .iter()
        .find(|(name, _)| contains_table_phrase(tokens, name))
        .map(|(_, code)| *code)
}

/// The first province code appearing in `location_raw` under the raw-code rule.
fn province_raw_code(location_raw: &str) -> Option<&'static str> {
    PROVINCE_CODES
        .iter()
        .copied()
        .find(|code| has_standalone_code(location_raw, code))
}

/// Whether the string carries any recognised non-Canadian country or region
/// marker (§21.1's NotCanada row).
fn has_non_canadian_marker(location_raw: &str, tokens: &[String]) -> bool {
    US_STATE_NAMES
        .iter()
        .chain(NON_CANADIAN_MARKERS)
        .any(|marker| contains_table_phrase(tokens, marker))
        || US_RAW_CODES
            .iter()
            .any(|code| has_standalone_code(location_raw, code))
}

/// The leading comma-separated segment, trimmed, unless that segment is itself a
/// region or country marker (§21.1).
///
/// The segment is rejected when it *carries* a marker rather than only when it
/// equals one, so `Remote - US` yields no city: a segment naming a country is
/// not a city name, however much other text surrounds it. This is the one
/// derived value that is trimmed — it is a classification output, not a stored
/// upstream string, so §21.1's preserve-exactly rule does not reach it.
fn leading_city(location_raw: &str) -> Option<String> {
    let segment = location_raw
        .split(',')
        .next()
        .unwrap_or(location_raw)
        .trim();

    if segment.is_empty() || carries_region_or_country_marker(segment) {
        return None;
    }
    Some(segment.to_owned())
}

/// Whether a single segment names a region or a country, in either direction.
///
/// [`AMBIGUOUS_CA`] counts here even though it decides no country: `CA` is a
/// region token whichever country it abbreviates, so a segment spelled `CA` is
/// not a city name either way.
fn carries_region_or_country_marker(segment: &str) -> bool {
    let tokens = tokenize(segment);

    contains_table_phrase(&tokens, CANADA)
        || province_full_name(&tokens).is_some()
        || province_raw_code(segment).is_some()
        || has_standalone_code(segment, AMBIGUOUS_CA)
        || has_non_canadian_marker(segment, &tokens)
}

/// Whether any [`REMOTE_MARKERS`] phrase appears as whole tokens.
fn is_remote(tokens: &[String]) -> bool {
    REMOTE_MARKERS
        .iter()
        .any(|marker| contains_table_phrase(tokens, marker))
}

// ---------------------------------------------------------------------------
// Employment type
// ---------------------------------------------------------------------------

/// Classifies employment type from `employment_type_raw`, falling back to the
/// title (§21.1).
///
/// The raw field is consulted first because it is the upstream's own statement
/// of the fact; the title is a guess made from prose. The fallback fires when
/// the raw field is absent *or* classifies `Unknown`, so a board that emits
/// `"Full-time"` for every posting including its internships still classifies
/// those internships from the title — `Unknown` and "the raw field was useless"
/// are the same condition here.
#[must_use]
pub fn employment_type(raw: Option<&str>, title: &str) -> EmploymentType {
    match raw.map(classify_employment) {
        Some(EmploymentType::Unknown) | None => classify_employment(title),
        Some(classified) => classified,
    }
}

/// Applies [`EMPLOYMENT_PATTERNS`] to one string, first row winning.
fn classify_employment(text: &str) -> EmploymentType {
    let tokens = tokenize(text);

    for (patterns, employment_type) in EMPLOYMENT_PATTERNS {
        if patterns
            .iter()
            .any(|pattern| contains_table_phrase(&tokens, pattern))
        {
            return *employment_type;
        }
    }
    EmploymentType::Unknown
}

// ---------------------------------------------------------------------------
// The normalization entry point
// ---------------------------------------------------------------------------

/// Applies §21.1's normalization contract to one raw posting.
///
/// # Errors
///
/// `Stage::Normalize` / `FaultDomain::Adapter` / `FailureKind::NormalizeFailed`
/// for an empty or all-ASCII-whitespace `external_id`, `title` or `url`, and for
/// an `external_id` containing any ASCII control character.
///
/// The control-character rule defends §13.2.1: `0x1F` and `0x1E` are the
/// identity separators, so an upstream id containing one could shift the field
/// boundaries of a durable event key and let a hostile or merely careless id
/// collide with another job's identity. Rejecting at this boundary is what keeps
/// INV-2 unconditional instead of dependent on upstream data hygiene.
///
/// The failure is attributed to `FaultDomain::Adapter` rather than `Upstream`
/// because an unusable id is either the upstream payload or the adapter's
/// reading of it, and only the adapter can be fixed.
///
/// # Emptiness is checked on the raw value, and the raw value is what is stored
///
/// A title of `"   "` is rejected, but a title of `"  Backend Intern  "` keeps
/// both runs of spaces. Trimming would change `content_hash` (§21.1.1) for a
/// cosmetic reason and fabricate a `JOB_UPDATED` transition for every job that
/// had leading whitespace the day the trim shipped.
pub fn normalize(raw: &RawJob) -> Result<NormalizedJob, PipelineError> {
    // Validation lives in `ExternalId::new`, and it raises exactly this triple —
    // both the emptiness rule and the control-character rule.
    let external_id = ExternalId::new(&raw.external_id)?;

    reject_if_blank(&raw.title, "title")?;
    reject_if_blank(&raw.url, "url")?;

    let (country, region, city, remote) = classify_location(&raw.location_raw);

    Ok(NormalizedJob {
        external_id,
        title: raw.title.clone(),
        location_raw: raw.location_raw.clone(),
        country,
        region,
        city,
        remote,
        employment_type: employment_type(raw.employment_type_raw.as_deref(), &raw.title),
        url: raw.url.clone(),
        posted_at: raw.posted_at,
    })
}

/// Rejects an empty or all-ASCII-whitespace field with §21.1's error triple.
fn reject_if_blank(value: &str, field: &str) -> Result<(), PipelineError> {
    if value.is_empty() || value.bytes().all(|b| b.is_ascii_whitespace()) {
        return Err(PipelineError::new(
            Stage::Normalize,
            FaultDomain::Adapter,
            FailureKind::NormalizeFailed,
            // `{value:?}` escapes control bytes as `\u{...}` rather than
            // emitting them raw into a log line.
            format!("invalid {field} (empty or all ASCII whitespace): {value:?}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A posting that normalizes cleanly, for tests that vary one field.
    fn valid_raw() -> RawJob {
        RawJob {
            external_id: "4012345".to_owned(),
            title: "Backend Engineering Intern".to_owned(),
            location_raw: "Toronto, ON".to_owned(),
            employment_type_raw: None,
            url: "https://example.invalid/jobs/4012345".to_owned(),
            posted_at: None,
        }
    }

    /// Just the country, for the location tests.
    fn country_of(location_raw: &str) -> Option<CountryClass> {
        classify_location(location_raw).0
    }

    // -----------------------------------------------------------------------
    // Rejections (§21.1, §13.2.1)
    // -----------------------------------------------------------------------

    /// Asserts that `valid_raw()` with one field broken fails with §21.1's
    /// triple.
    fn assert_rejected(label: &str, break_one_field: impl FnOnce(&mut RawJob)) {
        let mut raw = valid_raw();
        break_one_field(&mut raw);

        let error = normalize(&raw).expect_err(label);
        assert_eq!(error.stage, Stage::Normalize, "{label}");
        assert_eq!(error.domain, FaultDomain::Adapter, "{label}");
        assert_eq!(error.kind, FailureKind::NormalizeFailed, "{label}");
    }

    /// Every §21.1 rejection carries the same triple, and every one of them is
    /// raised before any key is derived — INV-2 depends on `0x1F` and `0x1E`
    /// never reaching [`crate::event_key`].
    #[test]
    fn invalid_fields_are_normalize_failures() {
        assert_rejected("empty external_id", |r| r.external_id = String::new());
        assert_rejected("blank external_id", |r| r.external_id = "   ".to_owned());
        assert_rejected("0x1F in external_id", |r| {
            r.external_id = "40\u{1f}1".to_owned()
        });
        assert_rejected("0x1E in external_id", |r| {
            r.external_id = "40\u{1e}1".to_owned()
        });
        assert_rejected("empty title", |r| r.title = String::new());
        assert_rejected("blank title", |r| r.title = " \t ".to_owned());
        assert_rejected("empty url", |r| r.url = String::new());
        assert_rejected("blank url", |r| r.url = " ".to_owned());
    }

    // -----------------------------------------------------------------------
    // Location classification (§21.1)
    // -----------------------------------------------------------------------

    #[test]
    fn province_code_yields_canada_region_and_city() {
        let (country, region, city, remote) = classify_location("Toronto, ON");

        assert_eq!(country, Some(CountryClass::Ca));
        assert_eq!(region.as_deref(), Some("ON"));
        assert_eq!(city.as_deref(), Some("Toronto"));
        assert!(!remote);
    }

    /// A full name still yields the two-letter code, so `region` has one
    /// spelling however the upstream wrote it.
    #[test]
    fn province_full_name_yields_the_code() {
        let (country, region, _, _) = classify_location("Vancouver, British Columbia, Canada");

        assert_eq!(country, Some(CountryClass::Ca));
        assert_eq!(region.as_deref(), Some("BC"));
    }

    /// Every unambiguous state code still decides on its own.
    #[test]
    fn us_state_code_yields_not_canada() {
        assert_eq!(country_of("Austin, TX"), Some(CountryClass::NotCa));
        assert_eq!(country_of("Seattle, WA"), Some(CountryClass::NotCa));
        assert_eq!(country_of("Chicago (IL)"), Some(CountryClass::NotCa));
    }

    /// `CA` is California to a US board and Canada to a Canadian one, so it
    /// decides nothing in either direction.
    ///
    /// Reading it as California would send `Toronto, CA` to `NOT_CA`, where
    /// §21.2 rule 3 drops it silently — the invisible false negative §2 ranks
    /// worst. Reading it as a non-signal sends `San Francisco, CA` to
    /// Unresolved instead, where rule 4 fails open and the owner sees it with
    /// §15.2's `⚠ location unparsed` marker. Visible noise over a silent miss.
    #[test]
    fn ambiguous_ca_code_decides_nothing_on_its_own() {
        // Not a non-Canadian marker, so it no longer suppresses the city rule
        // and the Canadian city alone carries the decision.
        assert_eq!(country_of("Toronto, CA"), Some(CountryClass::Ca));
        assert_eq!(country_of("Vancouver, CA"), Some(CountryClass::Ca));

        // Not a Canadian marker either: with no other signal, unresolved.
        assert_eq!(country_of("San Francisco, CA"), None);

        // Any decisive marker still wins, from either direction.
        assert_eq!(
            country_of("San Francisco, CA, USA"),
            Some(CountryClass::NotCa)
        );
        assert_eq!(country_of("Toronto, CA, Canada"), Some(CountryClass::Ca));

        // The full name is unambiguous and remains an explicit US marker.
        assert_eq!(country_of("California"), Some(CountryClass::NotCa));
        assert_eq!(
            country_of("San Francisco, California"),
            Some(CountryClass::NotCa)
        );

        // `CA` is still a region token for city extraction — never a city name —
        // and the raw-code rule still applies, so lowercase prose is untouched.
        assert_eq!(classify_location("CA").2, None);
        assert_eq!(
            classify_location("Toronto, CA").2.as_deref(),
            Some("Toronto")
        );
        assert_eq!(country_of("we ship ca builds weekly"), None);
    }

    #[test]
    fn remote_us_is_not_canada_and_remote() {
        let (country, _, _, remote) = classify_location("Remote - US");

        assert_eq!(country, Some(CountryClass::NotCa));
        assert!(remote);
    }

    /// The raw-code rule in the only form that matters: lowercase prose must not
    /// become a region. `in` is Indiana and `on` is Ontario after case folding,
    /// and both of these strings are ordinary English.
    #[test]
    fn lowercase_prose_is_never_a_region_code() {
        assert_eq!(country_of("work in office"), None);
        assert_eq!(country_of("based on team"), None);
    }

    /// §21.1: a string carrying both markers is Canada. A multi-location posting
    /// that includes Canada is relevant, and priority (1) settles it.
    #[test]
    fn ambiguity_resolves_to_canada() {
        assert_eq!(
            country_of("Toronto, ON / New York, NY"),
            Some(CountryClass::Ca)
        );

        let (country, _, _, remote) = classify_location("Remote — Canada & US");
        assert_eq!(country, Some(CountryClass::Ca));
        assert!(remote);
    }

    #[test]
    fn unrecognised_location_is_unresolved() {
        assert_eq!(country_of("Multiple Locations"), None);
    }

    /// The city list fires on its own, and yields no region — `region` is
    /// present only when Canada was reached through a province match.
    #[test]
    fn bare_canadian_city_is_canada_without_a_region() {
        let (country, region, _, _) = classify_location("Ottawa");

        assert_eq!(country, Some(CountryClass::Ca));
        assert_eq!(region, None);
    }

    /// The omitted-city choice, in both directions: `London, UK` resolves away
    /// from Canada, and a bare `London` stays unresolved rather than being
    /// invisibly claimed as Canadian.
    #[test]
    fn ambiguous_city_names_are_omitted_from_the_canadian_list() {
        assert_eq!(country_of("London, UK"), Some(CountryClass::NotCa));
        assert_eq!(country_of("London"), None);
    }

    // -----------------------------------------------------------------------
    // Tokenization (§21.1)
    // -----------------------------------------------------------------------

    #[test]
    fn punctuation_does_not_change_tokens() {
        let expected = ["co", "op"];

        assert_eq!(tokenize("Co-op"), expected);
        assert_eq!(tokenize("Co op"), expected);
        assert_eq!(tokenize("co_op"), expected);
    }

    /// §21.1's required test. Substring matching would make every
    /// `Internal Tools Engineer` an internship.
    #[test]
    fn matching_is_whole_token_not_substring() {
        let tokens = tokenize("Internal Tools Engineer");

        assert!(!contains_phrase(&tokens, &["intern"]));
        assert_eq!(
            employment_type(None, "Internal Tools Engineer"),
            EmploymentType::Unknown
        );
    }

    // -----------------------------------------------------------------------
    // Employment type (§21.1)
    // -----------------------------------------------------------------------

    #[test]
    fn employment_type_prefers_the_raw_field_then_the_title() {
        // The raw field decides when it classifies.
        assert_eq!(
            employment_type(Some("Co-op"), "Software Engineer Intern"),
            EmploymentType::CoOp
        );
        // It is consulted first but not blindly: an unclassifiable raw field
        // falls through to the title, as does an absent one.
        assert_eq!(
            employment_type(Some("Regular"), "Software Engineer Intern"),
            EmploymentType::Internship
        );
        assert_eq!(
            employment_type(None, "Software Engineer Intern"),
            EmploymentType::Internship
        );
    }

    // -----------------------------------------------------------------------
    // The whole contract
    // -----------------------------------------------------------------------

    /// Accepted values are preserved exactly — §21.1 forbids trimming, and
    /// `title`, `location_raw` and `url` feed `content_hash` (§21.1.1), so a
    /// cosmetic trim would fabricate a `JOB_UPDATED` for every affected job.
    #[test]
    fn accepted_values_are_carried_through_untrimmed() {
        let raw = RawJob {
            external_id: " 4012345 ".to_owned(),
            title: "  Backend Engineering Intern  ".to_owned(),
            location_raw: "  Toronto, ON  ".to_owned(),
            employment_type_raw: Some("  Internship  ".to_owned()),
            url: "  https://example.invalid/jobs/4012345  ".to_owned(),
            posted_at: None,
        };

        let job = normalize(&raw).expect("the fixture posting is valid");

        assert_eq!(job.external_id.as_str(), " 4012345 ");
        assert_eq!(job.title, "  Backend Engineering Intern  ");
        assert_eq!(job.location_raw, "  Toronto, ON  ");
        assert_eq!(job.url, "  https://example.invalid/jobs/4012345  ");

        // Classification still reads through the surrounding whitespace, and
        // `city` — a derived value, not a stored one — is trimmed.
        assert_eq!(job.country, Some(CountryClass::Ca));
        assert_eq!(job.region.as_deref(), Some("ON"));
        assert_eq!(job.city.as_deref(), Some("Toronto"));
        assert_eq!(job.employment_type, EmploymentType::Internship);
        assert!(!job.remote);
    }
}
