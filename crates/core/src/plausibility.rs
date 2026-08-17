//! Per-source suspicious-response gate (§22, INV-4).
//!
//! This is the last of §22's four conditions and the only one that judges a
//! response against *stored* state. The other three — shape changed, contract
//! violated, parse failed — are judgements about the response alone, and §22 opens
//! by naming the conflation of the four as the most common way this class of
//! system corrupts itself. Nothing here re-checks the contract, and
//! [`crate::shape::validate_contract`] deliberately does not check any count.
//!
//! # What a rejection buys (INV-4)
//!
//! An upstream that returns an empty or truncated array after a soft block, a
//! broken paginator, or an incomplete response is indistinguishable, to `diff`,
//! from a board that genuinely emptied out. Believing it emits 47 `JOB_REMOVED`
//! events and overwrites known-good canonical state with the truncated view. INV-4
//! forbids exactly that: a rejection here fails the poll, leaves every stored job
//! untouched, and raises one `SOURCE_FAILED`.
//!
//! Both functions therefore populate [`Detail::prev_job_count`] and
//! [`Detail::parsed_count`], which is what lets the §8 alert body render its
//! `Job count: previous 53 → parsed 0` line without re-deriving anything.
//!
//! # Bootstrap has no baseline
//!
//! [`check`] compares against a previous count, so the first poll of a source
//! cannot use it (§7). [`check_bootstrap`] applies the adapter contract's own
//! `min_expected` floor instead, and reports the same stage, domain and kind — so
//! downstream the two paths are one §22 row.

use crate::model::{AdapterContract, PlausibilityConfig};
use jobmon_errors::{Detail, FailureKind, FaultDomain, PipelineError, Stage};

/// Is this parsed count believable against the last one we stored? (§22)
///
/// The rule, exactly as §22 writes it:
///
/// ```text
/// if parsed_count == 0:
///     if last_job_count == 0: accept
///     if allow_zero:          accept
///     reject                              // prior count > 0 and zero is not allowed
///
/// // Only NONZERO parsed counts reach the ratio gate.
/// reject if last_job_count >= min_abs && parsed_count < min_ratio * last_job_count
/// accept otherwise
/// ```
///
/// # The two sentences this function exists to get right
///
/// **The zero branch short-circuits the ratio rule.** `allow_zero = true` accepts
/// a zero response regardless of how large the previous count was; the ratio rule
/// is not evaluated at all in that case.
///
/// **`min_abs` floors the ratio rule only — it does not suppress the zero rule**,
/// which is why `last_job_count = 2`, `parsed_count = 0`, `min_abs = 3` rejects.
///
/// §22 names that `2 → 0` row as the case this is consistently got wrong, and
/// §30.2 makes it a required regression test. Getting it wrong means a suspicious
/// empty response mutates canonical state, which is an INV-4 violation — the exact
/// failure this module exists to prevent.
///
/// `allow_zero` is intentionally a strong override and defaults to `false` (§20);
/// the safety boundary is operational rather than mathematical. It is per source,
/// and a nonzero partial collapse such as `54 → 7` is still rejected by the ratio
/// rule. Letting the ratio rule veto `54 → 0` would make the override ineffective
/// precisely when a populated board becomes empty, which is the one situation an
/// operator sets it for.
///
/// # Why the comparison widens to `f64`
///
/// [`PlausibilityConfig::min_ratio`] is stored as `f32` per §20, and §22's
/// boundary rows — `54 → 27` accepts, `54 → 26` rejects — are exactly where a
/// narrowing conversion changes the answer. Both sides are therefore widened to
/// `f64` and compared there.
///
/// # Errors
///
/// [`Stage::Plausibility`] / [`FaultDomain::Adapter`] /
/// [`FailureKind::PlausibilityFailed`], carrying both counts in [`Detail`].
pub fn check(
    parsed_count: usize,
    last_job_count: u32,
    cfg: &PlausibilityConfig,
) -> Result<(), PipelineError> {
    // The zero branch decides on its own and returns; the ratio gate below is
    // unreachable for a zero parsed count.
    if parsed_count == 0 {
        if last_job_count == 0 {
            // First poll, or a board that was already empty: there is no
            // known-good baseline for zero to be suspicious against.
            return Ok(());
        }
        if cfg.allow_zero {
            return Ok(());
        }
        return Err(plausibility_failed(
            Some(last_job_count as usize),
            parsed_count,
            format!(
                "parsed 0 jobs but the previous poll stored {last_job_count}; \
                 allow_zero is false for this source"
            ),
        ));
    }

    // Only nonzero parsed counts reach here. `min_abs` is a floor on *this* rule
    // and nothing else — it stops tiny boards being rejected for ordinary
    // small-number fluctuation, as in §22's `2 → 1` row.
    if last_job_count < cfg.min_abs {
        return Ok(());
    }

    let threshold = f64::from(cfg.min_ratio) * f64::from(last_job_count);
    // `usize as f64` is exact for every job count a board can plausibly return;
    // §22 requires the comparison itself to happen in `f64`.
    if (parsed_count as f64) < threshold {
        return Err(plausibility_failed(
            Some(last_job_count as usize),
            parsed_count,
            format!(
                "parsed {parsed_count} jobs, below min_ratio {} of the previous {last_job_count} \
                 (threshold {threshold}, min_abs {})",
                cfg.min_ratio, cfg.min_abs
            ),
        ));
    }

    Ok(())
}

/// The first-poll substitute for [`check`], where no baseline exists (§7, §22).
///
/// Rejects when `parsed_count < contract.min_expected`. There is no previous count
/// to report, so [`Detail::prev_job_count`] stays `None` and the floor that was
/// violated is stated in the message instead.
///
/// # Errors
///
/// The same [`Stage::Plausibility`] / [`FaultDomain::Adapter`] /
/// [`FailureKind::PlausibilityFailed`] triple [`check`] uses — §22 requires the
/// bootstrap path to be indistinguishable from the steady-state path once the
/// failure is classified.
pub fn check_bootstrap(
    parsed_count: usize,
    contract: &AdapterContract,
) -> Result<(), PipelineError> {
    if parsed_count < contract.min_expected {
        return Err(plausibility_failed(
            None,
            parsed_count,
            format!(
                "bootstrap parsed {parsed_count} jobs, below the adapter contract's min_expected \
                 of {}",
                contract.min_expected
            ),
        ));
    }

    Ok(())
}

/// The one §22 classification both checks produce.
///
/// A single constructor rather than two: §22 puts steady-state and bootstrap
/// implausibility on the same row, and two constructors would let them drift onto
/// different stages without anything failing.
fn plausibility_failed(
    prev_job_count: Option<usize>,
    parsed_count: usize,
    message: String,
) -> PipelineError {
    PipelineError::new(
        Stage::Plausibility,
        FaultDomain::Adapter,
        FailureKind::PlausibilityFailed,
        message,
    )
    .with_detail(Detail {
        prev_job_count,
        parsed_count: Some(parsed_count),
        ..Detail::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §22's worked-example table fixes `min_abs = 3` and `min_ratio = 0.5` for
    /// every row. Written out rather than taken from [`PlausibilityConfig::default`]
    /// so these tests pin the table's values even if §20's defaults ever move.
    fn cfg(allow_zero: bool) -> PlausibilityConfig {
        PlausibilityConfig {
            min_ratio: 0.5,
            min_abs: 3,
            allow_zero,
        }
    }

    /// Every rejection on either path is one §22 row, and both counts must reach
    /// [`Detail`] so §8 can render `Job count: previous 53 → parsed 0`.
    fn assert_rejected(err: &PipelineError, prev_job_count: Option<usize>, parsed_count: usize) {
        assert_eq!(err.stage, Stage::Plausibility);
        assert_eq!(err.domain, FaultDomain::Adapter);
        assert_eq!(err.kind, FailureKind::PlausibilityFailed);
        assert_eq!(err.detail.prev_job_count, prev_job_count);
        assert_eq!(err.detail.parsed_count, Some(parsed_count));
    }

    // -----------------------------------------------------------------------
    // §22 worked-example table — one test per row, named after the row
    // -----------------------------------------------------------------------

    /// Row 1: `54 | 0 | 3 | 0.5 | false` → **reject**, zero branch.
    #[test]
    fn prev_54_parsed_0_allow_zero_false_rejects_on_the_zero_branch() {
        let err = check(0, 54, &cfg(false)).expect_err("a populated board must not empty silently");
        assert_rejected(&err, Some(54), 0);
    }

    /// Row 2: `54 | 0 | 3 | 0.5 | true` → **accept**, `allow_zero` short-circuit;
    /// the ratio rule is not evaluated.
    #[test]
    fn prev_54_parsed_0_allow_zero_true_accepts_by_short_circuit() {
        assert!(
            check(0, 54, &cfg(true)).is_ok(),
            "allow_zero must accept a zero response however large the previous count was"
        );
    }

    /// Row 3: `2 | 0 | 3 | 0.5 | false` → **reject**. `min_abs` floors the ratio
    /// rule only and does not suppress the zero branch. §30.2 lists this row by
    /// name as an INV-4 regression test.
    #[test]
    fn prev_2_parsed_0_min_abs_3_rejects_because_min_abs_does_not_suppress_the_zero_branch() {
        let err = check(0, 2, &cfg(false)).expect_err("min_abs does not reach the zero branch");
        assert_rejected(&err, Some(2), 0);
    }

    /// Row 4: `2 | 0 | 3 | 0.5 | true` → **accept**, `allow_zero` short-circuit.
    #[test]
    fn prev_2_parsed_0_allow_zero_true_accepts_by_short_circuit() {
        assert!(check(0, 2, &cfg(true)).is_ok());
    }

    /// Row 5: `54 | 7 | 3 | 0.5 | false` → **reject**, nonzero ratio rule, `7 < 27`.
    #[test]
    fn prev_54_parsed_7_rejects_on_the_nonzero_ratio_rule() {
        let err = check(7, 54, &cfg(false)).expect_err("7 < 0.5 * 54");
        assert_rejected(&err, Some(54), 7);
    }

    /// Row 6: `54 | 27 | 3 | 0.5 | false` → **accept**, ratio boundary; `27 == 27`
    /// is not `<`. This is one of the two rows a narrowing `f32` comparison flips.
    #[test]
    fn prev_54_parsed_27_accepts_exactly_at_the_ratio_boundary() {
        assert!(
            check(27, 54, &cfg(false)).is_ok(),
            "the rule rejects strictly below the threshold, not at it"
        );
    }

    /// Row 7: `54 | 26 | 3 | 0.5 | false` → **reject**, nonzero ratio rule. The
    /// other side of the boundary in row 6.
    #[test]
    fn prev_54_parsed_26_rejects_just_below_the_ratio_boundary() {
        let err = check(26, 54, &cfg(false)).expect_err("26 < 0.5 * 54");
        assert_rejected(&err, Some(54), 26);
    }

    /// Row 8: `2 | 1 | 3 | 0.5 | false` → **accept**, ratio rule skipped because
    /// `2 < min_abs`.
    #[test]
    fn prev_2_parsed_1_accepts_because_the_previous_count_is_below_min_abs() {
        assert!(
            check(1, 2, &cfg(false)).is_ok(),
            "tiny boards fluctuate; min_abs exists to stop that being a failure"
        );
    }

    /// Row 9: `0 | 0 | 3 | 0.5 | false` → **accept**, first poll; there is no
    /// prior nonzero baseline for zero to contradict.
    #[test]
    fn prev_0_parsed_0_accepts_because_there_is_no_prior_nonzero_baseline() {
        assert!(check(0, 0, &cfg(false)).is_ok());
    }

    // -----------------------------------------------------------------------
    // check_bootstrap (§7, §22)
    // -----------------------------------------------------------------------

    const CONTRACT: AdapterContract = AdapterContract {
        array_path: "jobs",
        required_paths: &["id", "title"],
        min_expected: 5,
    };

    #[test]
    fn bootstrap_accepts_at_and_above_min_expected() {
        assert!(
            check_bootstrap(5, &CONTRACT).is_ok(),
            "the floor rejects strictly below min_expected, not at it"
        );
        assert!(check_bootstrap(9, &CONTRACT).is_ok());
    }

    #[test]
    fn bootstrap_rejects_below_min_expected_as_a_plausibility_failure() {
        let err = check_bootstrap(4, &CONTRACT).expect_err("4 < min_expected 5");

        // Same §22 row as the steady-state path, but with no baseline to report.
        assert_rejected(&err, None, 4);
        assert!(
            err.detail.message.contains("min_expected") && err.detail.message.contains('5'),
            "the violated floor is only visible in the message: {}",
            err.detail.message
        );
    }
}
