# Phase 1 audit report — Pure core

**Spec:** `documents/JOB_MONITOR_MASTER_SPEC_v1.2.2_phase-1-ready.md` (v1.2.2-phase-1-ready)
**Audited:** 2026-08-17
**Audit scope:** read-only. No Rust, no manifest, no spec, no test and no dependency was written or
modified in this session. The only file created is this report. No git command was run.

---

## 0. Headline

**All four CI gates pass. No required Phase-1 test is missing. Phase 1 is ready for Phase 2.**

One spec deviation was found and is recorded in §5: `core::normalize` omits `CA` from the US
two-letter raw-code table, where §21.1 says "all 50 state two-letter codes plus `DC`". It is
deliberate, documented in code, and covered by a test, but it is a change to a filtering policy
v1.2.2 pinned, and §39.14 requires it to be surfaced rather than absorbed.

---

## 1. CI gate results

| # | Gate | Result | Wall time |
|---|---|---|---|
| 1 | `cargo fmt --all -- --check` | **PASS** (exit 0, no output) | 1.29 s |
| 2 | `cargo clippy --workspace --all-targets --locked -- -D warnings` | **PASS** (exit 0, zero warnings) | 1.05 s |
| 3 | `cargo test --workspace --locked` | **PASS** (exit 0, 178 passed / 0 failed / 0 ignored) | 7.20 s |
| 4 | `RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps` | **PASS** (exit 0, 8 crates documented) | 0.49 s |

Gates 2 and 4 were re-run without shell stderr redirection to confirm the exit codes; the
`NativeCommandError` lines in the first capture were an artifact of PowerShell's `*>&1` on a native
executable, not a cargo diagnostic.

### Test count and runtime

| Target | Tests | Runtime |
|---|---|---|
| `jobmon-core` unit (`crates/core/src/lib.rs`) | 164 | 0.10 s |
| `crates/core/tests/event_key_regression.rs` (integration) | 8 | 0.00 s |
| `jobmon-errors` unit (`crates/errors/src/lib.rs`) | 5 | 0.00 s |
| Doc-tests `jobmon_errors` | 1 | 0.02 s |
| `adapters`, `engine`, `infra`, `ports`, `lambda`, `admin` | 0 each | — |
| **Total** | **178** | **0.12 s aggregate test execution** |

**Against §32's acceptance criteria:** "~70 tests, sub-second, no network, no mocks, no async. **No
`proptest`**."

- **178 tests** — above the ~70 target. §32 states a target, not a maximum; the surplus is
  traceability, not padding. The largest blocks are the 23 named §8.1 row tests, the 9 named §22
  worked-example rows, and the 6 named §13.3 transitions, each written as its own test named after
  the spec row it defends. No test was removed to approach 70.
- **Sub-second** — 0.12 s of test execution; the 7.20 s gate figure is compilation.
- **No network, no mocks, no async, no `proptest`** — confirmed by the scope audit in §4.

---

## 2. §32 Phase-1 required tests → concrete test functions

Every item in §32 Phase 1's `Tests:` line, mapped. All paths are repo-relative; line numbers are the
`fn` declaration.

### 2.1 Event key — repeat / retry / golden vectors

| §32 requirement | Test(s) |
|---|---|
| repeat-transition distinct keys | `repeat_transition_produces_distinct_keys` — `crates/core/tests/event_key_regression.rs:74` |
| | `full_lifecycle_yields_five_distinct_keys` — `crates/core/tests/event_key_regression.rs:141` |
| | `repeat_reposts_feed_job_event_key_and_produce_distinct_keys` — `crates/core/src/diff.rs:937` (driven end-to-end through `diff`) |
| retry-stable keys | `replayed_transition_produces_identical_key` — `crates/core/tests/event_key_regression.rs:123` |
| the pinned golden vector | `golden_vector_job_event` — `crates/core/tests/event_key_regression.rs:180` |
| | `golden_vector_source_health_event` — `:205` |
| | `golden_vector_system_event` — `:231` |

`repeat_transition_produces_distinct_keys` implements §38's mandated control exactly: it holds
`transition_seq` constant across two reposts and asserts the keys **collide**, then lets it advance
and asserts they do not — so `transition_seq` is demonstrably the only variable in play.

Supporting encoding tests, which §32 does not name individually but which pin §13.2.1 as durable
schema:

- `timestamp_fraction_is_normalised` — `event_key_regression.rs:260` (`…:07Z` and `…:07.000Z` derive one key)
- `event_key_shape` — `event_key_regression.rs:279` (all seven §13.2.3 shapes are 26 chars of RFC 4648 *standard* Base32)
- `frozen_primitive_encodings` — `crates/core/src/event_key.rs:461` (`Bool`, `Opt`, `List` and the N−1-separator rule, which no §13.2.3 shape reaches)

The golden vectors are stated as pre-digest hex **and** as the final key, so a change to the byte
encoding fails with a readable diff rather than an opaque hash mismatch.

### 2.2 All six diff transitions, and precedence

| §13.3 row | Test — `crates/core/src/diff.rs` |
|---|---|
| 1. `JOB_REPOSTED` | `job_reposted_fires_when_a_present_job_was_stored_inactive:509` |
| 2. `NEW_JOB` | `new_job_fires_for_an_id_absent_from_the_index:494` |
| 3. `BECAME_RELEVANT` | `became_relevant_fires_when_relevance_goes_false_to_true:531` |
| 4. `BECAME_IRRELEVANT` | `became_irrelevant_fires_when_relevance_goes_true_to_false:548` |
| 5. `JOB_UPDATED` | `job_updated_fires_when_only_the_content_hash_moved:562` |
| 6. `JOB_REMOVED` | `job_removed_fires_one_poll_after_the_marker:576` |

Precedence collapsing (INV-13):

- `a_repost_that_is_also_newly_relevant_emits_only_job_reposted:807` — precedence 1 over 3, and asserts the collapsed relevance still reaches `after` and the `JobWrite`
- `a_became_irrelevant_job_whose_content_also_changed_emits_only_became_irrelevant:830` — precedence 4 over 5, same collapse assertion
- Every diff test routes through the `only()` helper (`diff.rs:471`), which asserts exactly one transition per job per poll — so INV-13 is enforced in all 19 diff tests, not only the two precedence ones.
- `transitions_are_sorted_by_external_id_byte_order:917` — §13.4's pre-chunking sort, with two ids arriving from the absent pass and two from the present pass, so it also pins that the passes are merged rather than concatenated.

### 2.3 `JOB_REMOVED` marker / TTL / `last_seen_at`, and `JOB_REPOSTED` clear semantics

| Requirement | Test — `crates/core/src/diff.rs` |
|---|---|
| `JOB_REMOVED` retains `absent_since_poll`, preserves `last_seen_at`, sets `ttl = now + 180 d` | `job_removed_retains_the_marker_preserves_last_seen_at_and_sets_a_180_day_ttl:687` — asserts the whole `JobFacts` and the whole `JobWrite::MarkInactive`, with the 180-day horizon restated as a literal instant (`2027-02-13T10:00:00Z`) independently of the `INACTIVE_TTL` constant |
| `JOB_REPOSTED` clears both marker and TTL | `job_reposted_writes_update_active_clearing_the_marker_and_the_ttl:640` — asserts `clear_absent_since_poll: true` **and** `clear_ttl: true`, plus `first_seen_at`/`bootstrapped` preserved |
| `NEW_JOB` exact canonical facts | `new_job_writes_the_exact_canonical_facts:600` |
| newly-absent job gets one marker carrying `current_poll_seq` | `a_newly_absent_active_job_gets_one_marker_carrying_the_current_poll_seq:734` |
| continued absence of an inactive job writes nothing (§13.8 last line) | `continued_absence_of_an_inactive_job_writes_nothing_at_all:752` |
| returning job before threshold → one absence clear only | `a_returning_unchanged_job_produces_one_absence_clear_and_nothing_else:770` |
| §30.2 "unchanged present jobs across 100 polls → zero writes" | `unchanged_present_jobs_never_write:787` |

INV-15 / §21.3 routing:

- `a_filter_version_bump_suppresses_the_relevance_event_and_routes_a_reclassify:851`
- `a_reclassified_job_whose_content_changed_carries_the_new_relevance_in_job_updated:879`

### 2.4 The §22 plausibility matrix, verbatim

One test per §22 worked-example row, named after the row — `crates/core/src/plausibility.rs`:

| `last` | `parsed` | `min_abs` | `allow_zero` | Expected | Test |
|---|---|---|---|---|---|
| 54 | 0 | 3 | false | reject | `prev_54_parsed_0_allow_zero_false_rejects_on_the_zero_branch:216` |
| 54 | 0 | 3 | **true** | accept | `prev_54_parsed_0_allow_zero_true_accepts_by_short_circuit:224` |
| **2** | **0** | **3** | false | **reject** | `prev_2_parsed_0_min_abs_3_rejects_because_min_abs_does_not_suppress_the_zero_branch:235` |
| 2 | 0 | 3 | true | accept | `prev_2_parsed_0_allow_zero_true_accepts_by_short_circuit:242` |
| 54 | 7 | 3 | false | reject | `prev_54_parsed_7_rejects_on_the_nonzero_ratio_rule:248` |
| 54 | 27 | 3 | false | accept | `prev_54_parsed_27_accepts_exactly_at_the_ratio_boundary:256` |
| 54 | 26 | 3 | false | reject | `prev_54_parsed_26_rejects_just_below_the_ratio_boundary:266` |
| 2 | 1 | 3 | false | accept | `prev_2_parsed_1_accepts_because_the_previous_count_is_below_min_abs:274` |
| 0 | 0 | 3 | false | accept | `prev_0_parsed_0_accepts_because_there_is_no_prior_nonzero_baseline:284` |

Plus `check_bootstrap`: `bootstrap_accepts_at_and_above_min_expected:299` and
`bootstrap_rejects_below_min_expected_as_a_plausibility_failure:308`.

Every rejection asserts the full §22 triple (`Stage::Plausibility` / `FaultDomain::Adapter` /
`FailureKind::PlausibilityFailed`) **and** that both counts reach `Detail`, which is what §8's alert
body renders. The two boundary rows (27 accepts / 26 rejects) are exactly where the required `f64`
widening of an `f32` `min_ratio` changes the answer — `plausibility.rs:113` does it in `f64`.

### 2.5 Probe / backoff / `Retry-After` / jitter

All in `crates/core/src/schedule.rs`.

| §11.2 row | Test |
|---|---|
| success → interval + jitter, no probe | `success_schedules_one_interval_plus_jitter_and_no_probe:455` |
| 304 identical to success | `not_modified_schedules_exactly_like_success:470` |
| jitter clamped to `min(10 %, 30 s)` | `supplied_jitter_is_clamped_to_the_bound:486`, `max_jitter_crosses_from_ten_percent_to_thirty_seconds_at_a_five_minute_interval:515` |
| probe #1 from `HEALTHY` | `healthy_transient_with_no_prior_probe_schedules_the_thirty_second_probe:540` |
| probe #2 from `DEGRADED` while health has already moved to `FAILED` | `degraded_transient_at_probe_one_still_probes_although_health_moved_it_to_failed:557` |
| probe after a 429 run (no probe was spent) | `degraded_transient_after_a_rate_limit_run_probes_because_no_probe_was_spent:575` |
| both probes spent → backoff | `degraded_transient_with_both_probes_spent_backs_off_instead_of_probing:592`, `failed_transient_with_both_probes_spent_backs_off:630` |
| exponential backoff from the 2nd consecutive failure | `exponential_backoff_doubles_from_the_second_consecutive_failure:649` |
| cap at 2 h without overflow at `cf = 20` | `exponential_backoff_caps_at_two_hours_without_overflowing:684` |
| hard backoff carries jitter | `hard_backoff_carries_clamped_jitter:792` |
| `Retry-After` floor / honoured / absent | `rate_limited_floors_a_short_retry_after_at_sixty_seconds:886`, `rate_limited_honours_a_longer_retry_after_exactly:894`, `rate_limited_without_a_retry_after_uses_the_floor:900` |
| whole §10.4 backoff column | `backoff_for_reproduces_the_ten_point_four_column:820` (and `not_found_backs_off_one_hour:763`, `required_field_missing_backs_off_thirty_minutes:769`, `plausibility_failed_keeps_the_normal_interval:777`) |
| no wraparound on an absurd interval | `an_out_of_range_interval_saturates_instead_of_wrapping:913` |

**The two §30.2 regressions §32 names explicitly:**

- **`FAILED` + transient + `probe_attempts = 0` uses backoff, not a new probe sequence** —
  `failed_transient_with_zero_probe_attempts_backs_off_and_starts_no_probe_sequence:615`
- **`INITIALIZING` transient before the failure threshold uses the normal interval** —
  `initializing_transient_that_stays_initializing_uses_the_normal_interval:714`, with its companion
  `initializing_third_transient_that_enters_failed_uses_exponential_backoff:732` proving the boundary
  is the state transition and not the outcome.

The pre/post split §10.3 requires is structural: `ScheduleInput` carries `state_before` /
`probe_attempts_before` alongside `state_after` / `consecutive_failures_after`
(`schedule.rs:106-128`), and `probe_eligible` (`:229`) reads only the pre-poll pair.

### 2.6 Every §8.1 row, both quarantine routes, 25×429, bootstrap recovery

All 23 rows of §8.1 have a test named after the row — `crates/core/src/health.rs`:

| Row | Test |
|---|---|
| 1 `INITIALIZING` success, bootstrap incomplete | `row_01_initializing_success_bootstrap_incomplete_stays_initializing:673` |
| 2 `INITIALIZING` success, bootstrap complete | `row_02_initializing_success_bootstrap_complete_becomes_healthy:686` |
| 3 `INITIALIZING` transient `cf < 3` | `row_03_initializing_transient_below_limit_stays_initializing:707` |
| 4 `INITIALIZING` transient `cf == 3` | `row_04_initializing_third_transient_fails:721` |
| 5 `INITIALIZING` 429 | `row_05_initializing_rate_limited_degrades:740` |
| 6 `INITIALIZING` hard | `row_06_initializing_hard_fails_on_first_observation:761` |
| 7 `HEALTHY` success | `row_07_healthy_success_stays_healthy:784` + `row_07_healthy_not_modified_is_a_success:799` |
| 8 `HEALTHY` transient | `row_08_healthy_transient_degrades_and_spends_probe_one:812` |
| 9 `HEALTHY` 429 | `row_09_healthy_rate_limited_degrades_without_spending_a_probe:833` |
| 10 `HEALTHY` hard | `row_10_healthy_hard_fails_immediately:852` |
| 11 `DEGRADED` success, bootstrap incomplete | `row_11_degraded_success_bootstrap_incomplete_returns_to_initializing:875` |
| 12 `DEGRADED` success, bootstrap complete | `row_12_degraded_success_bootstrap_complete_recovers:888` |
| 13 `DEGRADED` transient | `row_13_degraded_transient_fails_and_spends_probe_two:907` |
| 14 `DEGRADED` hard | `row_14_degraded_hard_fails_and_resets_probes:928` |
| 15 `DEGRADED` 429 `cf < 20` | `row_15_degraded_rate_limited_below_limit_stays_degraded_and_silent:949` |
| 16 `DEGRADED` 429 `cf == 20` | `row_16_degraded_rate_limited_at_limit_quarantines:962` |
| 17 `FAILED` success, bootstrap incomplete | `row_17_failed_success_bootstrap_incomplete_returns_to_initializing:985` |
| 18 `FAILED` success, bootstrap complete | `row_18_failed_success_bootstrap_complete_recovers:998` |
| 19 `FAILED` any failure `cf < 20` | `row_19_failed_any_failure_below_limit_stays_failed_and_silent:1020` |
| 20 `FAILED` any failure `cf == 20` | `row_20_failed_any_failure_at_limit_quarantines:1039` |
| 21 `QUARANTINED` not polled | `row_21_quarantined_is_never_polled:1070` (+ `disabled_is_never_polled_either:1091`) |
| 22 any → `DISABLED` | `row_22_disable_from_any_state:1101` |
| 23 `QUARANTINED`/`DISABLED` → `INITIALIZING` | `row_23_enable_from_quarantined_or_disabled_reinitializes:1120` (+ `enable_is_a_no_op_for_a_source_that_is_already_enabled:1134`) |

**Bootstrap recovery (INV-10, §13.6):**

- `bootstrap_hazard_a_failed_initialization_recovers_to_initializing:1271` — hard failure →
  `FAILED` → success → `INITIALIZING` with counters and `first_failure_at` cleared and **no**
  `SOURCE_RECOVERED`, then only the bootstrap commit reaches `HEALTHY` with one
  `SOURCE_BOOTSTRAPPED`. Asserts the whole emitted sequence is exactly
  `[SOURCE_FAILED, SOURCE_BOOTSTRAPPED]`.
- `bootstrap_hazard_a_rate_limited_initialization_recovers_to_initializing:1317` — the same hazard
  reached through 429 → `DEGRADED`.
- `assert_no_job_events:1358` — a guard asserting `core::health` emits no job-lifecycle event on any
  of these paths, so a future change that gave it one fails loudly.

**Both routes into `QUARANTINED`:**

- 429 route — `twenty_five_consecutive_429s_quarantine_once_and_never_fail:1381`
- failure route — `twenty_consecutive_hard_failures_quarantine_through_failed:1426`
- identity — `quarantine_events_carry_the_outage_start:1452` (both routes, one outage start)
- INV-16 corruption guard — `a_counter_past_the_threshold_still_quarantines:1468`

**The 25-poll 429 run (§30.2, INV-16)** — `twenty_five_consecutive_429s_quarantine_once_and_never_fail:1381`
asserts, per poll across all 25: never `FAILED`, `probe_attempts` never leaves 0; and in aggregate
that `SOURCE_DEGRADED` fires only at poll 1, `SOURCE_QUARANTINED` only at poll 20, polls 21–25 change
nothing, and `first_failure_at` is one instant for the whole run.

**`first_failure_at` discriminator (§13.2.3):** `first_failure_at_is_set_on_the_first_failure_of_an_outage:1152`,
`…_is_preserved_for_the_length_of_the_outage:1173`, `…_is_cleared_by_any_success:1194`,
`source_recovered_carries_the_pre_clear_first_failure_at:1219`,
`a_missing_first_failure_at_is_repaired_rather_than_inherited:1240`.

**`PollOutcome` mapping (§17.3.1, INV-6/INV-11):** `transient_kinds_map_to_transient:1556`,
`rate_limited_maps_to_its_own_class:1569`, `hard_kinds_map_to_hard:1577`,
`shape_changed_is_not_a_source_failure:1599`, `success_signals_are_not_failures:1605`,
`infra_kinds_never_reach_source_health:1614`, `notify_kinds_never_reach_source_health:1630`,
`archive_failure_never_reaches_source_health:1643`.

### 2.7 Shape vs contract, and the canonical §18 structured-path algorithm

All in `crates/core/src/shape.rs`.

| Requirement | Test |
|---|---|
| **INV-11 in one test** — sibling field moves the shape hash **and** the contract still validates | `a_new_sibling_field_changes_the_shape_but_not_the_contract:805` |
| §18 path is not dotted text — punctuation collisions | `structural_paths_cannot_collide_with_punctuation_in_keys:777` (`{"a.b":…}` vs `{"a":{"b":…}}`, `{"a[]":…}` vs `{"a":[…]}`, and `{"ab":{"c":…}}` vs `{"a":{"bc":…}}` for the key-length prefix) |
| union across **all** elements, order-invariant | `shape_hash_ignores_top_level_element_order:708` |
| array length / index renumbering invisible | `shape_hash_ignores_array_length_and_index_renumbering:723` (both top level and nested) |
| values discarded | `shape_hash_discards_values:743` |
| empty containers recorded | `shape_hash_records_empty_containers:759` |
| `array_path` resolution, incl. empty path = root array | `validates_a_root_array_with_nested_required_paths:835`, `validates_a_nested_array_path:853` |
| `ArrayPathMissing`, all three ways | `a_missing_or_non_array_array_path_is_array_path_missing:870` |
| `RequiredFieldMissing` on a **later** element | `a_required_path_missing_on_a_later_element_is_required_field_missing:906` |
| nested required path missing via its parent | `a_nested_required_path_is_missing_when_its_parent_is:937` |
| a `null` required path is **present** | `a_null_required_path_is_present:956` |
| contract validation does **not** check `min_expected` | `contract_validation_does_not_check_min_expected:971` |

Both contract failures assert `Stage::Parse` (never `Stage::Schema`), `FaultDomain::Adapter`, and
that `Detail::missing_paths` carries required paths only — which is §31 acceptance criterion 2.

### 2.8 Full 52-character hashes and separator ambiguity

| Requirement | Test — `crates/core/src/shape.rs` |
|---|---|
| `body_hash` full-width, deterministic, byte-sensitive | `body_hash_is_full_width_deterministic_and_byte_sensitive:563` (incl. `{"a":1}` vs `{"a": 1}`) |
| `content_hash` full-width and stable | `content_hash_is_full_width_and_stable:593` |
| all five covered fields move it | `content_hash_changes_for_every_covered_field:608` (title, `location_raw`, `employment_type`, `url`, and `posted_at` both absent→present and value→value) |
| the excluded fields do not | `content_hash_ignores_fields_outside_the_covered_five:644` |
| **separator-ambiguity regression** | `hashes_are_unambiguous_across_separator_bytes:668` |
| `shape_hash` full-width and stable | `shape_hash_is_full_width_and_stable:697` |
| frozen golden vectors for `body_hash` / `content_hash` / `shape_hash` | `frozen_golden_vectors:999` |

`hashes_are_unambiguous_across_separator_bytes` runs for **both** `0x1F` and `0x1E`. Each iteration
first asserts the *precondition* — that separator concatenation genuinely cannot tell the two
postings apart — and only then asserts that `content_hash` does. Without that precondition the test
could pass while testing nothing.

Every hash assertion routes through `assert_full_width_base32` (`shape.rs:545`), which pins 52
characters **and** the RFC 4648 *standard* alphabet (`A`–`Z`, `2`–`7`), so a switch to `BASE32HEX`
fails too.

### 2.9 Normalization tables, whole-token behaviour, relevance location overrides

`crates/core/src/normalize.rs`:

| Requirement | Test |
|---|---|
| province code → Canada + region + city | `province_code_yields_canada_region_and_city:708` |
| province full name → code | `province_full_name_yields_the_code:720` |
| US state code → NotCanada | `us_state_code_yields_not_canada:729` |
| the `CA` ambiguity | `ambiguous_ca_code_decides_nothing_on_its_own:744` — see §5 |
| raw-code rule: lowercase prose is never a region | `lowercase_prose_is_never_a_region_code:789` (`work in office`, `based on team`) |
| **ambiguity resolves to Canada** | `ambiguity_resolves_to_canada:797` (`Toronto, ON / New York, NY`, `Remote — Canada & US`) |
| unresolved | `unrecognised_location_is_unresolved:809` |
| bare Canadian city, no region | `bare_canadian_city_is_canada_without_a_region:816` |
| deliberately omitted ambiguous city names | `ambiguous_city_names_are_omitted_from_the_canadian_list:827` (`London, UK` → NotCanada, bare `London` → unresolved) |
| remote derivation | `remote_us_is_not_canada_and_remote:778` |
| one tokenizer, punctuation-insensitive | `punctuation_does_not_change_tokens:837` |
| **whole-token, not substring** | `matching_is_whole_token_not_substring:848` |
| employment type: raw field first, then title | `employment_type_prefers_the_raw_field_then_the_title:863` |
| accepted values preserved untrimmed | `accepted_values_are_carried_through_untrimmed:889` |
| `NormalizeFailed` on empty/blank/control-byte fields | `invalid_fields_are_normalize_failures:688`, `valid_raw:652` |

`crates/core/src/filter.rs`:

| §21.2 requirement | Test |
|---|---|
| rule 1 — resolved Canadian | `canadian_internship_is_relevant:299` |
| rule 3 — resolved non-Canadian fails | `us_internship_fails_at_rule_3:309` |
| rule 2 above rule 3 | `remote_not_canada_passes_only_with_accept_remote_canada:324` |
| **rule 4 default = relevant** | `unresolved_location_is_relevant_under_the_default_override:347` |
| **rule 4 override = not_relevant** | `unresolved_location_policy_not_relevant_turns_rule_4_off:361` |
| role gate rejects on its own | `canadian_full_time_role_fails_the_role_gate:382` |
| exclusions last and win outright | `exclusions_win_over_a_passing_role_gate:400` |
| **whole-token in both directions** | `matching_is_whole_token_not_substring:422` |
| employment-type arm of the role gate | `co_op_employment_type_alone_makes_a_title_relevant:434` |
| tokenizer collapses spellings | `co_op_spellings_are_indistinguishable:451` |
| the predicate does not read `filter_version` | `the_verdict_does_not_depend_on_the_filter_version:477` |

The §30.2 row **"`Internal Tools Engineer` does not match the `intern` keyword"** is covered twice —
at the tokenizer level (`normalize.rs:848`) and at the predicate level (`filter.rs:422`, which also
asserts the converse: `Leadership Development Intern` must *not* be excluded by `lead`).

The filter tests build their fixtures through the **real** normalizer (`posting()`, `filter.rs:272`),
so they assert against the classifications §21.1 actually produces rather than hand-labelled data
that could drift.

### 2.10 Criticality validation

| Requirement | Test |
|---|---|
| §10.2 ceilings | `criticality_ceilings_are_the_declared_blind_spots` — `crates/core/src/model.rs:1325` (300 / 600 / 1800 s, and that `failure_detection_sla_secs` is **derived** from the ceiling, never stored separately) |
| accepts at the ceiling | `validate_interval_accepts_every_criticality_at_its_ceiling` — `crates/core/src/schedule.rs:930` |
| rejects above it | `validate_interval_rejects_an_interval_above_the_ceiling` — `schedule.rs:947` |
| measures the override, not the base | `validate_interval_measures_the_override_rather_than_the_base` — `schedule.rs:974` |

### 2.11 Model / errors traceability (not named individually in §32, listed for completeness)

`crates/core/src/model.rs`: `external_id_rejects_empty_whitespace_and_control_bytes:1289`,
`external_id_deserialization_validates:1311`, `poll_outcome_wire_names_and_success_classification:1342`,
`event_type_wire_names_agree_with_serde:1372` (asserts §14 defines exactly 16 types),
`event_type_notify_worthy_matches_section_14:1423`, `only_four_event_types_are_system_scoped:1467`,
`employment_type_wire_names_agree_with_serde:1497`, `country_class_wire_names_agree_with_serde:1516`,
`section_20_defaults:1528`, `job_index_iterates_in_external_id_byte_order:1570`,
`current_poll_seq_is_stored_plus_one:1607`.

`crates/errors/src/lib.rs`: `stage_wire_names_agree_with_serde:444`,
`fault_domain_wire_names_agree_with_serde:483`, `failure_kind_wire_names_agree_with_serde:506`,
`source_id_rejects_empty_and_control_bytes:583`, `accepted_source_id_round_trips_through_as_str:596`.

The wire-name tests use an exhaustiveness guard: a `match` over every variant that fails to compile
until a new variant is added to the assertion table, so a future variant cannot be added without a
wire name.

---

## 3. §30.2 coverage — Phase 1 vs later

§30.2 has 33 rows. **14 are fully Phase-1**, **6 have their Phase-1 half covered with the remainder
belonging to a later phase**, and **13 require a repository, an engine or a notifier and are
correctly not implemented here**.

### Fully covered in Phase 1 (14 rows, 13 entries)

| §30.2 row | Defends | Test |
|---|---|---|
| Repeat `JOB_REPOSTED` → distinct keys | INV-2 | `event_key_regression.rs:74`; end-to-end `diff.rs:937` |
| Pinned golden event-key vector | INV-2, §13.2.1 | `event_key_regression.rs:180/205/231` |
| 25-poll pure-429 run: never `FAILED`, `QUARANTINED` at the 20th | **INV-16**, §8.1 | `health.rs:1381` |
| `parsed_count = 0`, `last_job_count = 2`, `min_abs = 3` rejects | INV-4, §22 | `plausibility.rs:235` |
| Unresolvable location: relevant by default, not under `not_relevant` | §21.2 | `filter.rs:347`, `filter.rs:361` |
| `external_id` containing `0x1F` rejected as `NormalizeFailed` before any key is derived | INV-2, §13.2.1 | `model.rs:1289`, `model.rs:1311`, `normalize.rs:688` |
| `Internal Tools Engineer` does not match `intern` | §21.1 priority 6 | `normalize.rs:848`, `filter.rs:422` |
| `54 → 0` with `allow_zero = true` proceeds | INV-4 config | `plausibility.rs:224` |
| `FAILED` + transient + `probe_attempts = 0` backs off; `INITIALIZING` transient uses normal interval | §8.1, §10.3, §11.2 | `schedule.rs:615`, `schedule.rs:714` |
| `content_hash` unambiguous under `0x1F`/`0x1E`; all non-identity hashes full 52-char Base32 | schema, §21.1.1 | `shape.rs:668`; `assert_full_width_base32` across `:563/:593/:697` |
| Shape hash invariant to element order / array count, changes on a structural key-path change | INV-11, §18 | `shape.rs:708`, `:723`, `:743`, `:777`, `:805` |
| `JOB_REMOVED` retains marker + TTL + `last_seen_at`; `JOB_REPOSTED` clears marker + TTL | schema, §13.8, §17.3 | `diff.rs:687`, `diff.rs:640` |
| Unchanged present jobs across 100 polls → zero job-item writes | INV-12 corollary, §13.8 | `diff.rs:787` |

Thirteen entries, fourteen §30.2 rows: the row *"A job reactivating has its `ttl` attribute removed"*
is the same behaviour as the `JOB_REMOVED`/`JOB_REPOSTED` row and is asserted by the same test
(`diff.rs:640`, `clear_ttl: true`), so it shares that entry.

### Phase-1 half covered; remainder is a later phase (6)

| §30.2 row | Phase-1 half, covered | Remainder |
|---|---|---|
| Replaying the identical transition → same key **and one durable event** | same key — `event_key_regression.rs:123` | "one durable event" is the conditional `Put`; **Phase 3/4** |
| `54 → 0` and `54 → 7` **leave every stored job untouched** | both rejections — `plausibility.rs:216`, `:248` | "leave every stored job untouched" needs the engine short-circuit; **Phase 3** |
| Source fails during bootstrap, then succeeds → `INITIALIZING`, **no `NEW_JOB` storm** | the health half and the no-health-event guard — `health.rs:1271`, `:1317`, `:1358` | the diff-suppression half needs the engine's bootstrap branch; **Phase 3** |
| Sibling field added → poll succeeds, `API_CHANGED` emitted | shape moves, contract holds — `shape.rs:805`; the key exists — `event_key.rs:375` | emitting the event; **Phase 3** |
| Required field removed → immediate `SOURCE_FAILED`, state preserved | the classification — `shape.rs:906`; the transition — `health.rs:761/852` | the alert and state preservation; **Phase 3/5** |
| `filter_version` bump → one `FILTER_CHANGED`, zero individual `BECAME_RELEVANT` | suppression + reclassify routing — `diff.rs:851`, `:879`; the key — `event_key.rs:434` | emitting the one summary; **Phase 3** |

Two further rows are partly anticipated by the `outcome_for` mapping tests — "Telegram 429 → source
stays `HEALTHY`" (`health.rs:1630`) and "S3 PUT fails → poll still succeeds" (`health.rs:1643`) —
but both rows as written need a notifier and an archiver, so they are counted as later-phase below.

### Correctly not implemented in Phase 1 (13)

| §30.2 row | Phase |
|---|---|
| `ClientRequestToken`: different event-side parameters derive different tokens; canonically identical complete requests derive the same one | **Phase 4** (§32 Phase 4; §13.4.1) |
| Crash between chunk 1 and chunk 2 | Phase 3 (`FailPoint`, §30.3) |
| Crash inside a transaction | Phase 3 |
| Crash after all chunks, before the commit marker | Phase 3 |
| Crash after the event transaction, before notification claim | Phase 3 |
| Crash after Telegram accepts, before `sent` confirmation | Phase 3/5 |
| Telegram 429 → events open, source `HEALTHY`, `SUB#notification` degrades | Phase 5 |
| S3 PUT fails → poll succeeds, `SUB#archive` degrades | Phase 3 |
| Two concurrent `run_tick` → each source once, loser records `LeaseContention` | Phase 3 |
| Stale GSI1 result → conditional claim rejects it | Phase 4 |
| Bootstrap of a 300-job source → zero `NEW_JOB`, one summary; crash mid-bootstrap replays with still zero | Phase 3 |
| `BatchWriteItem` `UnprocessedItems` → loop until empty | Phase 4 |
| 40 simultaneous relevant events → one digest, all eventually `sent` | Phase 5 |

**Explicitly confirmed absent, as §32 Phase 1 requires:**

- **The `ClientRequestToken` complete-request fingerprint (§13.4.1) is NOT implemented.** It is
  Phase 4. `core::event_key` names it only in a doc comment (`event_key.rs:30-35`) that states the
  fingerprint is a *different, length-prefixed* encoding and is deliberately not implemented there.
  A workspace scan for `ClientRequestToken`, `client_request_token` and `request_fingerprint` returns
  only doc-comment mentions.
- **The crash-injection harness (`FailPoint`, §30.3) is NOT implemented.** It is Phase 3. A workspace
  scan for `FailPoint` returns zero hits.

---

## 4. Scope audit and dependency boundary

### 4.1 Phase-2/Phase-3 types must not exist in Phase-1 crates

```powershell
Select-String -Path crates\core\src\*.rs,crates\errors\src\*.rs -Pattern 'JobSource|HttpRequest|HttpResponse|CacheValidators|CommitMarker|TickCounters|TickStatus|AppliedCount|OpenEvent|EventRef|ClaimResult|CorrelationWindow|RollupDelta|ArchiveKey|pub struct Event'
```

Three hits, all benign:

| Hit | Assessment |
|---|---|
| `event_key.rs:100: pub struct EventKey(String);` | **False positive** on the `pub struct Event` alternative — `EventKey` is a §17.3 Phase-1 type. The Phase-3 `Event` data carrier does not exist. |
| `model.rs:35-36` (doc comment) | Prose stating that the full `Event` envelope is Phase 3 and that `HttpRequest`/`HttpResponse`/`CacheValidators` are Phase 2. No type declared. |

**No `JobSource` trait, no `HttpRequest`/`HttpResponse`/`CacheValidators`, and none of the eleven
Phase-3 model types exist anywhere in the workspace.** `crates/ports`, `crates/adapters` and
`crates/engine` remain empty doc-headed stubs (11, 12 and 11 lines respectively); `crates/infra`'s
six module files are 3 lines each.

### 4.2 Async / I/O / proptest / wall clock

```powershell
Select-String -Path crates\core\src\*.rs,crates\errors\src\*.rs -Pattern 'async |await|tokio|reqwest|aws_|aws-sdk|proptest|Utc::now'
```

Three hits, all benign:

| Hit | Assessment |
|---|---|
| `crates/core/src/lib.rs:8` — `//! **Must not know about:** ports, AWS, HTTP, tokio, async.` | Doc comment stating the constraint. |
| `crates/errors/src/lib.rs:8` — `//! **Must not know about:** AWS, HTTP, tokio.` | Doc comment stating the constraint. |
| `crates/errors/src/lib.rs:332` — `pub aws_error_code: Option<String>,` | **False positive** on `aws_`. This is a §9 `Detail` field, specified verbatim at spec line 414. It is a `String`, not an AWS SDK type. |

**Zero occurrences of `async`, `await`, `tokio`, `reqwest`, an AWS SDK, `proptest`, or `Utc::now()`
as code.** Phase 1 is fully synchronous and takes every current time as an explicit argument
(`diff(… now …)`, `health::next(… now …)`, `ScheduleInput::now`).

### 4.3 Dependency trees

```
$ cargo tree -p jobmon-errors --depth 1
jobmon-errors v0.1.0
└── serde v1.0.229
[dev-dependencies]
└── serde_json v1.0.151

$ cargo tree -p jobmon-core --depth 1
jobmon-core v0.1.0
├── chrono v0.4.45
├── data-encoding v2.11.1
├── jobmon-errors v0.1.0
├── serde v1.0.229
├── serde_json v1.0.151
└── sha2 v0.10.9
```

| Expected boundary | Actual | Verdict |
|---|---|---|
| `jobmon-errors`: `serde` only at runtime, `serde_json` dev-only | `--edges normal --depth 1` shows `serde` alone; `serde_json` appears only under `[dev-dependencies]` | **MATCHES** |
| `jobmon-core`: `jobmon-errors`, `serde`, `serde_json`, `chrono`, `sha2`, `data-encoding` | exactly those six, no more | **MATCHES** |
| No tokio / reqwest / AWS / proptest in either crate | none present at any depth | **MATCHES** |

### 4.4 chrono's `clock` feature

```
$ cargo tree --workspace -e normal -i chrono -f "{p} {f}"
chrono v0.4.45 alloc,serde,std
└── jobmon-core v0.1.0 …
```

**`chrono` resolves to `alloc,serde,std` — `clock` is NOT enabled**, workspace-wide, so feature
unification across all eight members does not turn it on either. The workspace manifest declares it
`default-features = false, features = ["std", "serde"]` (`Cargo.toml:62`). `Utc::now()` is therefore
not merely unused in core — it is not compiled in and cannot be called. **Nothing to report and
nothing was changed.**

### 4.5 §17.3 type-ownership audit

Every type §17.3 assigns to Phase 1 exists, in the assigned crate and module; nothing assigned to
Phase 2 or Phase 3 exists.

| Assigned to | Status |
|---|---|
| `jobmon-errors`: `SourceId`, `PipelineError`, `Stage`, `FaultDomain`, `FailureKind`, `Detail` | all present (`errors/src/lib.rs:54, 343, 147, 109, 202, 317`) |
| `jobmon-core::model`: `ExternalId`, `Source`, `SourceConfig`, `ScheduleState`, `HealthSnapshot`, `ContractState`, `SourceKind`, `Criticality`, `HealthState`, `JobState`, `BootstrapState`, `BootstrapMode`, `NotifyState`, `PollOutcome`, `EmploymentType`, `CountryClass`, `UnresolvedLocationPolicy`, `EndpointConfig`, `FilterOverrides`, `PlausibilityConfig`, `FilterConfig`, `AdapterContract`, `RawJob`, `NormalizedJob`, `Job`, `JobIndex`, `JobFacts`, `EventType`, `Transition`, `JobWrite`, `NonTransitionWrites`, `FilterReclassify` | all 32 present |
| `jobmon-core::event_key`: `EventKey` | present (`:100`) |
| `jobmon-core::schedule`: `ScheduleInput`, `ScheduleDecision` | present (`:106`, `:136`) |
| `jobmon-core::health`: `HealthEvent` | present (`:94`) |
| **Phase 2/3, must be absent:** `JobSource`, `HttpRequest`, `HttpResponse`, `CacheValidators`, `Event`, `CommitMarker`, `TickCounters`, `TickStatus`, `AppliedCount`, `OpenEvent`, `EventRef`, `ClaimResult`, `CorrelationWindow`, `RollupDelta`, `Channel`, `Message`, `ArchiveKey`, all I/O port traits | **all absent** |

The §17.3.1 canonical shapes were checked field by field against `model.rs` — `RawJob` (6 fields),
`NormalizedJob` (10, `remote` present), `Job` (19, `remote` deliberately absent), `JobFacts` (10),
`JobWrite`'s three variants, `Transition` (8), `NonTransitionWrites` (4), `ScheduleInput` (10),
`ScheduleDecision` (2), `HealthEvent` (2), `AdapterContract` (3). All match. The canonical `diff`
signature matches §17.3.1 exactly, including that `content_hash` is **not** a parameter
(`diff.rs:106-112`, computed internally at `:177` and `:224`).

`ExternalId` and `SourceId` are newtypes with private fields, a validating `new`, and a **hand-written**
`Deserialize` that routes through it (`model.rs:125`, tested at `model.rs:1311`). There is no other
construction path, so §30.2's "rejected **before any key is derived**" is enforced by the type system
rather than by call-site discipline.

---

## 5. Spec deviations discovered during implementation

### 5.1 Deviation — `CA` is omitted from the US two-letter raw-code table

**One deviation.**

| | |
|---|---|
| **Spec text** | §21.1, "Canonical Phase-1 location tables": *"US detection uses **all 50 state two-letter codes** plus `DC` under the raw-code rule…"* |
| **Code** | `crates/core/src/normalize.rs:226-231` — `US_RAW_CODES` holds **49** state codes plus `DC` plus `US`. `CA` is excluded and held separately as `AMBIGUOUS_CA` (`normalize.rs:261`), which is not a marker in either direction. |
| **Consulted at** | `has_non_canadian_marker` — `normalize.rs:491-499`; `carries_region_or_country_marker` — `normalize.rs:527-535` (where `CA` *is* consulted, but only to keep it out of the `city` field) |
| **Test** | `ambiguous_ca_code_decides_nothing_on_its_own` — `crates/core/src/normalize.rs:744` |

**Behavioural difference.** Under a literal reading of §21.1, `CA` is California, so it is a
non-Canadian marker; it would also suppress the Canadian-city rule, which fires *"only when the
string carries no other region/country marker."*

| Input | v1.2.2 literal | Implemented | Filter effect (default overrides) |
|---|---|---|---|
| `Toronto, CA` | `NOT_CA` | `CA` (Canada, via the city rule) | literal: fails §21.2 rule 3 and is **silently dropped**; implemented: relevant |
| `Vancouver, CA` | `NOT_CA` | `CA` | same |
| `San Francisco, CA` | `NOT_CA` | unresolved (`None`) | literal: fails rule 3; implemented: passes rule 4, delivered with §15.2's `⚠ location unparsed` marker |
| `San Francisco, CA, USA` | `NOT_CA` | `NOT_CA` | unchanged |
| `California`, `San Francisco, California` | `NOT_CA` | `NOT_CA` | unchanged — the full name is unambiguous and stays in `US_STATE_NAMES` |

**Why the code could not follow the spec exactly.** The two readings are mutually exclusive and
§21.1 asserts both: the sentence quoted above makes `CA` a US state code, while the same section's
Canada row and its "ambiguity resolves to Canada" rule exist so that a string carrying a Canadian
signal is not dropped. `Toronto, CA` is the collision — it is the standard ISO-3166 spelling for
Canada *and* the USPS spelling for California, and no single classification of the token satisfies
both rules. The implementation resolved it in the direction §2 and §36 mandate (a visible false
positive over an invisible false negative), and documented the reasoning at `normalize.rs:236-260`.

**Recommended action.** This is exactly the class §39.14 says an implementation prompt may not
settle: *"If a prompt needs to choose a durable encoding, a cross-phase data shape, a
health/scheduling transition, or a persistence mutation that this document does not state, stop
before code and update this document first."* The behaviour is under `filter_version = 1`
(`filter.rs:67`) and §21.3 makes the location tables part of that version, so the stored data is
self-describing either way. **Amend §21.1 to say "49 state two-letter codes plus `DC`; `CA` is
ambiguous and decides nothing", or reject the deviation and have the code follow the 50-code
sentence.** Either is a one-line change; leaving the two out of step is the option to avoid, because
the next person to read §21.1 will implement the 50-code version.

### 5.2 Under-determined cases the code had to complete — NOT deviations

These are recorded so Phase 2+ does not rediscover them as surprises. In each, the code follows every
case v1.2.2 actually decides; what it adds is a defined answer for a case the text does not reach.
None contradicts stated spec text, so none belongs in §5.1.

| Case | v1.2.2 | Code | Test |
|---|---|---|---|
| §8.1 writes `cf == 3` and `cf == 20`; what if a stored counter starts past the threshold? | not stated | `>=`, so it still quarantines — leaving it `FAILED` forever is the "silently forgotten" state INV-16 forbids. Identical for every reachable sequence. | `a_counter_past_the_threshold_still_quarantines` — `health.rs:1468`; rationale at `health.rs:233-241` |
| §8.1 has a `QUARANTINED \| not polled` row but no `DISABLED` poll row | not stated | `(Quarantined \| Disabled, _)` → no counter, timestamp or event moves | `disabled_is_never_polled_either` — `health.rs:1091`; rationale at `health.rs:394-401` |
| §11.2's hard-failure row assumes a `failure_kind` is present | not stated | falls back to the normal interval rather than inventing a backoff | `hard_without_a_failure_kind_falls_back_to_the_normal_interval` — `schedule.rs:806` |
| §21.1: `city` is the leading segment *"when that segment is not itself a region or country token"* — what about a compound segment such as `Remote - US`? | not stated | the segment is rejected when it **carries** a marker, not only when it equals one, so `Remote - US` yields no city | rationale at `normalize.rs:501-508`; `carries_region_or_country_marker` — `:527` |
| §13.8's removal predicate when `current_poll_seq == absent_since_poll` (the crash-retry case) | implied by "stable across a crash and retry", not stated as a comparison | `current_poll_seq <= absent_since_poll` → no second write, and it cannot underflow | `job_removed_fires_one_poll_after_the_marker` — `diff.rs:576`; rationale at `diff.rs:325-331` |
| §17.3.1 forbids saturating/wrapping `transition_seq`, but the canonical `diff` signature returns no `Result` | both stated; the combination is not resolved | panics with an explanatory message on `u64::MAX`, documented in `diff`'s `# Panics` section | `next_transition_seq` — `diff.rs:377`; rationale at `diff.rs:98-104` |

---

## 6. Ready for Phase 2

**Yes. Every Phase-1 acceptance criterion in §32 is satisfied**, with the single caveat in §5.1,
which is a spec-text question rather than a code defect and does not block Phase 2 (no Phase-2
adapter or fixture depends on the `CA` classification).

| §32 Phase-1 acceptance criterion | Status |
|---|---|
| `crates/errors` complete (§9, including `SourceId`) | **MET** — all six §9 types, all wire names asserted against serde |
| `crates/core`: `model`, `normalize`, `filter`, `shape`, `plausibility`, `diff`, `event_key`, `schedule`, `health` | **MET** — all nine modules populated (`core/src/lib.rs:13-21`) |
| All synchronous | **MET** — §4.2 |
| `core` owns `AdapterContract`, the §17.3.1 shapes, and the §13.2.1 encoder | **MET** — §4.5 |
| Non-identity hashes use §21.1.1, not the event-key truncation | **MET** — `FULL_SHA256_BASE32_LEN = 52` (`shape.rs:79`), asserted on every hash |
| Only the types §17.3 assigns to Phase 1 | **MET** — §4.5 |
| `chrono` without its wall-clock feature; every current time an explicit argument | **MET** — §4.4 |
| The event-key regression test written FIRST | **MET** — it is the workspace's only integration test, `crates/core/tests/event_key_regression.rs`, and its header records the §38 mandate |
| ~70 tests, sub-second, no network, no mocks, no async, no `proptest` | **MET** — 178 tests, 0.12 s, §1 and §4.2 |
| Do NOT build: I/O, trait impls, AWS types, `JobSource`, `HttpRequest`/`HttpResponse`/`CacheValidators`, any Phase-3 type | **HONOURED** — §4.1, §4.5 |

### Canonical interfaces Phase 2 may rely on

Stable, tested, and not to be redefined:

- **`jobmon_core::model::AdapterContract`** (`model.rs:800`) — `array_path: &'static str`,
  `required_paths: &'static [&'static str]`, `min_expected: usize`. **Already exists — Phase 2 must
  not redefine it** (§17.3). `Copy` and `const`-constructible, so an adapter declares its contract as
  a `const` next to its parser. It derives `Serialize` but deliberately **not** `Deserialize`: a
  contract is code, not configuration.
- **`jobmon_core::model::RawJob`** (`model.rs:822`) — exactly the six §17.3.1 fields. This is what an
  adapter's `parse` returns.
- **`jobmon_core::model::NormalizedJob`** (`model.rs:838`) — the ten §17.3.1 fields including
  `remote`. Produced by `jobmon_core::normalize::normalize(&RawJob)` (`normalize.rs:608`).
- **The public event-key encoder** — `jobmon_core::event_key`: `Component`, `encode_component`,
  `encode_components`, `digest_base32`, `event_key_from_components`, and the seven §13.2.3 typed
  constructors. Public by design: §13.2.1 requires the encoding to be implemented **exactly once** and
  reused. `COMPONENT_SEPARATOR`, `RECORD_SEPARATOR`, `EVENT_KEY_LEN` and `SYS_SCOPE` are exported as
  frozen constants.
- **`jobmon_core::shape`** — `validate_contract` (the §22 signature verbatim, explicit lifetime),
  `shape_hash`, `content_hash`, `body_hash`, `FULL_SHA256_BASE32_LEN`. Phase 2's fixture tests
  ("required-field-removed → `RequiredFieldMissing`", "empty array → contract `min_expected`
  violated", "shape change with valid contract → success + `ShapeChanged`") call these directly.
- **`jobmon_core::plausibility`** — `check`, `check_bootstrap`, both matching §22's canonical
  signatures.
- **`jobmon_errors`** — `PipelineError`, `Stage`, `FaultDomain`, `FailureKind`, `Detail`, `SourceId`.
  Adapters return `Result<_, PipelineError>`.

### Still Phase 2 — create these, they do not exist yet

- **The `JobSource` trait** — §18's five methods, in **`jobmon-adapters`**, not in `core`. It is not
  an I/O port and does not belong in `ports`.
- **`HttpRequest`, `HttpResponse`, `CacheValidators`** — added to **`jobmon_core::model`** (not to
  `adapters`), when `build_request` first needs them (§17.3).

Both were verified absent (§4.1).

### Still Phase 3+ — do not implement early

- **`Event`** (the full §16.2 envelope) — Phase 3. Phase 1's authoritative transition payload is
  `Transition` + `JobWrite`, and §17.3.1 forbids inventing an incomplete event payload before then.
- **The eleven Phase-3 model types** and **all I/O port traits** — Phase 3.
- **`ClientRequestToken`'s complete-request fingerprint (§13.4.1)** and the **`FailPoint`
  crash-injection harness (§30.3)** — Phase 4 and Phase 3 respectively. Both confirmed absent (§3).

### Two carried-forward items from the §32 Phase 0 record, still open

Neither is a Phase-1 defect; both are scheduled, and this report restates them so they are not lost:

1. **`bin/admin` will need a direct `jobmon-adapters` edge** (§19 requires `admin add-source` to
   validate `endpoint_config` and perform a live probe *and parse*). Deferred to Phase 6 as a
   deliberate amendment to the §17 table.
2. **`panic = "abort"` is deliberately absent from `[profile.release]`** — a Phase 6 decision, so
   that per-source processing can be wrapped in `catch_unwind`.

A third Phase-0 note is now actionable: *"Consider scoping `clippy::unwrap_used` / `expect_used` to
non-test builds in Phase 1."* It was **not** imposed. Phase-1 non-test code uses `expect` in exactly
one place — `next_transition_seq` (`diff.rs:378`), which is the documented §17.3.1 overflow refusal —
so the lint would pass today if the workspace chose to add it. That is a manifest change and is
therefore out of scope for this audit session.
