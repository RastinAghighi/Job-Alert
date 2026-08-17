//! The relevance predicate (§21.2) and the version its tables are published
//! under (§21.3, INV-15).
//!
//! # One predicate, three gates, one order
//!
//! [`is_relevant`] is a pure function of a [`NormalizedJob`] and the source's
//! [`FilterOverrides`]. It never sees an ATS-specific field, because §21.1's
//! normalization has already erased the difference between Greenhouse, Lever,
//! Ashby and everything after them. That layering is the whole reason a single
//! filter can serve every adapter: the filter is written once, against the
//! normalized model, and a new ATS costs an adapter rather than a new filter.
//!
//! The gates are evaluated in §21.2's order — location, then role, then the
//! exclusion list, which is applied last and wins outright.
//!
//! # Every table here is code, and code is versioned
//!
//! The keyword and exclusion lists below are `const` tables, not configuration.
//! §21.3 makes that a durable obligation rather than an implementation detail:
//! each job stores the [`FILTER_VERSION`] its `relevant` flag was computed
//! under, so editing a table without bumping the version would re-classify
//! pre-existing jobs and fire an individual `BECAME_RELEVANT` for every one of
//! them — precisely the channel-burying storm INV-15 exists to prevent.
//!
//! # No scoring, no model
//!
//! §35 defers ML relevance ranking until the false-negative rate is measurable
//! and material. A keyword table is auditable: when a posting is missed, the
//! reason is a row that can be read, argued with, and changed under a version
//! bump. A score cannot be read that way, and §36 already rates filter false
//! negatives as *invisible* — an unauditable filter would make the one failure
//! mode the owner cannot detect also the one he cannot investigate.

use crate::model::{
    CountryClass, EmploymentType, FilterConfig, FilterOverrides, NormalizedJob,
    UnresolvedLocationPolicy,
};
use crate::normalize::{contains_phrase, tokenize};

/// The version the relevance tables are published under (§21.3).
///
/// # What it covers
///
/// Everything that can change a `relevant` verdict, not only the keyword lists
/// in this module. §21.3 is explicit: the province table, the Canadian-city
/// list, the non-Canadian country list and the employment-type patterns in
/// [`crate::normalize`] determine relevance just as directly as this module's
/// `ROLE_KEYWORDS` and `EXCLUDED_TITLE_TERMS` do. Changing **any** of them
/// requires a bump here.
///
/// # What a bump costs, and why the alternative is worse
///
/// A bump is not free — it re-evaluates every stored job at its next poll. That
/// is the point. §21.3 routes the resulting relevance changes into a single
/// `FILTER_CHANGED` summary and suppresses the individual `BECAME_RELEVANT` and
/// `BECAME_IRRELEVANT` events, so one table edit produces one message instead of
/// hundreds. Editing a table *without* bumping skips that machinery entirely and
/// fabricates the storm directly, which is INV-15's failure mode.
///
/// # Keeping the operator in sync
///
/// This constant is the code's version; `SYS#CONFIG/FILTER` carries the
/// operator's. §34's runbook makes bumping that item the documented procedure
/// whenever the filter changes, and names the symptom of getting it wrong: if
/// individual `BECAME_RELEVANT` alerts appear where one `FILTER_CHANGED` should
/// have, INV-15 is broken and deployment stops until it is fixed.
pub const FILTER_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Keyword tables (§21.2)
// ---------------------------------------------------------------------------
//
// Both tables are stored PRE-TOKENIZED — each entry is the token sequence
// `normalize::tokenize` would produce for it, so a multi-word phrase such as
// `head of` is `&["head", "of"]`. Storing them this way makes the whole-token
// contract structural rather than a rule to remember: there is no string here
// for anyone to reach for `str::contains` with, and a phrase can only ever be
// compared as a contiguous run of whole tokens.
//
// Changing either table requires a `FILTER_VERSION` bump (§21.3).

/// Title terms that mark a posting as a student, intern or new-graduate role
/// (§21.2's role gate).
///
/// `co op` and `coop` both appear because the tokenizer collapses punctuation
/// but not spelling: `Co-op` and `Co op` both tokenize to `["co", "op"]`, while
/// the closed-up `coop` is a single token and would otherwise be missed.
const ROLE_KEYWORDS: &[&[&str]] = &[
    &["intern"],
    &["interns"],
    &["internship"],
    &["co", "op"],
    &["coop"],
    &["student"],
    &["new", "grad"],
    &["new", "graduate"],
    &["university", "grad"],
    &["campus"],
];

/// Title terms that disqualify a posting outright (§21.2's exclusion list).
///
/// These are seniority markers, and they are checked as whole tokens like
/// everything else — which matters in this direction too. `lead` must not match
/// `Leadership Development Intern`, or the filter would silently drop a genuine
/// internship, and §36 rates a silent drop as the failure the owner cannot see.
const EXCLUDED_TITLE_TERMS: &[&[&str]] = &[
    &["senior"],
    &["sr"],
    &["staff"],
    &["principal"],
    &["lead"],
    &["manager"],
    &["director"],
    &["head", "of"],
    &["vp"],
    &["distinguished"],
];

// ---------------------------------------------------------------------------
// The predicate
// ---------------------------------------------------------------------------

/// Whether one normalized posting should reach the owner's phone (§21.2).
///
/// Three gates, in this order: the location gate, the role gate, and the
/// exclusion list. All three must be satisfied.
///
/// # Exclusions are last, and they win outright
///
/// An excluded title is not relevant however well it scored above.
/// `Senior Software Engineering Intern` in Toronto passes the location gate on
/// `ON` and the role gate on `Intern`, and is still rejected — the seniority
/// marker is not a tiebreaker against the other two gates, it is a veto over
/// them.
///
/// # Why `FilterConfig` is a parameter this function never reads
///
/// The signature is §21.2's, and it is deliberate that the version travels
/// alongside the predicate without entering it. Relevance is a function of the
/// posting and the source's overrides only; the version is what the *caller*
/// stores next to the answer so a `BECAME_RELEVANT` from March stays
/// interpretable against the filter that was live in March (§21.3). INV-15's
/// suppression logic reads the stored version in [`crate::diff`], not here. A
/// predicate that branched on the version would mean the same posting decided
/// differently on either side of a bump for reasons no stored record explained,
/// which is exactly the interpretability the version was introduced to protect.
#[must_use]
pub fn is_relevant(job: &NormalizedJob, _cfg: &FilterConfig, overrides: &FilterOverrides) -> bool {
    // Tokenized once and shared: the role gate and the exclusion list read the
    // same view of the same title, so no punctuation difference can make them
    // disagree about what the words are.
    let title = tokenize(&job.title);

    passes_location_gate(job, overrides) && passes_role_gate(job, &title) && !is_excluded(&title)
}

/// §21.2's location gate: four rules, evaluated in order, first match decides.
///
/// The chain is written out rather than folded into a single expression because
/// the *order* is the specification here, not an implementation choice, and the
/// two orderings that matter are both invisible in a collapsed form.
///
/// # Rule 2 sits above rule 3, and that is the entire point of the override
///
/// `accept_remote_canada` exists to rescue a posting the tables resolved
/// `NOT_CA` — a US-headquartered board writing `Remote - US` for a role that is
/// in fact open to Canadian applicants, at an employer where the owner has
/// confirmed that is true. Below rule 3 the override would be unreachable code:
/// rule 3 would already have failed every posting it was written to save. Its
/// position in the chain *is* its behaviour.
///
/// # Rule 4 fails open, which is the expensive choice, deliberately made
///
/// Rule 4 is reached only when the location resolved in *neither* direction —
/// not Canada, and not any recognised non-Canadian marker. Calling those
/// relevant manufactures noise. Calling them irrelevant would manufacture
/// silence, and the two errors are not symmetric:
///
/// - A false negative is **invisible**. Nothing fails, nothing is logged, no
///   counter moves. A Canadian internship whose location string the tables do
///   not cover simply never arrives, and the owner has no way to learn it ever
///   existed. §36 rates this Medium-likelihood, invisible, and a direct
///   violation of §2's priority (1) — *never silently miss a relevant posting*.
/// - A false positive is **visible**. It lands in the channel wearing §15.2's
///   `⚠ location unparsed` prefix, so it is never mistaken for a confirmed
///   Canadian posting, and §16.2's `jobs_location_unresolved` and
///   `events_unresolved_location` count it per source per hour.
///
/// Failing closed hides exactly the errors the owner cannot detect; failing open
/// converts them into errors he can. §2 settles which of those is worse.
///
/// # And it is bounded, because unbounded it would defeat itself
///
/// §2 also makes noise a priority-(1) problem the moment it teaches the owner to
/// ignore alerts, so four things keep this from becoming a firehose:
///
/// - **The class is narrow.** `Austin, TX` and `Remote - US` are `NOT_CA` and
///   die at rule 3. What reaches rule 4 is genuinely ambiguous text —
///   `Multiple Locations`, `Global`, an empty field, a city on neither list.
/// - **The marker labels it**, so false positives erode trust in the marker
///   rather than in the channel.
/// - **The counters measure it**, which makes the default falsifiable on
///   evidence instead of on intuition — the same discipline D17 applies to
///   adaptive polling. §29 carries the trigger that revisits this decision when
///   `events_unresolved_location` proves the tables are too thin.
/// - **It is switchable per source.** `unresolved_location = not_relevant` (§20)
///   turns rule 4 off for a board that is overwhelmingly non-Canadian and emits
///   unresolvable location strings.
fn passes_location_gate(job: &NormalizedJob, overrides: &FilterOverrides) -> bool {
    // Rule 1 — resolved Canadian.
    if job.country == Some(CountryClass::Ca) {
        return true;
    }
    // Rule 2 — remote, at an employer whose remote roles are Canada-eligible.
    // Above rule 3 on purpose; see the doc comment.
    if job.remote && overrides.accept_remote_canada {
        return true;
    }
    // Rule 3 — resolved non-Canadian.
    if job.country == Some(CountryClass::NotCa) {
        return false;
    }
    // Rule 4 — unresolved. `None` is not a synonym for `NOT_CA`, and this line
    // is the only place in the system where that distinction is cashed in.
    overrides.unresolved_location == UnresolvedLocationPolicy::Relevant
}

/// §21.2's role gate: an internship, co-op or new-grad employment type, **or** a
/// title carrying one of [`ROLE_KEYWORDS`].
///
/// The two arms are independent on purpose, and each covers the other's blind
/// spot. A board that leaves `employment_type_raw` empty is caught by the title;
/// a board that titles its co-op postings `Software Developer` and says `Co-op`
/// only in the structured field is caught by the type. Requiring both would drop
/// every posting either kind of board produces.
fn passes_role_gate(job: &NormalizedJob, title: &[String]) -> bool {
    matches!(
        job.employment_type,
        EmploymentType::Internship | EmploymentType::CoOp | EmploymentType::NewGrad
    ) || ROLE_KEYWORDS
        .iter()
        .any(|phrase| contains_phrase(title, phrase))
}

/// Whether the title carries any [`EXCLUDED_TITLE_TERMS`] entry as whole tokens.
fn is_excluded(title: &[String]) -> bool {
    EXCLUDED_TITLE_TERMS
        .iter()
        .any(|phrase| contains_phrase(title, phrase))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RawJob;
    use crate::normalize::normalize;

    /// The version under which these tables are published. The predicate does
    /// not read it — [`the_verdict_does_not_depend_on_the_filter_version`] pins
    /// that — so one value serves every test that is not about the version.
    const CFG: FilterConfig = FilterConfig {
        filter_version: FILTER_VERSION,
    };

    /// A posting built through the *real* normalizer.
    ///
    /// These tests therefore assert against the classifications §21.1 actually
    /// produces for real location strings, rather than against hand-labelled
    /// fixtures that could drift away from them and leave the filter tested
    /// only against a world that no longer exists.
    fn posting(
        title: &str,
        location_raw: &str,
        employment_type_raw: Option<&str>,
    ) -> NormalizedJob {
        normalize(&RawJob {
            external_id: "4012345".to_owned(),
            title: title.to_owned(),
            location_raw: location_raw.to_owned(),
            employment_type_raw: employment_type_raw.map(str::to_owned),
            url: "https://example.invalid/jobs/4012345".to_owned(),
            posted_at: None,
        })
        .expect("the fixture posting is valid")
    }

    /// [`is_relevant`] under §20's default overrides.
    fn relevant_by_default(job: &NormalizedJob) -> bool {
        is_relevant(job, &CFG, &FilterOverrides::default())
    }

    // -----------------------------------------------------------------------
    // The location gate (§21.2 rules 1–4)
    // -----------------------------------------------------------------------

    /// Rule 1, and the whole predicate's happy path.
    #[test]
    fn canadian_internship_is_relevant() {
        let job = posting("Software Engineering Intern", "Toronto, ON", None);

        assert_eq!(job.country, Some(CountryClass::Ca), "precondition");
        assert!(relevant_by_default(&job));
    }

    /// Rule 3: a resolved non-Canadian country fails, and a perfectly good role
    /// gate does not rescue it.
    #[test]
    fn us_internship_fails_at_rule_3() {
        let job = posting("Software Engineering Intern", "Austin, TX", None);

        assert_eq!(job.country, Some(CountryClass::NotCa), "precondition");
        assert_eq!(
            job.employment_type,
            EmploymentType::Internship,
            "the role gate passes; the location gate is what rejects this"
        );
        assert!(!relevant_by_default(&job));
    }

    /// Rule 2 above rule 3 — the ordering `accept_remote_canada` exists for. The
    /// same posting, the same tables, and only the override differs.
    #[test]
    fn remote_not_canada_passes_only_with_accept_remote_canada() {
        let job = posting("Software Engineering Intern", "Remote - US", None);

        // Without these the test could pass for the wrong reason: the posting
        // must really be both remote and resolved NOT_CA for rule 2 to be the
        // thing under test.
        assert_eq!(job.country, Some(CountryClass::NotCa), "precondition");
        assert!(job.remote, "precondition");

        assert!(!relevant_by_default(&job));
        assert!(is_relevant(
            &job,
            &CFG,
            &FilterOverrides {
                accept_remote_canada: true,
                ..FilterOverrides::default()
            }
        ));
    }

    /// Rule 4's default: fail open. An unresolved location is not `NOT_CA`, and
    /// this is where that distinction is spent.
    #[test]
    fn unresolved_location_is_relevant_under_the_default_override() {
        let job = posting("Software Engineering Intern", "Multiple Locations", None);

        assert!(
            job.country.is_none(),
            "precondition: unresolved, not resolved NOT_CA"
        );
        assert!(relevant_by_default(&job));
    }

    /// §20's per-source escape hatch, on the identical posting: the same job
    /// that rule 4 admits by default is rejected when the operator turns rule 4
    /// off for a board that is overwhelmingly non-Canadian.
    #[test]
    fn unresolved_location_policy_not_relevant_turns_rule_4_off() {
        let job = posting("Software Engineering Intern", "Multiple Locations", None);

        assert!(!is_relevant(
            &job,
            &CFG,
            &FilterOverrides {
                unresolved_location: UnresolvedLocationPolicy::NotRelevant,
                ..FilterOverrides::default()
            }
        ));
    }

    // -----------------------------------------------------------------------
    // The role gate and the exclusion list (§21.2)
    // -----------------------------------------------------------------------

    /// The role gate rejecting on its own. The second posting carries no
    /// excluded term at all, which is what shows the role gate — and not the
    /// exclusion list — doing the work.
    #[test]
    fn canadian_full_time_role_fails_the_role_gate() {
        let senior = posting("Senior Software Engineer", "Toronto, ON", Some("Full-time"));
        let plain = posting("Software Engineer", "Toronto, ON", Some("Full-time"));

        assert_eq!(
            plain.employment_type,
            EmploymentType::FullTime,
            "precondition"
        );
        assert!(!relevant_by_default(&senior));
        assert!(!relevant_by_default(&plain));
    }

    /// Exclusions are applied last and win outright. Both gates above pass —
    /// `Toronto, ON` is Canada and the posting really does classify as an
    /// internship — and `Senior` vetoes them. The control differs by exactly
    /// that one word.
    #[test]
    fn exclusions_win_over_a_passing_role_gate() {
        let excluded = posting("Senior Software Engineering Intern", "Toronto, ON", None);
        let control = posting("Software Engineering Intern", "Toronto, ON", None);

        assert_eq!(
            excluded.employment_type,
            EmploymentType::Internship,
            "the role gate passes, so the exclusion is what decides"
        );
        assert!(!relevant_by_default(&excluded));
        assert!(relevant_by_default(&control));
    }

    /// §21.1's whole-token rule, which §21.1 calls a required property rather
    /// than a stylistic preference — tested in both directions, because it
    /// protects against a different failure in each.
    ///
    /// `Internal` matching `intern` would mail the owner staff-engineering
    /// postings until he stopped reading the channel. `Leadership` matching the
    /// exclusion `lead` would silently drop a real internship, which is the
    /// error §36 rates invisible.
    #[test]
    fn matching_is_whole_token_not_substring() {
        let internal = posting("Internal Tools Engineer", "Toronto, ON", None);
        let leadership = posting("Leadership Development Intern", "Toronto, ON", None);

        assert!(!relevant_by_default(&internal));
        assert!(relevant_by_default(&leadership));
    }

    /// The employment-type arm of the role gate, isolated: the title carries no
    /// role keyword whatsoever, and the structured field alone qualifies the
    /// posting. The control is the same title with the field removed.
    #[test]
    fn co_op_employment_type_alone_makes_a_title_relevant() {
        let typed = posting("Software Developer", "Toronto, ON", Some("Co-op"));
        let untyped = posting("Software Developer", "Toronto, ON", None);

        assert_eq!(typed.employment_type, EmploymentType::CoOp, "precondition");
        assert!(relevant_by_default(&typed));
        assert!(
            !relevant_by_default(&untyped),
            "the title alone must not qualify, or the test proves nothing"
        );
    }

    /// One tokenizer, so upstream punctuation cannot change a verdict. All three
    /// spellings collapse to matching tokens and must decide identically —
    /// `Co-op` and `Co op` both tokenize to `["co", "op"]`, and the closed-up
    /// `coop` is carried by its own table entry.
    #[test]
    fn co_op_spellings_are_indistinguishable() {
        let verdicts: Vec<bool> = [
            "Co-op Software Developer",
            "Co op Software Developer",
            "coop software developer",
        ]
        .iter()
        .map(|title| relevant_by_default(&posting(title, "Toronto, ON", None)))
        .collect();

        assert_eq!(verdicts, [true, true, true]);
    }

    // -----------------------------------------------------------------------
    // Versioning (§21.3)
    // -----------------------------------------------------------------------

    /// The predicate does not read [`FilterConfig`]. Relevance is a function of
    /// the posting and the source's overrides; the version is what the caller
    /// stores beside the answer, and INV-15's suppression reads it in
    /// [`crate::diff`].
    ///
    /// Pinning this stops a future edit from quietly making the filter
    /// version-dependent, which would let the same posting mean different things
    /// on either side of a bump for a reason no stored record explains.
    #[test]
    fn the_verdict_does_not_depend_on_the_filter_version() {
        let job = posting("Software Engineering Intern", "Toronto, ON", None);
        let overrides = FilterOverrides::default();

        for filter_version in [0, FILTER_VERSION, FILTER_VERSION + 1, u32::MAX] {
            assert!(
                is_relevant(&job, &FilterConfig { filter_version }, &overrides),
                "filter_version {filter_version} changed the verdict"
            );
        }
    }
}
