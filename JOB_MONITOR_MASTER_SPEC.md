# JOB_MONITOR_MASTER_SPEC.md

## 0. Document metadata

| Field | Value |
|---|---|
| **Project** | Job Monitor — reliable monitoring platform for job sources |
| **Document** | Canonical engineering specification, decision log, implementation roadmap, operational handoff |
| **Version** | `v1.0-architecture-frozen` |
| **Status** | Architecture FROZEN for V1. Implementation NOT STARTED. |
| **Last architecture review** | 2026-08-16 (fifth review; four material issues found and resolved) |
| **External facts verified** | 2026-08-16 — see §28 and §39.4 for the reverification protocol |
| **Current project phase** | Phase 0 (not begun) |
| **Next recommended action** | Create the Cargo workspace, pin the Rust toolchain, configure CI (`fmt` / `clippy -D warnings` / `test`) |
| **Implementation language** | Rust (mandatory, frozen) |
| **Target platform** | AWS: EventBridge Scheduler + Lambda (ARM64) + DynamoDB + S3 + SSM + CloudWatch; Telegram + healthchecks.io |

---

## 1. Executive summary

**What this is.** A small, continuously running service that watches the careers APIs of many companies and alerts the owner's phone within minutes when a Canadian internship or co-op posting appears. It runs entirely in AWS, independently of any personal machine.

**Why it exists.** The owner is a Canadian university student applying for co-op and internship placements. Canadian postings drop unpredictably and are frequently missed. Manual refreshing of career pages is unreliable, does not scale past a handful of companies, and cannot run overnight. Third-party aggregators cover large US-headquartered employers but systematically miss Canadian-scoped requisitions, Canadian startups, and AI labs.

**What problem it actually solves.** The naive version of this project is a scraper. That is not what this is. The hard problem is not *fetching* — it is knowing, at every moment, whether each monitor is genuinely working. A scraper that silently breaks is worse than no scraper, because the owner stops checking manually while receiving no alerts. Therefore the system is designed first as a **monitoring platform for job sources**, in which the fetching is one subsystem among several, and in which *silence is never ambiguous*.

**What success looks like.**

- A relevant posting appearing at 10:07 produces a phone notification at approximately 10:07.
- Any broken source is identified — by name, pipeline stage, fault domain, and probable cause — within roughly one polling interval.
- The owner can distinguish "the company changed their API" from "our parser is wrong" from "our AWS infrastructure is broken" from "Telegram is down," without reading logs.
- Adding a company that uses an already-supported ATS takes one to two minutes and requires no deployment.
- Total cost stays inside AWS free allowances, with only trivial S3 charges.
- The system runs for months without attention, and the owner *knows* it is running because it says so.

---

## 2. User goals and non-negotiable priorities

**Priority ordering. This ordering resolves all design conflicts. When two requirements conflict, the lower number wins.**

1. **Never silently miss a relevant posting.**
2. **Detect broken sources quickly.**
3. **Correctly identify what broke** (upstream / adapter / infrastructure / notification / archive).
4. **Deliver relevant job alerts to the phone immediately.**
5. **Remain easy to extend** with many companies and startups.
6. **Avoid noisy or duplicate alerts.**
7. **Stay extremely cheap** (effectively $0/month).
8. **Remain independent of the owner's laptop.**

**Derived rules from this ordering:**

- A rare duplicate notification is acceptable. A missed notification is not. → at-least-once delivery.
- An occasional false health alert is acceptable. A dead monitor unnoticed for an hour is not. → aggressive failure detection, tight watchdog grace.
- Correctness beats cost. Cost only breaks ties between otherwise equivalent designs.
- Noise reduction (6) is subordinate to detection (2) — but noise that causes the owner to *ignore* alerts is itself a violation of (1), which is why `QUARANTINED` and alert throttling exist.

**Growth expectation.** The owner will add companies continuously throughout the year: large technology companies, Canadian companies, startups, AI labs, and obscure employers with unusual careers systems. Expected trajectory: ~4 sources at first integration, ~20 within months, plausibly 100–300 within a year.

---

## 3. Scope

### In scope for V1

- Central 1-minute scheduler; per-source data-driven polling intervals.
- Adapter families for Greenhouse, Lever, Ashby (Workday and custom as needed).
- Normalization to a common job model; Canada + internship/co-op relevance filtering.
- Adapter contract validation, shape-change telemetry, plausibility gating.
- Deterministic event identity; atomic transition/event persistence.
- Immediate Telegram job alerts; separate health channel.
- Source health state machine with fast failure detection and precise diagnostics.
- System-level correlation for infrastructure-wide failures.
- External dead-man's-switch with success / explicit-fail / silence semantics.
- S3 diagnostic raw payload archive with a strict write policy.
- DynamoDB telemetry sufficient to answer §24 analysis questions later.
- Admin CLI for source registration.

### Explicitly NOT in V1

See §35 for full deferral rationale and triggers. Summary: no time-of-day or seasonal polling multipliers, no dashboard, no browser fetcher, no Push source implementation (seam only), no packed job index, no cross-source deduplication, no ML ranking, no automated application submission, no Workday adapter at first integration.

---

## 4. Frozen architectural decisions

**These are frozen. A future AI must not casually reopen them.** Each row states what would legitimately justify reopening.

| # | Decision | Why | Rejected alternatives | Reopen if… |
|---|---|---|---|---|
| D1 | **Monitor structured job state, not HTML** | Career pages are SPAs; DOM diffs produce daily false positives from session tokens, CSS hashes, and relative timestamps. Every major ATS exposes a JSON endpoint. | HTML/DOM diffing; headless browser rendering | A high-value target has no JSON endpoint at all (→ see D14 Push seam) |
| D2 | **Rust** | Owner requirement. Also: strong type modelling of the error taxonomy, cheap Lambda footprint, fast cold start. | TypeScript, Python | Never — owner requirement |
| D3 | **AWS over Cloudflare Workers** | Rust on Lambda is GA (Nov 2025) with SLA backing and a 1.0 runtime crate; native tokio, reqwest, rustls, and standard Linux-target Rust crates. Cloudflare's workers-rs is 0.x and runs in the wasm32-unknown-unknown environment without Tokio, which imposes substantially greater crate/runtime constraints and a less natural Rust development/testing workflow. Cloudflare also costs about $5/mo where AWS is effectively free at this workload. | Cloudflare Workers + D1; Fly.io; Oracle Cloud Always Free; Hetzner VPS; Raspberry Pi | AWS materially changes free-tier terms, or a hard requirement emerges that Lambda cannot serve |
| D4 | **AWS Lambda (ARM64) as the compute unit** | Free-tier headroom of ~25× at V1 scale; 12–22 ms cold start is irrelevant at 1-minute cadence but means nothing fights you; no OS to patch. | Always-on VPS with a tokio `interval` loop (genuinely simpler, but ~$10/mo and owner-operated OS) | Sustained duration exceeds ~40% of the 400,000 GB-s free allowance |
| D5 | **EventBridge Scheduler, 1-minute cadence** | Removes quantization lag between `next_check_at` and actual poll; enables sub-5-minute manual overrides without redeploying. 43,200 invocations/month vs a 14M free allowance. | 5-minute cadence (adds up to 2.5 min median lag); per-company cron (see D6) | Never for V1 |
| D6 | **One central scheduler + work queue, not per-company cron** | Scheduling lives in the database as data, so intervals, backoff, probes, jitter, and overrides change without deployment. A per-company cron design has nowhere to put `next_check_at` and no observer for a dead monitor. | N independent schedules | Never |
| D7 | **DynamoDB, single table** | Four of five access patterns are natively key-value; the fifth (due-source queue) is a standard sparse-GSI idiom. Conditional writes give leases, idempotency, and CAS transitions. No connection pooling. **No VPC** — this alone avoids a ~$32/month NAT Gateway. | Aurora DSQL (no foreign keys, PG16 subset, OCC retries, IAM token refresh); Aurora Serverless (free tier tied to the 6-month auto-closing Free Plan); RDS; Postgres via Neon/Supabase | Consumed RCU consistently exceeds free allowance *after* the §29 packed-index mitigation |
| D8 | **S3 for raw diagnostic payloads** | 10× cheaper per GB than DynamoDB; no 400 KB item cap (responses reach 300 KB); lifecycle rules give free expiry. | Raw payloads in DynamoDB; raw payloads in CloudWatch Logs (a $4+/month footgun) | Never |
| D9 | **Telegram as the primary notification channel** | It is a **direct message**, not a server channel — Discord server channels default to mentions-only on mobile and fail *silently*, which violates priority (1). Inline keyboard buttons give a real one-tap "Apply" target. One HTTP POST, no hosted process. | Discord webhook; email; ntfy.sh; Pushover | Telegram becomes unreliable in the owner's region, or bans bot use of this shape |
| D10 | **Adapter families, not per-company adapters** | Twenty Greenhouse companies share one contract. Per-company adapters make adding a company a code change, violating priority (5). | One adapter per company | Never |
| D11 | **Pure, synchronous domain core** | Normalization, filtering, diffing, plausibility, scheduling, and health rules need no I/O. Making them sync means `cargo test` covers all business logic with no runtime, no mocks, no network — and the `Send`/`!Send` question never touches them. | Async-everywhere core | Never |
| D12 | **Notification state belongs to EVENTS, not jobs** | One job legitimately produces multiple alert-worthy transitions over its lifetime (`NEW_JOB`, then `BECAME_RELEVANT`, then `JOB_REPOSTED`). A single `job.notify_state` flag cannot represent that and would drop the later ones. | `job.notify_state` | Never |
| D13 | **At-least-once notification delivery** | Priority (1) beats priority (6). A duplicate alert costs a glance; a dropped alert costs an internship. | At-most-once; exactly-once (unachievable across a third-party API) | Never |
| D14 | **`SourceKind::{Pull, Push}` seam, Push unimplemented** | If a target blocks datacenter ASNs or requires browser execution, an external fetcher on a residential IP can POST the raw body into the pipeline from `Decode` onward. Costs one enum discriminant today. | Building Push now; VPN/IP rotation as a default strategy (rejected: consumer VPN and cloud exits are *worse*-reputation datacenter ASNs than a plain AWS IP, and rotating location is counterproductive when filtering for Canadian postings) | A source actually blocks us |
| D15 | **External dead-man's-switch on an independent vendor** | An internal monitor cannot report its own death. Must not share a failure domain with AWS *or* with Telegram. | Internal-only health; healthchecks alerting only via Telegram | Never |
| D16 | **Atomic transition/event pairs (§13)** | Job state must never advance without its event. Discovered by review 2026-08-16. | Sequential job-writes-then-event-writes with a commit marker (**incorrect** — loses events on crash) | Never |
| D17 | **No time-of-day or seasonal polling multipliers in V1** | No evidence yet about when companies publish. More importantly: *adaptive polling in V1 would corrupt the very dataset used to design adaptive polling*, because `first_seen_at` is censored by the polling interval. Uniform intervals produce an unbiased hour-of-week distribution. | Business-hours multipliers; seasonal boosts | ≥3 months of `first_seen_at` data exists and shows a real distribution |

---

## 5. Architecture diagram

```
                         ┌───────────────────────────────────────────────┐
                         │  FAULT DOMAIN: Upstream (not our code)        │
                         │                                               │
                         │   Greenhouse   Lever   Ashby   Workday        │
                         │   in-house careers APIs                       │
                         └───────────────────▲───────────────────────────┘
                                             │ HTTPS GET (ETag / If-Modified-Since)
                                             │ per-request timeout 10 s
                                             │
  ┌──────────────────┐                       │
  │ EventBridge      │  every 1 minute       │
  │ Scheduler        │  retries = 0 (!)      │
  │ maxEventAge=60s  │                       │
  └────────┬─────────┘                       │
           │ invoke                          │
           ▼                                 │
╔══════════════════════════════════════════════════════════════════════════╗
║  FAULT DOMAIN: Infra + Adapter        Lambda (Rust, ARM64, 512 MB, 60 s) ║
║                                                                          ║
║  ┌────────────────────────────────────────────────────────────────────┐ ║
║  │ 0. NOTIFICATION RECOVERY (runs FIRST — delivery beats discovery)   │ ║
║  │    Query GSI2 NOTIFY#OPEN → resend stale/unsent events             │ ║
║  └────────────────────────────────────────────────────────────────────┘ ║
║                                    │                                     ║
║  ┌─────────────────────────────────▼──────────────────────────────────┐ ║
║  │ 1. DISCOVER   Query GSI1  PK="DUE", SK <= now, Limit 30            │ ║
║  │               (eventually consistent — ADVISORY HINT ONLY)         │ ║
║  └─────────────────────────────────┬──────────────────────────────────┘ ║
║                                    │                                     ║
║  ┌─────────────────────────────────▼──────────────────────────────────┐ ║
║  │ 2. CLAIM      Conditional UpdateItem on SRC#<id>/META  ← AUTHORITY │ ║
║  │               COND: next_check_at <= now                           │ ║
║  │                 AND (lease_until absent OR lease_until < now)      │ ║
║  │               SET lease_until = now+90s, lease_owner = <inv_id>    │ ║
║  └─────────────────────────────────┬──────────────────────────────────┘ ║
║                                    │  buffer_unordered(8)                ║
║  ┌─────────────────────────────────▼──────────────────────────────────┐ ║
║  │ 3. FETCH      reqwest + rustls · 10 s timeout · conditional GET    │ ║
║  │               304 → no body, no diff, no S3, success               │ ║
║  ├────────────────────────────────────────────────────────────────────┤ ║
║  │ 4. DECODE     content-type check, gzip, UTF-8, JSON parse          │ ║
║  ├────────────────────────────────────────────────────────────────────┤ ║
║  │ 5. ADAPTER    parse(&[u8]) -> Vec<RawJob>          (pure, sync)    │ ║
║  │    CONTRACT   required_paths present? array_path present?          │ ║
║  │               shape_hash changed but contract valid → API_CHANGED  │ ║
║  │                                                     (NOT a failure)│ ║
║  ├────────────────────────────────────────────────────────────────────┤ ║
║  │ 6. NORMALIZE  RawJob -> NormalizedJob (country, region, emp type)  │ ║
║  ├────────────────────────────────────────────────────────────────────┤ ║
║  │ 7. PLAUSIBILITY  count vs last_job_count, per-source thresholds    │ ║
║  │                  FAIL → PRESERVE canonical state, snapshot, alert  │ ║
║  ├────────────────────────────────────────────────────────────────────┤ ║
║  │ 8. FILTER     Canada + internship/co-op predicate → relevant flag  │ ║
║  ├────────────────────────────────────────────────────────────────────┤ ║
║  │ 9. DIFF       load_job_index(src) vs fetched                       │ ║
║  │               ≤1 transition per job per poll (strict precedence)   │ ║
║  └─────────────────────────────────┬──────────────────────────────────┘ ║
║                                    │                                     ║
║  ┌─────────────────────────────────▼──────────────────────────────────┐ ║
║  │ 10. PERSIST — ATOMIC TRANSITION/EVENT PAIRS  (§13)                 │ ║
║  │     chunks of 25 transitions = 50 transaction actions              │ ║
║  │     ┌──────────────────────────────────────────────────────┐       │ ║
║  │     │ TransactWriteItems:                                  │       │ ║
║  │     │   Update JOB#x  COND transition_seq == k             │       │ ║
║  │     │   Put    EVT#<deterministic key>                     │       │ ║
║  │     │          COND attribute_not_exists(SK)               │       │ ║
║  │     │          notify_state = "unsent"  (GSI2 attrs set)   │       │ ║
║  │     └──────────────────────────────────────────────────────┘       │ ║
║  │     then non-transition writes (absence markers, AGG rollup)       │ ║
║  │     then COMMIT MARKER: TransactWrite{ META update, POLL record }  │ ║
║  └────────┬──────────────────────┬─────────────────────┬──────────────┘ ║
║           │                      │                     │                 ║
║  ┌────────▼─────────┐   ┌────────▼────────┐   ┌────────▼──────────────┐ ║
║  │ 11. NOTIFY       │   │ 12. ARCHIVE     │   │ 13. TICK CLOSE        │ ║
║  │  claim → send    │   │  raw_latest if  │   │  correlation window   │ ║
║  │  → confirm       │   │  body changed   │   │  UpdateItem ADD +     │ ║
║  │  token bucket    │   │  + ≥6 h elapsed │   │  ReturnValues ALL_NEW │ ║
║  │  health first    │   │  snapshot on    │   │  → SYSTEM_DEGRADED?   │ ║
║  │  cap 8/src/tick  │   │  diagnostics    │   │                       │ ║
║  └────────┬─────────┘   └────────┬────────┘   └────────┬──────────────┘ ║
╚═══════════╪══════════════════════╪═════════════════════╪════════════════╝
            │                      │                     │
 ┌──────────▼──────────┐  ┌────────▼───────┐  ┌──────────▼───────────────┐
 │ FAULT DOMAIN:Notify │  │ S3             │  │ healthchecks.io          │
 │  Telegram Bot API   │  │  raw_latest/   │  │  period 1 m, grace 5 m   │
 │  ├ job DM  (1/s)    │  │  snapshot/     │  │  ping /      = healthy   │
 │  └ health grp(20/m) │  │  90 d lifecycle│  │  ping /fail  = infra bad │
 │        │            │  │  versioning OFF│  │  silence     = dead      │
 │        ▼            │  └────────────────┘  │        │                 │
 │     PHONE           │                       │        ▼                 │
 └─────────────────────┘  ┌────────────────┐  │  EMAIL (independent      │
                          │ CloudWatch     │  │  transport, not Telegram)│
                          │ structured JSON│  └──────────────────────────┘
                          │ 14 d retention │
                          │ NEVER raw bodies
                          └────────────────┘

           DynamoDB (single table `jobmon`) — see §16
           SRC#<id>/META · JOB#<ext> · EVT#<key> · POLL# · AGG#
           SYS#CORR/WIN# · SYS#HEALTH/SUB#
           GSI1 = DUE (advisory)   GSI2 = NOTIFY#OPEN (sparse)
```

**Fault-domain legend.** `Upstream` = the company's API, not our code. `Adapter` = our parsing and contract assumptions. `Infra` = our AWS resources. `Notify` = Telegram delivery. `Archive` = S3. These four domains determine alert routing and wording; the pipeline *stage* determines diagnosis.

---

## 6. Core reliability invariants

**These are the contract. Every test in §30 exists to defend one of them. A future AI that discovers a violation must surface it before coding around it.**

| ID | Invariant |
|---|---|
| **INV-1** | No event-producing canonical job transition may become durable unless the corresponding logical event is durable in the same atomic operation. |
| **INV-2** | Retrying the same logical transition must never create a second logical event. Event identity is deterministic. |
| **INV-3** | Every durable notify-worthy event is discoverable by the notification recovery sweeper from the instant it exists, without any further write. |
| **INV-4** | A suspicious or implausible poll must never mutate canonical known-good job state. |
| **INV-5** | Notification failure must never delete, skip, or advance past an event. Throttling delays delivery; it never destroys it. |
| **INV-6** | Source health and notification health are independent. A Telegram outage must not mark a source unhealthy, and a source failure must not mark notifications degraded. |
| **INV-7** | The due-source GSI is advisory. Only the conditional base-table update on `SRC#<id>/META` may authorize work on a source. |
| **INV-8** | Total internal system death must be detectable externally, by a vendor that shares no failure domain with AWS or Telegram. |
| **INV-9** | A Lambda that executes but cannot reach DynamoDB must not make the external watchdog appear healthy. |
| **INV-10** | Bootstrapping a source must never produce a notification storm, and must never be mistaken for genuine new-job detection. |
| **INV-11** | A changed response shape is not a failure. Only a violated adapter contract, a parse error, or a plausibility failure is a failure. |
| **INV-12** | `next_check_at` may only advance after all of that poll's transitions, events, and non-transition writes are durable. |
| **INV-13** | At most one event may be derived per job per poll. |
| **INV-14** | Raw response bodies are never written to CloudWatch Logs. |
| **INV-15** | Changing the owner's relevance filter must not fabricate `BECAME_RELEVANT` events for pre-existing jobs. |
| **INV-16** | A permanently broken source must eventually stop re-alerting, but must never become silently forgotten. |

---

## 7. Source lifecycle

```
register ──▶ INITIALIZING ──▶ bootstrap ──▶ HEALTHY ⇄ DEGRADED ──▶ FAILED ──▶ QUARANTINED
                                              ▲                                    │
                                              └────────── manual re-enable ────────┘

                                   DISABLED  ←── manual, from any state
```

| Stage | What happens |
|---|---|
| **register** | `admin add-source` writes `SRC#<id>/META` with `health_state = INITIALIZING`, `next_check_at = now`, `poll_seq = 0`, `bootstrap_state = pending`. No code change if the adapter exists. |
| **initialize** | First tick claims it. Full fetch, parse, contract validation, normalization. Plausibility is **skipped** (no prior count to compare against); instead `min_expected` from the adapter contract applies. |
| **bootstrap** | See §13.6. Baseline `BatchWriteItem` of all current jobs (no per-job events, `transition_seq = 1`, `bootstrapped = true`), one `SOURCE_BOOTSTRAPPED` summary event, then `bootstrap_state = complete` in the commit marker. Transitions to `HEALTHY`. |
| **healthy operation** | Normal polling per §11. |
| **degraded / failed / recovery / quarantine / disable** | See §8. |

---

## 8. Source health state machine

```
                      first successful poll + bootstrap complete
   INITIALIZING ──────────────────────────────────────────────▶ HEALTHY
        │                                                       │   ▲
        │ 3 consecutive failures                soft failure    │   │ success
        │ during init                          (429, 1× transient)  │
        ▼                                                       ▼   │
     FAILED ◀───────────────────────────────────────────── DEGRADED │
        │  ▲        2nd consecutive failure, OR any hard failure    │
        │  │                                                        │
        │  └────────────────── success ───────────────────────────┐ │
        │                                                          │ │
        │ 20 consecutive failures                                  ▼ ▼
        ▼                                                        HEALTHY
   QUARANTINED ──────── manual `admin enable-source` ──────────▶ INITIALIZING

   DISABLED  ←──── manual `admin disable-source`, from any state ────
```

| State | Entry trigger | Polling | Alert on entry | Re-alert |
|---|---|---|---|---|
| `INITIALIZING` | registration; or manual re-enable from `QUARANTINED` | normal interval | none | — |
| `HEALTHY` | successful poll from any non-disabled state | normal interval + jitter | `SOURCE_RECOVERED` if previous state was `DEGRADED` or `FAILED`, with outage duration and returned job count | — |
| `DEGRADED` | 429, or first transient failure (timeout, 5xx, connect/DNS/TLS) | **priority probe** (§10) | `SOURCE_DEGRADED`, health channel, low severity | suppressed while in state |
| `FAILED` | 2nd consecutive failure of any kind, OR **any hard failure on first observation** (see §10 table) | exponential backoff, cap 2 h | `SOURCE_FAILED`, health channel, **immediate**, full diagnostic body | at most every 6 h while stuck |
| `QUARANTINED` | 20 consecutive failures | **stopped** | one final message, then silence | never — appears in daily digest instead (INV-16) |
| `DISABLED` | manual | stopped | none | never |

**`SOURCE_FAILED` alert body** — rendered from `PipelineError.detail` + META, one template with optional fields:

```
🚨 MICROSOFT SOURCE FAILED
Stage:            PARSE
Fault domain:     ADAPTER
Kind:             REQUIRED_FIELD_MISSING  (missing: "location.name")
Last success:     10:00:12  (2 attempts ago)
First failure:    10:05:07
Latest failure:   10:06:04
HTTP:             200  application/json  142 KB
Job count:        previous 53 → parsed 0
Shape changed:    YES  (a1f3… → 9c72…)
Adapter:          microsoft v3
Likely cause:     upstream schema change; adapter contract no longer holds
Snapshot:         s3://…/snapshot/microsoft/2026-08-16T10-06-04-schema.json.gz
Blind spot:       5 min (criticality=critical)
Next retry:       10:36  (backoff)
```

**`QUARANTINED` message must contain** (INV-16): source id and company, reason, last success timestamp, total failure duration, attempt count, likely repair required, and the exact command to re-enable (`admin enable-source --id <id>`). Quarantined sources are then listed by name in every daily digest until resolved.

---

## 9. Failure / error taxonomy

Three orthogonal axes. **Stage answers *where*. Domain answers *whose fault*. Kind answers *what*. Alert policy keys on `(domain, kind)`; the human-facing message leads with stage.** A flat stage list is insufficient because the same stage can belong to different domains — a `Decode` failure is `Upstream` if they returned HTML, `Adapter` if we sent the wrong `Accept` header.

```rust
pub struct PipelineError {
    pub stage:   Stage,
    pub domain:  FaultDomain,
    pub kind:    FailureKind,
    pub detail:  Detail,          // structured facts rendered into the alert
    pub source_id: Option<SourceId>,
}

pub enum FaultDomain { Upstream, Adapter, Infra, Notify, Archive }

pub enum Stage {
    Scheduler, Claim, Connect, Http, Decode, Parse, Schema,
    Normalize, Plausibility, Persist, Archive, Notify, Heartbeat,
}

pub enum FailureKind {
    // Upstream
    NotFound, Gone, Forbidden, BotChallenge, AuthRequired, RateLimited,
    ServerError, Timeout, ConnectFailed, DnsFailed, TlsError,
    WrongMediaType, MalformedBody, EmptyBody,
    // Adapter
    ParseFailed, RequiredFieldMissing, ArrayPathMissing,
    NormalizeFailed, PlausibilityFailed, ShapeChanged,
    // Infra
    DbThrottled, DbConditionalCheckFailed, DbAccessDenied, DbFailed,
    LeaseContention, TickTimeout, ConfigInvalid, SecretUnavailable,
    // Notify
    NotifySendFailed, NotifyRateLimited, NotifyAuthFailed,
    // Archive
    ArchivePutFailed,
}

pub struct Detail {
    pub http_status:     Option<u16>,
    pub content_type:    Option<String>,
    pub response_bytes:  Option<usize>,
    pub retry_after:     Option<Duration>,
    pub prev_job_count:  Option<usize>,
    pub parsed_count:    Option<usize>,
    pub shape_hash_prev: Option<String>,
    pub shape_hash_new:  Option<String>,
    pub missing_paths:   Vec<String>,
    pub adapter:         Option<(&'static str, u32)>,
    pub snapshot_key:    Option<String>,
    pub aws_error_code:  Option<String>,
    pub message:         String,
}
```

**Stages dropped from the original 17-stage list and why:** `REQUEST_BUILD` → validated at registration time, not a runtime failure (`ConfigInvalid` covers it). `CONTENT_TYPE` → merged into `Decode`. `FILTER` and `DIFF` → pure predicates over already-validated data; they cannot fail at runtime.

**Notes on specific kinds.**

- `ShapeChanged` is **not a failure** (INV-11). It is emitted as `API_CHANGED` telemetry and the poll succeeds.
- `LeaseContention` is **not an error**. It means another invocation legitimately owns the source. Log at debug, do not count as a failure.
- `DbConditionalCheckFailed` on a transition means *already applied by a prior attempt*. It is a **success signal**, not a failure (§13.5).
- `ArchivePutFailed` never invalidates a poll (INV-6 corollary). It degrades the archive subsystem only.

---

## 10. Failure detection and escalation SLA

### 10.1 The two latencies are different — this distinction is mandatory

```
   T0                        T1                          T2
   │                         │                           │
   upstream                  first FAILED poll           confirmed / alerted
   actually breaks           (first OBSERVATION)         (ESCALATION)
   │                         │                           │
   └───── BLIND SPOT ────────┘                           │
   bounded ONLY by the        └──── CONFIRMATION ────────┘
   full polling interval           bounded by probe timing
```

**A fast retry improves T1→T2. It does absolutely nothing for T0→T1.**

A source polled every 30 minutes whose API breaks one second after a successful poll cannot be observed as broken for ~30 minutes, no matter what the scheduler does. There is no cheap probe that detects a schema break or a parser incompatibility — only a full poll validates the contract. **A future AI must not claim otherwise.**

Therefore: **blind spot = full polling interval.** This is a property of configuration, not of code.

### 10.2 `criticality` — making the tradeoff visible

Rather than a second free-floating knob that can silently contradict the interval, criticality is a validated ceiling on the interval:

| `criticality` | Max `base_interval` | Blind spot | Intended for |
|---|---|---|---|
| `Critical` | 300 s (5 min) | ≤ 5 min | Microsoft, Shopify, top-choice employers |
| `Standard` | 600 s (10 min) | ≤ 10 min | default for most sources |
| `Background` | 1800 s (30 min) | ≤ 30 min — **consciously accepted** | fragile/restrictive APIs, low-priority companies |

**Validation rule, enforced by `admin add-source` and re-checked at tick start:**

```
effective_interval = interval_override.unwrap_or(base_interval)
assert!(effective_interval <= criticality.max_interval_secs())
```

`admin add-source --criticality critical --interval 30m` is **rejected** with an explanatory error. If a source must be polled slowly, the operator must explicitly downgrade its criticality — which makes the reliability tradeoff a deliberate, recorded decision rather than an accident.

`failure_detection_sla_secs` is **derived, not stored**: `criticality.max_interval_secs()`. It appears in `SOURCE_FAILED` messages as "Blind spot: N min" so the owner is reminded which sources have weak detection.

### 10.3 Probe timing under a 1-minute scheduler

EventBridge Scheduler has minute-level resolution. The spec therefore does **not** promise "retry in exactly 45 seconds."

```
transient failure observed
  → health = DEGRADED, probe_attempts += 1
  → next_check_at = now + 30 s          (internal value; guarantees it lands
                                         on the NEXT tick, not the current one)
  → actual re-poll occurs at the next scheduler opportunity
  → typical ≈ 30–60 s later; worst case ≈ 90 s
```

**Published SLA wording: "confirmation within 1–2 scheduler ticks (typically ≤ 60 s, worst case ≤ ~90 s)."** After `probe_attempts` reaches 2, the source drops to exponential backoff and stops being probed.

### 10.4 Per-class SLA table

| Failure class | First observation (T0→T1) | Alert on 1st failure? | Probe? | Backoff after | Health | Max notification delay (from T1) |
|---|---|---|---|---|---|---|
| Contract break (`RequiredFieldMissing`, `ArrayPathMissing`) | ≤ blind spot | **YES** | no | 30 m | → `FAILED` | ≤ 1 tick (~60 s) |
| `ParseFailed`, `NormalizeFailed` | ≤ blind spot | **YES** | no | 30 m | → `FAILED` | ≤ 1 tick |
| `NotFound` (404), `Gone` (410) | ≤ blind spot | **YES** | no | 1 h | → `FAILED` | ≤ 1 tick |
| `Forbidden`/`BotChallenge` (403), `AuthRequired` (401) | ≤ blind spot | **YES** | no | 30 m | → `FAILED` | ≤ 1 tick |
| `WrongMediaType`, `MalformedBody`, `EmptyBody` | ≤ blind spot | **YES** | no | 15 m | → `FAILED` | ≤ 1 tick |
| `PlausibilityFailed` | ≤ blind spot | **YES** | no | normal interval | → `FAILED` | ≤ 1 tick |
| `Timeout`, `ConnectFailed`, `DnsFailed`, `TlsError`, `ServerError` (5xx) | ≤ blind spot | no | **yes ×2** | 2× per failure, cap 2 h | → `DEGRADED`, then `FAILED` | ≤ 2 ticks (~90 s) |
| `RateLimited` (429) | ≤ blind spot | no (DEGRADED notice only) | **no** — honour `Retry-After` | `Retry-After`, floor 60 s | → `DEGRADED` only | ≤ 1 tick, low severity |
| `DbAccessDenied`, `DbFailed`, `TickTimeout`, `ConfigInvalid` | **≤ 1 min** (every tick touches DynamoDB) | **YES, SYSTEM** | n/a | next tick | system-level | ≤ 1 tick |
| Telegram broken | ≤ 1 min | after 3 pending events or 5 min | n/a | next tick | `NOTIFICATION_DEGRADED` + healthchecks `/fail` | ≤ 5 min |
| Correlated infra failure (many sources, same stage) | ≤ 10 min | `SYSTEM_DEGRADED` | n/a | n/a | system-level | ≤ 10 min |
| Lambda or EventBridge stops entirely | **≤ 6 min** | via watchdog silence | n/a | n/a | external | 6 min + email latency ≈ **10 min** |
| Total AWS account death | ≤ 6 min | via watchdog silence | n/a | n/a | external | ≈ 10 min |

**Worst realistic case for a `Background` source:** upstream breaks at 10:00:01, first observed 10:30, transient class, confirmed 10:31, alert 10:31. Blind spot 30 min, confirmation 1 min. **The owner controls the 30 via `criticality`, and the system refuses to let that number silently exceed the declared intent.**

---

## 11. Scheduler and work queue

### 11.1 Model

- EventBridge Scheduler fires **every 1 minute**. It carries no state and knows nothing about sources.
- All scheduling lives in `SRC#<id>/META` as data: `base_interval_secs`, `interval_override_secs`, `criticality`, `next_check_at`, `probe_attempts`, `consecutive_failures`, `enabled`.
- Only sources with `next_check_at <= now` are considered.
- **GSI1 (`DUE`) is an advisory discovery hint** and is eventually consistent (INV-7). Authority is the conditional base-table update.

### 11.2 Rescheduling rules

```
on success:
    next_check_at = now + effective_interval + jitter
    probe_attempts = 0; consecutive_failures = 0

on transient failure, probe_attempts < 2:
    next_check_at = now + 30 s          // lands on the next tick

on transient failure, probe_attempts >= 2:
    next_check_at = now + min(effective_interval * 2^(consecutive_failures - 2), 2 h) + jitter

on hard failure:
    next_check_at = now + backoff_for(kind) + jitter   // per §10.4

on 429:
    next_check_at = now + max(retry_after, 60 s)       // no jitter — honour exactly

jitter = uniform(0, min(0.10 * effective_interval, 30 s))
```

Jitter's purpose is **not** evasion — at 288 requests/day/source you are indistinguishable from one attentive human. Its purpose is preventing many sources from aligning on the same tick and the same top-of-minute.

### 11.3 Tick pseudocode

```rust
async fn run_tick(deps: &Deps, cfg: &TickConfig) -> TickReport {
    let inv_id = uuid_v4();
    let mut counters = TickCounters::default();
    let deadline = Instant::now() + cfg.tick_budget;          // 45 s

    // ── 0. Notification recovery FIRST (priority 1 beats priority 2) ──
    let outstanding = deps.repo.list_open_notifications(now - 5min).await;
    deliver(deps, outstanding, &mut counters).await;

    // ── 1. Discover (advisory) ──
    let candidates = deps.repo.query_due_hint(now, cfg.claim_limit).await; // GSI1

    // ── 2. Claim (authoritative conditional write) ──
    let claimed = deps.repo.claim(candidates, now, cfg.lease, &inv_id).await;
    // ConditionalCheckFailed here => LeaseContention => skip silently, NOT an error

    // ── 3–10. Process with bounded concurrency ──
    let results = futures::stream::iter(claimed)
        .map(|src| async move {
            tokio::time::timeout(cfg.per_source_budget, process_source(deps, src)).await
        })
        .buffer_unordered(cfg.concurrency)                    // 8
        .take_until(tokio::time::sleep_until(deadline))
        .collect::<Vec<_>>()
        .await;
    // Sources not reached: NOT committed. next_check_at unchanged.
    // Their lease expires in 90 s and they are retried. This is correct, not a bug.

    // ── 11. Notify newly created events ──
    deliver(deps, results.new_events(), &mut counters).await;

    // ── 13. Close the tick ──
    let window = deps.repo.record_tick(counters).await;       // ADD + ReturnValues=ALL_NEW
    if let Some(sys) = evaluate_correlation(&window) { emit_system_degraded(sys).await; }

    let status = if counters.infra_errors > 0 { TickStatus::Fail } else { TickStatus::Ok };
    deps.heartbeat.ping(status).await;                        // INV-9
    report
}
```

### 11.4 Platform retry configuration — **document prominently, easy to get catastrophically wrong**

EventBridge Scheduler's `MaximumRetryAttempts` accepts values from 0 to 185</cite> and **defaults to 185**, with `MaximumEventAgeInSeconds` defaulting to 24 hours. AWS Lambda's asynchronous invocation retry defaults to 2. Left at defaults, a systematically failing function at a 1-minute cadence can generate an enormous invocation storm and replay entire batches, overriding our per-source backoff semantics.

**Mandatory configuration:**

```yaml
EventBridge Scheduler:
  MaximumRetryAttempts:      0
  MaximumEventAgeInSeconds:  60
Lambda (async invocation config):
  MaximumRetryAttempts:      0
  MaximumEventAgeInSeconds:  60
Lambda:
  ReservedConcurrentExecutions: 3    # caps blast radius of any pileup
```

Our engine implements per-source retry and backoff. Platform retries are strictly harmful here.

---

## 12. Concurrency and execution budgets (V1 defaults)

| Parameter | V1 value | Rationale | Change when |
|---|---|---|---|
| Lambda memory | **512 MB** | ~0.3 vCPU. If CPU-bound, doubling memory halves duration so GB-s is neutral while wall time improves. | measured duration is I/O-dominated → try 256 MB |
| Lambda timeout | **60 s** | Must not routinely span two 1-minute ticks. | p99 tick duration > 40 s |
| Reserved concurrency | **3** | Bounds a runaway pileup. | sustained overlap is legitimate |
| Tick budget | **45 s** | Leaves 15 s to write health state on a clean path rather than being SIGKILLed. | — |
| Per-source budget | **20 s** | fetch + parse + persist for one source | p99 per-source > 12 s |
| HTTP timeout | **10 s** total, 4 s connect | | a legitimate source is consistently slower |
| Concurrency | **`buffer_unordered(8)`** | politeness to origins; 8 in-flight 300 KB responses is trivial at 512 MB | due-rate exceeds throughput |
| Claim limit | **30 per tick** | covers ~300 sources at 10-min intervals: `due_per_min = Σ(1/interval_min)` | backlog metric fires (§29) |
| Lease duration | **90 s** | > Lambda timeout (60 s) + margin, so a killed invocation's sources free ~1.5 ticks later and never overlap a live one | Lambda timeout changes |
| Transaction chunk | **25 transitions = 50 actions** | 100 actions at 2× transactional capacity</cite> would throttle a 10-WCU table | base WCU raised |
| Notification cap | **8 per source per tick**, 20 global per tick | beyond → grouped digest; remainder stays `unsent` for the next tick | — |

**Unprocessed claimed items.** If the tick budget expires with sources claimed but unprocessed, they are simply not committed. `next_check_at` is unchanged, the lease expires in 90 s, and the next tick retries them. This is correct behaviour under INV-12, not an error.

---

## 13. State-transition + event atomicity protocol

**This is the most correctness-critical section in the document. A future implementation chat must not reinvent this algorithm.**

### 13.1 The bug this protocol exists to prevent

An earlier design wrote job records, then event records, then a META commit marker. It is **incorrect**:

```
stored:  JOB#123  state=inactive  transition_seq=4
fetched: JOB#123 present  →  derive JOB_REPOSTED at seq 5

Step 1: write JOB#123 { state=active, transition_seq=5 }   ✅ durable
        *** Lambda crashes ***
Step 2: write EVT#…JOB_REPOSTED…                            ❌ never happens

META was not advanced, so the source is still due and is retried.
On retry:  stored state = active,  fetched state = active
           →  NO TRANSITION DETECTED  →  event never regenerated
           →  the phone notification is silently and permanently lost.
```

This violates INV-1 and therefore priority (1). The fix is not "retry harder" — it is to make the two writes inseparable.

### 13.2 Deterministic event identity

```
event_key = base32( sha256( source_id ‖ 0x1F ‖ external_id ‖ 0x1F
                          ‖ event_type ‖ 0x1F ‖ transition_seq ) )[0..26]

DynamoDB SK = "EVT#" + event_key
```

`transition_seq: u64` lives on the job record and increments on **every** event-producing transition.

**Why `transition_seq` and not a content hash.** A content-based key collides on genuine repeat transitions: a job removed → reposted → removed → reposted with unchanged content produces two identical keys, so the *second* real repost is deduped away and never notified — a silent miss. `transition_seq` gives both required properties simultaneously:

- **Stable under retry.** A failed transaction leaves the job at seq `k`, so the retry recomputes `k+1` and derives the identical key. (INV-2)
- **Distinct across genuine repeats.** The second repost occurs at seq `k+3`. Different key. (INV-2 does not over-apply.)

ULIDs and timestamps live as *attributes* for ordering. They must never be part of the key.

### 13.3 One transition per job per poll (INV-13)

TransactWriteItems cannot target the same item with multiple operations within the same transaction</cite>. A job that simultaneously reappears *and* newly matches the filter would require two `Update`s on the same `JOB#` item. Therefore transitions are collapsed by strict precedence:

```
1. JOB_REPOSTED         (was inactive, now present)
2. NEW_JOB              (never seen before)
3. BECAME_RELEVANT      (relevant false → true)
4. BECAME_IRRELEVANT    (relevant true → false)
5. JOB_UPDATED          (content_hash changed only)
6. JOB_REMOVED          (absent for ≥2 polls)
```

The highest-precedence applicable event is emitted; all other changed fields are still written in the same job `Update` and recorded in the event's `after` block. `JOB_REPOSTED` and `NEW_JOB` are mutually exclusive. A reposted job that is also relevant notifies via `JOB_REPOSTED`, so nothing is lost.

**Consequence:** every transition is exactly 2 transaction actions. Chunk size 25 → 50 actions, comfortably under the 100-action cap.

### 13.4 The protocol

```
PHASE A — TRANSITIONS (chunked, atomic pairs)
  for chunk in transitions.chunks(25):
      TransactWriteItems(
          ClientRequestToken = sha256(source_id ‖ poll_seq ‖ chunk_index)[0..36],
          items = chunk.flat_map(|t| [
              Update {
                  Key: (SRC#<src>, JOB#<ext_id>),
                  ConditionExpression:
                      "attribute_not_exists(SK) OR transition_seq = :old",
                  UpdateExpression:
                      "SET transition_seq = :new, #state = :st, relevant = :rel,
                           content_hash = :ch, title = :t, /* …fields… */
                           last_seen_at = :now, filter_version = :fv
                       REMOVE absent_since_poll, #ttl",
              },
              Put {
                  Item: {
                      PK: SRC#<src>,  SK: EVT#<event_key>,
                      event_type, transition_seq: :new, detected_at, ulid,
                      before: {…}, after: {…},
                      notify_state: "unsent"      ← INV-3, set HERE
                      GSI2PK: "NOTIFY#OPEN",      ← only if notify-worthy
                      GSI2SK: <detected_at>,
                      notify_attempts: 0, ttl: now + 180 d,
                  },
                  ConditionExpression: "attribute_not_exists(SK)",
              },
          ]))

PHASE B — NON-TRANSITION WRITES (plain, idempotent, no events)
  - absence markers:  SET absent_since_poll = :poll_seq
                      COND attribute_not_exists(absent_since_poll)
  - hourly AGG rollup: UpdateItem with ADD expressions
  - S3 raw_latest / snapshot puts

PHASE C — COMMIT MARKER (single small transaction, LAST)
  TransactWriteItems([
      Update SRC#<src>/META {
          SET next_check_at, poll_seq = poll_seq + 1, last_success_at,
              last_etag, last_body_hash, last_shape_hash, last_job_count,
              health_state, consecutive_failures, probe_attempts,
              bootstrap_state
          REMOVE lease_until, lease_owner
          COND: lease_owner = :inv_id            ← we still hold the lease
      },
      Put POLL#<ts> { …telemetry… , ttl: now + 90 d },
  ])
```

**`ClientRequestToken`** makes a chunk idempotent for 10 minutes after the first request completes</cite>, eliminating the "did my transaction land?" ambiguity during a network partition. It is belt-and-braces: the conditional expressions already guarantee correctness. Note that reusing the same token with different parameters returns `IdempotentParameterMismatch`</cite> — so a *fresh* poll with different data must derive a fresh token, which it does automatically because `poll_seq` has advanced.

### 13.5 Interpreting failures

| Outcome | Meaning | Action |
|---|---|---|
| Transaction succeeds | transition + event both durable | continue |
| `TransactionCanceled` with `ConditionalCheckFailed` on the **job Update** | a prior attempt already applied this exact transition | **treat as success**, skip, do not count as a failure |
| `ConditionalCheckFailed` on the **event Put** but not the job Update | impossible under this protocol (they land together) | if observed, it indicates an out-of-band event write — alert as `Infra`/`DbFailed` |
| `TransactionConflict` | concurrent writer on the same item | retry the chunk once, then defer the source to the next tick |
| `ProvisionedThroughputExceeded` | capacity | SDK adaptive retry; if it persists, `DbThrottled` |
| Commit-marker condition fails (`lease_owner ≠ :inv_id`) | our lease expired and another invocation took over | abandon; the other invocation owns the outcome |

### 13.6 Bootstrap is NOT the transition protocol (INV-10)

Running 300 jobs through Phase A would be 600 actions at 2× capacity ≈ 1,200 WCU in one burst, and would emit 300 events.

```
bootstrap_state: pending → in_progress → complete

1. Fetch, parse, validate contract, normalize, filter.
   Plausibility is SKIPPED (no baseline); adapter contract `min_expected` applies.
2. BatchWriteItem all jobs in batches of 25:
      { state: active, transition_seq: 1, bootstrapped: true,
        first_seen_at: now, relevant, content_hash, filter_version }
   ** Loop on UnprocessedItems ** — BatchWriteItem returns them rather than
      failing, and ignoring this is a classic silent-data-loss bug.
   Rate-limit to ~5 writes/sec to respect provisioned capacity.
3. ONE Put of a SOURCE_BOOTSTRAPPED event carrying the relevant-jobs summary.
4. Commit marker (Phase C) sets bootstrap_state = complete, health = HEALTHY.
```

Crash mid-bootstrap: `bootstrap_state` is still `in_progress`, `next_check_at` unchanged, source retried. All job writes are idempotent `Put`s keyed by `(source_id, external_id)`, so replay is a no-op for what already landed. **Because no `NEW_JOB` events were ever created, no storm is possible even across many retries.**

### 13.7 Crash walkthroughs

**NEW_JOB.** Job absent from stored index. Transaction: `Put JOB#x {seq:1}` (COND `attribute_not_exists`) + `Put EVT#hash(…,NEW_JOB,1)` (COND `attribute_not_exists`).
- *Crash before:* nothing durable. Retry re-derives NEW_JOB at seq 1 → identical key → idempotent. ✅
- *Crash after:* job exists at seq 1. Retry's diff sees an existing active job with an unchanged content hash → no transition → no duplicate event. The event is already durable and already in GSI2. ✅

**JOB_REPOSTED (the original bug).** Stored `{state: inactive, seq: 4}`, job reappears.
- *Crash before:* nothing durable. Retry sees `inactive` vs present → re-derives JOB_REPOSTED at seq 5 → identical key. ✅
- *Crash after:* both durable together. Retry sees `active` vs present, unchanged hash → no transition. The event exists and is `unsent` in GSI2 → the notification sweeper delivers it. ✅ **The old failure mode is structurally impossible: state cannot reach `active` without the event.**

**BECAME_RELEVANT.** Identical structure; `relevant` false→true at seq k→k+1.
- *Crash between event-durable and Telegram send:* the event is `notify_state = "unsent"` **with GSI2 attributes already set inside the transaction** (INV-3). The next tick's recovery sweep finds it and delivers. ✅ *This is the gap that a naive "set notify_state after the transaction" design would leave open, and it would silently lose the alert.*

**Crash between chunks.** Chunk 1 (25 jobs) committed with their events; chunk 2 not. `next_check_at` unchanged → source retried. Retry re-fetches: chunk-1 jobs show no transition (already applied); chunk-2 jobs show their original stored state and re-derive identical transitions at identical seqs. ✅

**Crash after all chunks, before the commit marker.** All transitions + events durable. META not advanced. Retry re-polls with *fresh* data, sees no transitions for the already-applied jobs, and correctly emits events for anything that changed upstream in the interim. Only the POLL telemetry record from the first attempt is lost — telemetry loss, not correctness loss. `consecutive_failures` is not incremented because the poll did not fail. ✅

**Overlapping invocations.** Lease 90 s > Lambda timeout 60 s, so a live invocation's lease can never expire under it. A second invocation cannot claim a leased source (INV-7). If a dying invocation's in-flight transaction lands after it is killed, the later invocation's diff simply observes the applied state. ✅

**Duplicate EventBridge delivery.** Two invocations, same GSI hint, both attempt the conditional claim; exactly one wins; the loser records `LeaseContention` (not an error). ✅

**Multiple genuine transitions over time.** seq 1 NEW_JOB → 2 BECAME_IRRELEVANT → 3 JOB_REMOVED → 4 JOB_REPOSTED → 5 BECAME_RELEVANT. Five distinct keys, five distinct events, three notifications. ✅

### 13.8 Absence tracking without write amplification (INV-12 corollary)

Naively storing `last_seen_at` and `absent_ticks` on every job on every poll is ~864,000 writes/day at V1 scale and destroys the capacity budget. It is also non-idempotent: a crashed `absent_ticks` increment double-counts on retry.

**Absence is computed, not accumulated.**

- `SRC#<id>/META.poll_seq` — monotonic, incremented once per successful poll in the commit marker.
- `JOB#<ext>.absent_since_poll` — **sparse**; written only when a present job first goes missing, removed when it reappears.

```
job absent this poll, absent_since_poll unset  → SET absent_since_poll = poll_seq   (1 write)
job absent this poll, absent_since_poll set    → no write
    if poll_seq - absent_since_poll >= 1       → JOB_REMOVED transition (atomic pair)
job present, absent_since_poll set             → REMOVE absent_since_poll           (1 write)
job present, absent_since_poll unset           → NO WRITE AT ALL
```

Steady-state writes per poll: **zero** for unchanged present jobs.

`last_seen_at` is a display field updated only when the job item is written anyway. The exact value is derived: for `state == active && absent_since_poll` unset, the true last-seen time is `SRC.last_success_at`, because an absent job would necessarily have been written.

---

## 14. Event model

| Event | Trigger | Durable | Notify | Channel | Idempotency identity |
|---|---|---|---|---|---|
| `NEW_JOB` | id present, absent from stored index | ✅ | **if `relevant`** | job DM | `hash(src, ext, NEW_JOB, seq)` |
| `BECAME_RELEVANT` | `relevant` false → true | ✅ | **always** | job DM | `hash(src, ext, BECAME_RELEVANT, seq)` |
| `JOB_REPOSTED` | present, stored `state == inactive` | ✅ | **if `relevant`** | job DM | `hash(src, ext, JOB_REPOSTED, seq)` |
| `JOB_UPDATED` | `content_hash` changed only | ✅ | no | — | `hash(src, ext, JOB_UPDATED, seq)` |
| `BECAME_IRRELEVANT` | `relevant` true → false | ✅ | no | — | `hash(src, ext, BECAME_IRRELEVANT, seq)` |
| `JOB_REMOVED` | `poll_seq - absent_since_poll >= 1` | ✅ | no | — | `hash(src, ext, JOB_REMOVED, seq)` |
| `SOURCE_BOOTSTRAPPED` | bootstrap completes | ✅ | **yes** (one summary) | health | `hash(src, BOOTSTRAP, poll_seq)` |
| `SOURCE_DEGRADED` | `HEALTHY → DEGRADED` | ✅ | yes, low severity | health | `hash(src, DEGRADED, first_failure_at)` |
| `SOURCE_FAILED` | `→ FAILED` | ✅ | **yes, immediate** | health | `hash(src, FAILED, first_failure_at)` |
| `SOURCE_RECOVERED` | `DEGRADED/FAILED → HEALTHY` | ✅ | **yes** (with outage duration + job count) | health | `hash(src, RECOVERED, first_failure_at)` |
| `SOURCE_QUARANTINED` | 20 consecutive failures | ✅ | yes, once | health | `hash(src, QUARANTINED, first_failure_at)` |
| `API_CHANGED` | `shape_hash` changed, contract still valid | ✅ | yes, throttled ≤1/source/day | health | `hash(src, API_CHANGED, new_shape_hash)` |
| `SYSTEM_DEGRADED` | correlation rule (§25) | ✅ | **yes, immediate; suppresses per-source alerts in the window** | health | `hash(SYSTEM, stage, domain, window_id)` |
| `NOTIFICATION_DEGRADED` | ≥3 open events older than 5 min | ✅ | health chat **+ healthchecks `/fail`** (Telegram may itself be broken) | health | `hash(NOTIFY_DEGRADED, window_id)` |
| `NOTIFICATION_RECOVERED` | queue drains after degraded | ✅ | yes | health | `hash(NOTIFY_RECOVERED, window_id)` |
| `FILTER_CHANGED` | `filter_version` bump | ✅ | yes, **one summary only** | health | `hash(FILTER_CHANGED, filter_version)` |

**Notify-worthy events set `GSI2PK = "NOTIFY#OPEN"` inside the creating transaction.** Non-notifying events omit the GSI2 attributes entirely and never enter the sweeper's view.

**Daily digest** (08:00 America/Toronto) is proof-of-life only, never part of job delivery: healthy/total sources, quarantined source names, events in the last 24 h, median detection lag, notification queue depth.

---

## 15. Notification delivery state machine

```
   (created inside the transition transaction)
              notify_state = "unsent"
              GSI2PK = "NOTIFY#OPEN", GSI2SK = detected_at
                            │
                            │ conditional UpdateItem
                            │ COND: notify_state = "unsent"
                            │       OR (notify_state = "claimed"
                            │           AND notify_claimed_at < now - 5 min)
                            ▼
              notify_state = "claimed", notify_claimed_at = now
                            │
                            │ Telegram sendMessage
                     ┌──────┴──────┐
              success│             │failure / 429
                     ▼             ▼
   notify_state = "sent"     notify_state stays "claimed"
   REMOVE GSI2PK, GSI2SK      notify_attempts += 1
   notified_at = now          last_notify_error = …
   (leaves the sweeper view)  (recovered by the sweeper after 5 min)
```

**Semantics:** at-least-once (D13). If the process dies after Telegram accepts but before the `sent` confirmation, the retry produces a duplicate. That is accepted. A drop is not (INV-5).

### 15.1 Telegram rate limiting — modelled on the actual limits

Verified 2026-08-16: avoid more than one message per second in a single chat; in a group, no more than 20 messages per minute; for bulk notifications, approximately 30 messages per second</cite>. Critically, exceeding the limit blocks the bot entirely for the `retry_after` duration — no API calls succeed during that time, for all chats, not just the one being sent to</cite>.

**This means the job chat and the health chat share a failure domain at the bot-account level. A job-alert storm can silence health alerts.** The limiter must therefore be global-aware, not per-chat only.

```
Limiters:
  job DM         token bucket  1 msg / s
  health group   token bucket  20 msg / 60 s   (forum topics share the group limit)
  global         token bucket  25 msg / s      (headroom under the ~30/s ceiling)

Ordering:
  health events are always dequeued before job events
  (a health alert usually explains why the job alerts look strange)

Caps:
  ≤ 8 individual alerts per source per tick  → beyond that, one grouped digest
  ≤ 20 messages per tick globally            → remainder stays "unsent", next tick

On 429:
  read parameters.retry_after
  PARK ALL SENDING for that duration (the bot is globally blocked)
  leave every affected event in "claimed"/"unsent"
  emit NOTIFICATION_DEGRADED if the queue exceeds 3 events / 5 minutes
```

**Invariant restated:** throttling delays delivery; it never destroys an event (INV-5). A grouped digest contains every suppressed item, so capping is a formatting decision, not a data-loss decision.

### 15.2 Channels

| Channel | Content | Telegram target |
|---|---|---|
| Job alerts | `NEW_JOB`, `BECAME_RELEVANT`, `JOB_REPOSTED` (relevant only) | direct message to the owner |
| Health | all `SOURCE_*`, `API_CHANGED`, `SYSTEM_DEGRADED`, `NOTIFICATION_*`, `FILTER_CHANGED`, daily digest | separate group or forum topic |
| Total system death | watchdog silence | **email via healthchecks.io — deliberately not Telegram** |

Job alert format (inline keyboard button for a real one-tap target):

```
🚨 NEW COHERE INTERNSHIP
Software Engineering Intern
Toronto, ON · Internship
First seen: 10:07:13 ET
Posted: 2026-08-16
[ APPLY ]                      ← inline_keyboard url button
```

Timestamps render in `America/Toronto`; storage is always UTC.

---

## 16. DynamoDB schema

Table `jobmon`. Provisioned capacity. PITR **on**. TTL attribute `ttl`.

### 16.1 Key design

| Entity | PK | SK | GSI1PK | GSI1SK | GSI2PK | GSI2SK | TTL |
|---|---|---|---|---|---|---|---|
| Source | `SRC#<id>` | `META` | `DUE`¹ | `<next_check_at>` | — | — | — |
| Job | `SRC#<id>` | `JOB#<ext_id>` | — | — | — | — | 180 d when inactive² |
| Event | `SRC#<id>` | `EVT#<event_key>` | — | — | `NOTIFY#OPEN`³ | `<detected_at>` | 180 d |
| Poll attempt | `SRC#<id>` | `POLL#<iso8601>` | — | — | — | — | 90 d |
| Hourly rollup | `SRC#<id>` | `AGG#<yyyy-mm-ddThh>` | — | — | — | — | 400 d |
| Correlation window | `SYS#CORR` | `WIN#<epoch_min/10>` | — | — | — | — | 1 h |
| Subsystem health | `SYS#HEALTH` | `SUB#<name>` | — | — | — | — | — |
| Global config | `SYS#CONFIG` | `FILTER` | — | — | — | — | — |

¹ Written only while `enabled = true`. Disabled/quarantined sources drop out of the index entirely.
² `REMOVE #ttl` in the same update that sets `state = active` — otherwise DynamoDB deletes a reactivated job.
³ Present only while `notify_state ∈ {unsent, claimed}`; removed on `sent`. Sparse and normally empty.

### 16.2 Attributes

```
SRC#<id> / META
  # ---- configuration (data, not code) ----
  source_id, company, source_kind (Pull|Push), adapter_type, adapter_version,
  endpoint_config{}, enabled, criticality (Critical|Standard|Background),
  base_interval_secs, interval_override_secs?, bootstrap_mode,
  filter_overrides{}, plausibility{ min_ratio, min_abs, allow_zero },
  tags[]
  # ---- schedule state ----
  next_check_at, poll_seq, lease_until?, lease_owner?
  # ---- health state ----
  health_state, failure_stage?, failure_domain?, failure_kind?,
  consecutive_failures, probe_attempts, first_failure_at?,
  last_attempt_at, last_success_at, last_health_alert_at?
  # ---- contract state ----
  last_etag?, last_modified?, last_body_hash?, last_shape_hash?,
  last_job_count, last_raw_put_at?, bootstrap_state, filter_version

SRC#<id> / JOB#<ext_id>
  title, location_raw, country, region, city, employment_type, url,
  posted_at?, first_seen_at, last_seen_at, state (active|inactive),
  relevant, content_hash, transition_seq, absent_since_poll?,
  filter_version, bootstrapped, ttl?

SRC#<id> / EVT#<event_key>
  event_type, external_id?, transition_seq?, detected_at, ulid,
  before{}, after{},
  notify_state (unsent|claimed|sent|na), notify_claimed_at?, notify_attempts,
  notified_at?, last_notify_error?, GSI2PK?, GSI2SK?, ttl

SRC#<id> / POLL#<ts>
  attempted_at, duration_ms, stage_reached, http_status?, content_type?,
  response_bytes?, etag_hit, job_count_raw?, job_count_parsed?,
  shape_hash?, adapter_version, outcome, failure_domain?, failure_kind?,
  next_check_at, ttl

SRC#<id> / AGG#<hour>            # all additive — see §24
  attempts, successes, etag_hits, latency_sum_ms, latency_count, latency_max_ms,
  lat_le_100, lat_le_250, lat_le_500, lat_le_1000, lat_le_2500, lat_le_5000, lat_le_inf,
  http_2xx, http_304, http_4xx, http_429, http_5xx,
  fail_<kind> (one counter per observed kind),
  jobs_seen, events_new, events_became_relevant, ttl

SYS#CORR / WIN#<bucket>
  attempted, failed,
  fail_<STAGE>_<DOMAIN> (Number), src_<STAGE>_<DOMAIN> (String Set), ttl

SYS#HEALTH / SUB#<notification|archive|scheduler|repository>
  state, since, consecutive_failures, last_error, last_ok_at
```

### 16.3 Access patterns

| # | Pattern | Operation | Consistency |
|---|---|---|---|
| A1 | Discover due sources | `Query GSI1 PK="DUE", SK <= now, Limit 30, ScanIndexForward` | **eventual — advisory only** |
| A2 | Claim a source | `UpdateItem SRC#<id>/META` with the dual condition | **strongly consistent — authoritative (INV-7)** |
| A3 | Load job index for diff | `Query PK=SRC#<id>, begins_with(SK,"JOB#")` behind `load_job_index` | eventual acceptable |
| A4 | Apply a transition | `TransactWriteItems` per §13.4 | strongly consistent |
| A5 | Commit a poll | `TransactWriteItems{META, POLL}`, `COND lease_owner = :inv_id` | strongly consistent |
| A6 | Find outstanding notifications | `Query GSI2 PK="NOTIFY#OPEN", SK <= now, Limit 50` | eventual acceptable — a 1 s-late discovery is irrelevant |
| A7 | Claim / confirm a notification | conditional `UpdateItem` on the EVT item | strongly consistent |
| A8 | Correlation window | `UpdateItem SYS#CORR` with `ADD`, `ReturnValues=ALL_NEW` | strongly consistent, **zero extra reads** |
| A9 | Hourly rollup | `UpdateItem SRC#<id>/AGG#<hour>` with `ADD` | — |
| A10 | Analysis: source history | `Query PK=SRC#<id>, begins_with(SK,"AGG#")` | eventual |

**Eventual-consistency stance.** Only A1 and A6 read eventually-consistent indexes, and neither can cause incorrect behaviour: A1 is filtered by the authoritative conditional write in A2, and A6 only affects *when* a notification is discovered, never *whether*.

### 16.4 Capacity (V1)

| Component | WCU | RCU | Note |
|---|---|---|---|
| Base table | 10 | 10 | raised from 5 to absorb transactional 2× cost and bootstrap bursts |
| GSI1 (`DUE`) | 5 | 5 | written only when `next_check_at` changes |
| GSI2 (`NOTIFY#OPEN`) | 3 | 3 | sparse, normally empty |
| **Total** | **18** | **18** | of the **25 / 25** always-free allowance (verified 2026-08-16) |

Transactional writes consume twice the capacity of the equivalent non-transactional operation</cite>. A 25-transition chunk = 50 actions ≈ 100 WCU in one burst, absorbed by the burst bucket (300 s × 10 WCU = 3,000 WCU) and SDK adaptive retry.

---

## 17. Rust workspace

```
job-monitor/
├── Cargo.toml                  # workspace
├── rust-toolchain.toml         # pinned
├── crates/
│   ├── errors/                 # PipelineError, Stage, FaultDomain, FailureKind, Detail
│   ├── core/                   # PURE, SYNCHRONOUS, no I/O, no provider types
│   │   ├── model.rs            # Source, Job, JobIndex, Event, PollOutcome, Health
│   │   ├── normalize.rs        # RawJob -> NormalizedJob
│   │   ├── filter.rs           # relevance predicate + filter_version
│   │   ├── shape.rs            # shape_hash, body_hash, content_hash
│   │   ├── plausibility.rs     # per-source suspicious-response gate
│   │   ├── diff.rs             # (JobIndex, Vec<NormalizedJob>) -> Vec<Transition>
│   │   ├── event_key.rs        # deterministic identity
│   │   ├── schedule.rs         # next_check_at, jitter, probe, backoff, Retry-After
│   │   └── health.rs           # state machine + alert-threshold rules
│   ├── ports/                  # trait definitions ONLY
│   ├── adapters/               # ATS-family parsers; depends on core + errors ONLY
│   ├── engine/                 # async orchestration, generic over ports
│   └── infra/                  # concrete port impls
│       ├── repo_dynamo.rs
│       ├── repo_memory.rs      # ~150 lines, for tests
│       ├── fetch_reqwest.rs
│       ├── notify_telegram.rs
│       ├── archive_s3.rs
│       └── heartbeat_http.rs
├── bin/
│   ├── lambda/                 # ~80 lines: wire ports, call engine
│   └── admin/                  # source registration CLI
├── tests/fixtures/             # captured real payloads, one dir per adapter
└── infra/template.yaml         # AWS SAM
```

| Crate | Responsibility | May depend on | Must NOT know about |
|---|---|---|---|
| `errors` | the three-axis taxonomy | nothing (std + serde) | AWS, HTTP, tokio |
| `core` | all business logic, **100% sync** | `errors` | ports, AWS, HTTP, tokio, async |
| `ports` | trait definitions | `core`, `errors` | any concrete impl |
| `adapters` | byte-slice → `Vec<RawJob>`, **pure** | `core`, `errors` | HTTP clients, tokio, AWS. **Adapters never perform networking.** |
| `engine` | one tick: claim → fetch → diff → persist → notify | `core`, `ports`, `adapters`, `errors` | AWS SDK, reqwest, Telegram |
| `infra` | concrete impls | everything above + AWS SDK, reqwest | business rules |
| `bin/lambda` | wiring only | everything | business rules |
| `bin/admin` | source CRUD | `core`, `ports`, `infra` | — |

### 17.1 Traits

Native `async fn` in traits (stable since Rust 1.75), **static dispatch via generics**, no `#[async_trait]`, no `Box<dyn>`. There is exactly one implementation per port in production and one in tests; monomorphization costs nothing and avoids dyn-compatibility problems.

```rust
pub trait Clock { fn now(&self) -> DateTime<Utc>; }
pub trait Jitter { fn jitter(&self, max: Duration) -> Duration; }

pub trait JobSource {                                   // sync + pure
    fn adapter_type(&self) -> &'static str;
    fn adapter_version(&self) -> u32;
    fn contract(&self) -> &AdapterContract;
    fn build_request(&self, cfg: &EndpointConfig, cache: Option<&CacheValidators>)
        -> Result<HttpRequest, PipelineError>;
    fn parse(&self, body: &[u8], content_type: Option<&str>)
        -> Result<Vec<RawJob>, PipelineError>;
}

pub trait HttpFetcher {
    async fn fetch(&self, req: HttpRequest, timeout: Duration)
        -> Result<HttpResponse, PipelineError>;
}

pub trait Repository {
    async fn query_due_hint(&self, now: DateTime<Utc>, limit: usize)
        -> Result<Vec<SourceId>, PipelineError>;
    async fn claim(&self, ids: Vec<SourceId>, now: DateTime<Utc>,
                   lease: Duration, owner: &str) -> Result<Vec<Source>, PipelineError>;
    async fn load_job_index(&self, id: &SourceId) -> Result<JobIndex, PipelineError>;
    async fn apply_transitions(&self, id: &SourceId, poll_seq: u64,
                               t: &[Transition]) -> Result<AppliedCount, PipelineError>;
    async fn write_non_transitions(&self, id: &SourceId, w: &NonTransitionWrites)
        -> Result<(), PipelineError>;
    async fn commit_poll(&self, marker: CommitMarker) -> Result<(), PipelineError>;
    async fn bootstrap_source(&self, id: &SourceId, jobs: &[NormalizedJob])
        -> Result<(), PipelineError>;
    async fn list_open_notifications(&self, stale_before: DateTime<Utc>, limit: usize)
        -> Result<Vec<OpenEvent>, PipelineError>;
    async fn claim_notification(&self, k: &EventRef, now: DateTime<Utc>)
        -> Result<ClaimResult, PipelineError>;
    async fn confirm_notification(&self, k: &EventRef) -> Result<(), PipelineError>;
    async fn record_notify_failure(&self, k: &EventRef, e: &PipelineError)
        -> Result<(), PipelineError>;
    async fn record_tick(&self, c: TickCounters) -> Result<CorrelationWindow, PipelineError>;
    async fn record_rollup(&self, id: &SourceId, hour: &str, m: RollupDelta)
        -> Result<(), PipelineError>;
}

pub trait NotificationSink {
    async fn send(&self, ch: Channel, msg: &Message) -> Result<(), PipelineError>;
}
pub trait RawArchive {
    async fn put(&self, key: &ArchiveKey, gz_body: &[u8]) -> Result<String, PipelineError>;
}
pub trait Heartbeat {
    async fn ping(&self, status: TickStatus) -> Result<(), PipelineError>;
}
```

`load_job_index` deliberately hides *how* the index is obtained. V1 implements it with a full `Query`; §29 describes the packed-item optimization that replaces the implementation without touching the engine.

---

## 18. Adapter architecture

**Rule (D10): one adapter per ATS family, never one per company.** Twenty Greenhouse companies share one parser and differ only by `endpoint_config`.

```rust
pub struct AdapterContract {
    pub array_path:     &'static str,      // "jobs", "data.postings", ""
    pub required_paths: &'static [&'static str],  // relative to an array element
    pub min_expected:   usize,             // sanity floor for a first/bootstrap poll
}
```

**Contract validation vs shape hashing (INV-11).** The adapter validates *its own dependencies*, not structural equality:

- New sibling field appears → `shape_hash` changes → `API_CHANGED` telemetry + snapshot → **the poll succeeds normally**.
- A `required_path` disappears → `RequiredFieldMissing` → immediate `SOURCE_FAILED`.

`shape_hash` = hash of the sorted set of JSON key paths across a sample of array elements, values discarded. It fingerprints structure, not content, so it is stable across normal job churn and changes the instant the schema does.

**Adapter versioning.** `adapter_version: u32` is bumped on any parsing-behaviour change. A version bump forces an S3 snapshot on the next poll so the pre/post payloads are comparable. Stored on `META` and on every `POLL` record.

**Registry.**

```rust
pub fn adapter_for(t: &str) -> Option<&'static dyn JobSource> {
    match t {
        "greenhouse"     => Some(&GREENHOUSE),
        "lever"          => Some(&LEVER),
        "ashby"          => Some(&ASHBY),
        "workday"        => Some(&WORKDAY),
        "smartrecruiters"=> Some(&SMARTRECRUITERS),
        custom           => CUSTOM_REGISTRY.get(custom),
    }
}
```

An unknown `adapter_type` in the database is `ConfigInvalid` (Infra domain) — an operator error, alerted, source skipped, never a crash.

**Fixtures.** Every adapter ships real captured payloads in `tests/fixtures/<adapter>/`, plus deliberately mutated variants: a required field removed, an HTML error page, a truncated body, and an empty array. Adapters are tested purely on bytes; they never open a socket.

---

## 19. Adding a new company

### Case A — existing supported ATS (target: 1–2 minutes, no deployment)

```bash
cargo run -p admin -- add-source \
    --company    "Cohere" \
    --adapter    greenhouse \
    --board      cohere \
    --criticality standard \
    --interval   10m \
    --bootstrap  relevant_summary
```

Writes one `SRC#<id>/META` item. `admin` validates: adapter exists, `endpoint_config` satisfies the adapter's schema, `interval <= criticality.max_interval_secs()`, and performs one live probe fetch + parse before committing. **Nothing else changes** — not the scheduler, engine, database schema, notification path, health logic, or deployment.

### Case B — new but simple JSON API (~1 hour, requires deployment)

1. Capture a live response → `tests/fixtures/<name>/sample.json`.
2. Implement `JobSource` (~80 lines) + `AdapterContract`.
3. Write three tests: happy parse, required-field-removed → `RequiredFieldMissing`, HTML body → `WrongMediaType`.
4. Register in the adapter registry.
5. `sam deploy`.
6. `admin add-source --adapter <name> …`.

### Case C — complex / custom API (~half a day)

As Case B plus pagination handling, cursor state in `endpoint_config`, and possibly per-request headers. Pagination is the most common source of plausibility failures — set `plausibility.min_ratio` conservatively for the first week and watch `POLL` records.

### Case D — browser-required or datacenter-IP-blocked

Not implemented in V1; the **seam exists** (D14). Design: `source_kind = Push`, `next_check_at` set far in the future, and an external fetcher (residential IP, e.g. a Raspberry Pi) POSTs the raw body to a Lambda Function URL. The pipeline runs unchanged from `Decode` onward. Freshness health switches from "poll overdue" to "no push received in N minutes." **The cloud remains authoritative for state, health, events, and notifications.** Build only when triggered (§29).

**Code vs configuration:** code = adapter implementations, contracts, the filter predicate, event and health rules. Configuration = everything in §20.

---

## 20. Source configuration schema

| Field | Type | Required | Meaning |
|---|---|---|---|
| `source_id` | string | ✅ | stable slug, e.g. `cohere-greenhouse` |
| `company` | string | ✅ | display name used in alerts |
| `source_kind` | `Pull` \| `Push` | ✅ | V1 always `Pull` |
| `adapter_type` | string | ✅ | registry key |
| `adapter_version` | u32 | ✅ | set by the registry at registration |
| `endpoint_config` | map | ✅ | adapter-specific: `board`, `token`, `tenant`, `site`, `url`, `headers` |
| `enabled` | bool | ✅ | false removes it from GSI1 entirely |
| `criticality` | `Critical` \| `Standard` \| `Background` | ✅ | declares the accepted blind spot (§10.2) |
| `base_interval_secs` | u32 | ✅ | must satisfy `<= criticality.max_interval_secs()` |
| `interval_override_secs` | u32 | — | temporary manual override; same validation applies |
| `bootstrap_mode` | `silent` \| `relevant_summary` \| `notify_existing_relevant` | ✅ | default `relevant_summary` |
| `filter_overrides` | map | — | per-source relaxations, e.g. accept remote-Canada |
| `plausibility.min_ratio` | f32 | ✅ | default `0.5` — reject if `parsed < ratio × last_job_count` |
| `plausibility.min_abs` | u32 | ✅ | default `3` — never reject when counts are tiny |
| `plausibility.allow_zero` | bool | ✅ | default `false`; `true` for boards that legitimately empty out |
| `tags` | list | — | grouping for digests |

**Derived, never stored:** `failure_detection_sla_secs = criticality.max_interval_secs()`, `effective_interval = interval_override_secs.unwrap_or(base_interval_secs)`.

---

## 21. Filtering and relevance

**Current goal:** Canadian internship and co-op postings relevant to a Canadian undergraduate CS/AI student.

**Filtering operates on the normalized model, never on ATS-specific fields.** Adapters produce `NormalizedJob`; the filter is a single pure predicate over it. This is what makes one filter work across Greenhouse, Lever, Ashby, and every future adapter.

```rust
pub fn is_relevant(job: &NormalizedJob, cfg: &FilterConfig) -> bool
```

Signals: `country == CA` (or remote-with-Canada eligibility, per source override); employment type or title matching internship / co-op / intern / student / new-grad patterns; title-based exclusions for senior and staff roles. Concrete keyword lists live in `core::filter` and are **code**, versioned by `filter_version`.

**Filter versioning (INV-15).** Every job stores the `filter_version` under which its `relevant` flag was computed. On a version bump, jobs are re-evaluated at their next poll, but the resulting relevance changes are routed into **one `FILTER_CHANGED` summary** rather than individual `BECAME_RELEVANT` alerts. Without this, editing the filter fabricates hundreds of fake transitions and buries the channel.

The filter is expected to evolve. Because relevance is stored per job with its version, event history remains interpretable — a `BECAME_RELEVANT` from March is understood against the filter that was live in March.

---

## 22. Plausibility and contract validation

Four distinct conditions, four distinct behaviours. **Conflating them is the most common way this class of system corrupts itself.**

| Condition | Meaning | Poll outcome | Canonical state | Event | Alert |
|---|---|---|---|---|---|
| **Shape changed** | JSON key paths differ; all required paths present | **SUCCESS** | mutated normally | `API_CHANGED` | health, low, ≤1/source/day |
| **Contract invalid** | a `required_path` or `array_path` is missing | FAILURE | **preserved** | `SOURCE_FAILED` | immediate |
| **Parse failed** | body is not valid JSON, or the array shape is unusable | FAILURE | **preserved** | `SOURCE_FAILED` | immediate |
| **Plausibility failed** | contract holds and parsing succeeded, but the count is implausible | FAILURE | **preserved (INV-4)** | `SOURCE_FAILED` | immediate |

**Plausibility rule:**

```
reject if  parsed_count == 0 && last_job_count > 0 && !allow_zero
reject if  last_job_count >= min_abs
           && parsed_count < min_ratio * last_job_count
accept     otherwise
```

Skipped entirely during bootstrap (no baseline exists); the adapter's `min_expected` applies instead.

Typical causes of a plausibility failure: broken pagination, a soft block returning a partial page, a parser regression, or an incomplete upstream response. **Never emit 47 `JOB_REMOVED` events from a suspicious response.** Thresholds are per source because some boards legitimately fluctuate.

---

## 23. Raw payload / S3 strategy

| Object | Written when | Path | Retention |
|---|---|---|---|
| `raw_latest` | `body_hash != last_body_hash` **AND** ≥ 6 h since `last_raw_put_at` | `raw_latest/<source_id>.json.gz` | overwrite in place |
| `snapshot` | parse failure, required-field missing, plausibility failure, shape change, adapter version bump | `snapshot/<source_id>/<iso8601>-<reason>.json.gz` | 90 d lifecycle rule |

**Never written:** on a `304 Not Modified` (there is no body); on an unchanged body; on every successful poll "just because."

**The 6-hour throttle matters.** Many boards embed volatile `updated_at` fields, so `body_hash` changes on nearly every poll. Without the throttle, `raw_latest` would PUT on every poll — at 300 sources that is ~$1/month of pure noise.

**Bucket configuration:** versioning **OFF** (repeated overwrites would otherwise accumulate versions forever), plus a `NoncurrentVersionExpiration` rule as belt-and-braces; lifecycle expiration at 90 days on `snapshot/`; SSE-S3; block all public access; gzip everything.

**Archive failure never invalidates a poll** (INV-6 corollary). `ArchivePutFailed` degrades `SYS#HEALTH/SUB#archive` only; job detection, events, and notifications proceed normally.

**Raw bodies never enter CloudWatch Logs (INV-14).** At 86,400 polls × ~150 KB that is ~13 GB/month against a 5 GB free allowance — a real, recurring, entirely avoidable charge.

---

## 24. Observability

| Store | Contents | Retention |
|---|---|---|
| **DynamoDB `POLL#`** | full record for **every non-OK outcome**; OK outcomes sampled 1-in-10 once source count exceeds 100 | 90 d TTL |
| **DynamoDB `AGG#<hour>`** | additive counters per source per hour | 400 d TTL |
| **DynamoDB `SYS#CORR`** | 10-minute rolling correlation windows | 1 h TTL |
| **CloudWatch Logs** | one structured JSON line per tick + one per non-OK source outcome | **14 d, explicitly set** |
| **S3** | raw payloads only (§23) | 90 d / overwrite |

### 24.1 No fake percentiles

A true p50 cannot be reconstructed from `count`, `sum`, and `max`. **Do not store an approximate `p50`.** V1 rollups are strictly additive, plus fixed histogram buckets that *do* support quantile estimation:

```
attempts, successes, etag_hits
latency_sum_ms, latency_count, latency_max_ms
lat_le_100, lat_le_250, lat_le_500, lat_le_1000, lat_le_2500, lat_le_5000, lat_le_inf
http_2xx, http_304, http_4xx, http_429, http_5xx
fail_<kind>            # one counter per observed FailureKind
jobs_seen, events_new, events_became_relevant
```

Seven bucket counters cost seven extra `ADD` expressions in a single `UpdateItem` — effectively free, and they yield defensible quantile estimates. Exact percentiles come later from sampled `POLL#` records or offline S3 + DuckDB analysis.

### 24.2 Analysis questions this data must answer later

Publication-time distribution (from `first_seen_at`, unbiased because V1 polling is uniform — see D17); detection latency (`first_seen_at − posted_at`); per-source failure rate; 403/429 frequency; schema-change rate (`API_CHANGED` counts); per-source latency distribution; per-source maintenance burden (failures + adapter version bumps); events per source; and — the decision this enables — which sources deserve faster or slower polling.

---

## 25. System-level correlation

**Problem:** at a 1-minute cadence with 20 sources at ~10-minute intervals, the average tick has ~2 due sources. "50% of one tick failed" means one failure. That rule is statistically meaningless and would fire constantly.

**Rule — time-windowed with an absolute floor:**

```
Within the current 10-minute window, emit SYSTEM_DEGRADED when ALL hold:
    distinct_sources_failed_at(stage, domain)  >= 3
    failed / attempted                          >= 0.6
    all failures share the same (stage, domain)
```

**Implementation — one write, zero extra reads:**

```
UpdateItem  PK=SYS#CORR, SK=WIN#<epoch_minute/10>
  ADD attempted :n,
      failed :m,
      fail_<STAGE>_<DOMAIN> :k,
      src_<STAGE>_<DOMAIN> :string_set
  ReturnValues = ALL_NEW          ← the response IS the post-update window state
```

The tick accumulates counters in memory, writes them once at close, and evaluates the rule against the returned item.

**Suppression.** While a `SYSTEM_DEGRADED` window is active, per-source `SOURCE_FAILED` alerts for the *same* `(stage, domain)` are suppressed and coalesced into the system message. When your egress breaks you want one message saying "the fetch layer is down," not twenty saying "Microsoft timed out." Per-source health state still transitions normally and is still recorded — only the *alerting* is coalesced (this preserves INV-6 while satisfying priority 6).

**What it distinguishes:** `Microsoft changed its API` (one source, `Parse`/`Adapter`) from `our entire outbound fetch layer is broken` (many sources, `Connect`/`Upstream`) from `our database is broken` (many sources, `Persist`/`Infra`).

---

## 26. External watchdog

**Provider:** healthchecks.io. Verified 2026-08-16: free accounts are limited to 20 checks</cite>; the free plan retains ~100 ping-log entries per check. **One check for the whole service, not one per source.**

| Signal | Meaning | Result |
|---|---|---|
| `POST /<uuid>` | tick completed with **no Infra-domain errors** | healthy |
| `POST /<uuid>/fail` | tick ran but hit an Infra-domain error (DynamoDB, IAM, config) or notification degradation | **immediate alert** |
| *(silence)* | Lambda, EventBridge, IAM, or the AWS account is dead | alert after grace |

**Configuration: period = 1 minute, grace = 5 minutes.** Alert after ~6 minutes of silence. This tolerates four consecutive missed ticks. Grace was reduced from a previously considered 20 minutes because the owner explicitly prefers occasional false positives over slow discovery of system death.

**The `/fail` distinction is essential (INV-9).** A Lambda that executes every minute but cannot reach DynamoDB would otherwise keep the watchdog green while the system is functionally dead.

**Notification transport: email.** Deliberately *not* Telegram — the watchdog must not share a failure domain with the primary channel. Realistic total-death detection is ~6 minutes plus email latency ≈ 10 minutes. Optionally add a second heartbeat provider if that residual single point of failure becomes unacceptable; not required for V1.

**Note:** ~100 retained ping-log entries at 1 ping/minute is ~100 minutes of history. The watchdog is an alerting mechanism, **not** a telemetry store.

---

## 27. Security and secrets

| Item | Where | Why |
|---|---|---|
| Table name, bucket name, log level, tick parameters | **Lambda environment variables** | not secret, zero-latency |
| Telegram bot token | **SSM Parameter Store, Standard tier, `SecureString`** | free; Secrets Manager would be $0.40/secret/month for no benefit |
| Telegram job chat ID, health chat ID | SSM Standard `SecureString` | |
| healthchecks.io ping UUID | SSM Standard `SecureString` | possession of the URL is the credential |
| Future per-source API credentials | SSM Standard, path `/jobmon/sources/<id>/…` | |

**Caching.** Fetch SSM parameters once and cache in a `OnceCell` across warm invocations — 43,200 `GetParameter` calls per month is pointless overhead. **Failure to load secrets must degrade, not abort:** the tick still polls, diffs, and persists; only notification is unavailable, and it surfaces as `SecretUnavailable` in the `Notify` domain with events left `unsent`. Job detection must never be blocked by a notification-layer problem (INV-6).

**IAM — least privilege, no wildcards:**

- `dynamodb:{GetItem, Query, PutItem, UpdateItem, BatchWriteItem, TransactWriteItems}` scoped to the exact table ARN **and its two index ARNs**
- `s3:PutObject` scoped to `arn:…:<bucket>/raw_latest/*` and `/snapshot/*`
- `ssm:GetParameter`, `ssm:GetParameters` scoped to `/jobmon/*`; `kms:Decrypt` on the SSM default key
- `AWSLambdaBasicExecutionRole` for logs
- **No VPC.** No NAT Gateway. No `*` resources.

---

## 28. Cost model

> ⚠️ **All figures in this section were verified 2026-08-16 and are temporally volatile.** A future AI must reverify against current AWS pricing pages before making deployment or cost decisions if significant time has passed. See §39.4.

**Workload:** 43,200 Lambda invocations/month (1-minute cadence). Source polls = `Σ(1440 / interval_minutes)` per day per source.

| Component | 20 sources | 100 sources | 300 sources | Allowance |
|---|---|---|---|---|
| Lambda invocations | 43,200 | 43,200 | 43,200 | 1 M/mo always-free |
| Lambda GB-s (512 MB) | ~26,000 | ~43,000 | ~65,000 | 400,000/mo always-free |
| EventBridge Scheduler | 43,200 | 43,200 | 43,200 | 14 M/mo, permanent |
| DynamoDB capacity | 18 WCU / 18 RCU | 18 / 18 | ~20 / ~20¹ | 25 WCU / 25 RCU always-free |
| DynamoDB storage | ~50 MB | ~250 MB | ~800 MB | 25 GB always-free |
| DynamoDB PITR | ~$0.01 | ~$0.05 | ~$0.16 | **not free** — $0.20/GB-mo |
| CloudWatch Logs | ~150 MB | ~400 MB | ~1 GB | 5 GB/mo free |
| S3 PUTs | ~3,000 | ~15,000 | ~40,000 | $0.005/1,000 |
| S3 storage | ~200 MB | ~600 MB | ~1.5 GB | $0.023/GB-mo |
| Data transfer out | a few MB | ~20 MB | ~60 MB | 100 GB/mo free |
| Telegram, healthchecks.io | free tier | free tier | free tier | — |
| **Realistic total** | **≈ $0.03/mo** | **≈ $0.15/mo** | **≈ $0.45/mo** | |

¹ Requires the §29 packed-index mitigation to stay inside the free allowance.

**Terminology discipline:** Lambda, EventBridge, and DynamoDB capacity/storage are **inside the free allowance** (genuinely $0). PITR, S3 PUTs, and S3 storage are **small real charges**. The honest total is "**effectively zero — roughly $0.03/month at V1 scale**," not "$0."

### 28.1 Cost footguns — verify every one before and after deployment

| Footgun | Impact | Guard |
|---|---|---|
| **NAT Gateway** (any VPC-attached Lambda) | **~$32/month** — dwarfs everything else here | Lambda stays **out of any VPC**. This is the single strongest reason DynamoDB was chosen over Aurora/RDS. |
| Logging raw response bodies | ~13 GB/mo → ~$4/mo and rising | INV-14; log shape hash and counts only |
| No CloudWatch retention set | default is never-expire; storage accrues forever | set 14 days explicitly at creation |
| **DynamoDB on-demand mode** (the console default) | the 25 WCU/25 RCU allowance applies **only to provisioned mode**; on-demand tables bill from the first request | create with provisioned capacity, verify after any console edit |
| Forgetting GSI capacity | GSIs have separate provisioned throughput that counts against the same allowance | budget 10 + 5 + 3 as in §16.4 |
| **EventBridge Scheduler default 185 retries** | invocation storm; overrides our backoff | `MaximumRetryAttempts: 0` (§11.4) |
| Lambda async default 2 retries | duplicate batch execution | `MaximumRetryAttempts: 0` |
| Unthrottled `raw_latest` PUTs | ~$1/mo at 300 sources | 6-hour throttle (§23) |
| S3 bucket versioning on | unbounded version accumulation | versioning off + lifecycle rule |
| **AWS Free Plan account** | auto-closes after six months or when credits run out</cite> — takes the whole system with it | **use the Paid Plan.** Always-free allowances persist indefinitely on it. |
| Unindexed queries / `Scan` in the hot path | DynamoDB bills on rows *scanned* | never `Scan`; §16.3 patterns only |
| Secrets Manager | $0.40/secret/month | SSM Standard is free and sufficient |

**Configure a $5 billing alarm before deploying anything.**

---

## 29. Scaling triggers

**Do not implement these now.** Each row states the observable that justifies the work.

| Trigger (observable) | Action |
|---|---|
| `ConsumedReadCapacityUnits > 15` sustained | Switch `load_job_index` to a packed index item (`SK = INDEX`, ~50 bytes/job, one item, rewritten only on change: ~4 RCU instead of ~25–37). **Implementation change only — the `Repository` trait does not move.** |
| `count(next_check_at < now - 2 min) > 0` sustained | Scheduler backlog. Raise `claim_limit`, then `concurrency`. |
| Due-rate exceeds one Lambda's throughput (~300 sources) | Shard `GSI1PK` to `DUE#0..3`; fan out claimed sources via SQS to worker invocations. |
| A source returns 403/bot-challenge consistently from AWS | Implement `SourceKind::Push` + external residential fetcher (D14, §19 Case D). |
| A high-value board is JS-only with no JSON endpoint | Browser fetcher behind the same Push seam. |
| ≥3 months of `first_seen_at` data exists | Analyse publication-time distribution; only then consider time-of-day polling (D17). |
| Analysis questions become frequent | Nightly export of `POLL#`/`AGG#` to S3 as NDJSON; query with DuckDB locally. |
| Lambda GB-s exceeds ~40% of the free allowance | Tune memory down, or shorten per-source budgets. |
| Telegram limits are hit routinely | Reassess channel structure; consider a second bot for health. |

---

## 30. Testing strategy

### 30.1 Layers

| Layer | Scope | Runtime | Runs |
|---|---|---|---|
| **Pure unit** | `core`, `errors` | none — sync | every commit, < 1 s |
| **Adapter fixture** | `adapters` against captured bytes | none | every commit |
| **Property / invariant** | `proptest` over diff, event keys, schedule | none | every commit |
| **In-memory engine** | `engine` + `repo_memory` + stubs | tokio | every commit |
| **DynamoDB Local** | `repo_dynamo` | tokio + Docker | every commit in CI |
| **Notification** | `notify_telegram` against a mock server | tokio | every commit |
| **AWS staging** | a second SAM stack, one fake source | real AWS | before each release |
| **Production fault injection** | see §31 | real AWS | at the MVP gate, then quarterly |

### 30.2 Critical regression tests — one or more per invariant

| Test | Defends |
|---|---|
| Repeat `JOB_REPOSTED` for the same job produces **distinct** event keys | INV-2 |
| Replaying the identical transition produces the **same** event key and one durable event | INV-2 |
| Crash injected **between chunk 1 and chunk 2** → replay yields exactly the right event set, no duplicates, no losses | INV-1, INV-2 |
| Crash injected **inside** a transaction → neither the job update nor the event is durable | INV-1 |
| Crash injected **after all chunks, before the commit marker** → replay emits no duplicate events; `next_check_at` unchanged | INV-1, INV-12 |
| Crash injected **after the event transaction, before notification claim** → the sweeper finds and delivers it | **INV-3** |
| Crash injected **after Telegram accepts, before `sent` confirmation** → exactly one duplicate, never a loss | INV-5, D13 |
| `54 → 0` and `54 → 7` responses leave every stored job untouched | INV-4 |
| `54 → 0` with `allow_zero = true` proceeds normally | INV-4 config |
| Telegram returns 429 → events remain open, source stays `HEALTHY`, `SUB#notification` degrades | INV-5, INV-6 |
| S3 PUT fails → poll still succeeds, events still created, `SUB#archive` degrades | INV-6 |
| Two concurrent `run_tick` calls → each source processed exactly once; loser records `LeaseContention`, not an error | INV-7 |
| Stale GSI1 result for a just-polled source → conditional claim rejects it | INV-7 |
| Bootstrap of a 300-job source → **zero** `NEW_JOB` events, one summary; crash mid-bootstrap replays with still zero | INV-10 |
| Sibling field added to a fixture → poll succeeds, `API_CHANGED` emitted | INV-11 |
| Required field removed from a fixture → immediate `SOURCE_FAILED`, state preserved | INV-11 |
| `filter_version` bump → one `FILTER_CHANGED` summary, zero individual `BECAME_RELEVANT` alerts | INV-15 |
| A job reactivating has its `ttl` attribute removed | schema correctness |
| `BatchWriteItem` returning `UnprocessedItems` → loop until empty, no silent loss | data integrity |
| Unchanged present jobs across 100 simulated polls → **zero** job-item writes | INV-12 corollary, §13.8 |
| 40 simultaneous relevant events → one grouped digest, all events eventually `sent` | INV-5, priority 6 |

### 30.3 Crash-injection harness

`repo_memory` and `repo_dynamo` accept a `FailPoint` enum enabling deterministic failure at: `BeforeTransitionTxn`, `AfterChunk(n)`, `BetweenChunks`, `BeforeCommitMarker`, `AfterEventDurableBeforeNotifyClaim`, `AfterTelegramBeforeConfirm`, `DuringBootstrapBatch(n)`. Every test above that mentions a crash uses this harness. **These tests are not optional; they are the reason the system can be trusted.**

---

## 31. MVP acceptance criteria — hard gate

V1 is not "working" until **all** of the following are demonstrated against real AWS. Do not add sources beyond the first four until these pass.

| # | Scenario | Expected |
|---|---|---|
| 1 | Point a source at a 404 URL | `SOURCE_FAILED` in the health chat within one interval, stage `Http`, domain `Upstream`, kind `NotFound` |
| 2 | Deliberately break an adapter's required-field mapping | `SOURCE_FAILED`, stage `Parse`, domain `Adapter`, kind `RequiredFieldMissing`, S3 snapshot key present in the message |
| 3 | Revoke the Lambda's DynamoDB IAM permission | system-level alert **and** healthchecks `/fail` within one tick |
| 4 | Disable the EventBridge schedule | email from healthchecks.io within ~6 minutes |
| 5 | Point `TELEGRAM_API_BASE` at a black hole | events remain open, source stays `HEALTHY`, `NOTIFICATION_DEGRADED` after 5 minutes; restoring it delivers everything |
| 6 | Inject a relevant job into a fixture-backed source | phone notification with a working inline Apply button within one interval |
| 7 | Kill the Lambda at each of the seven `FailPoint`s | no relevant logical event lost, no duplicate logical event created |
| 8 | Invoke two ticks concurrently | each source processed once; no duplicate events |
| 9 | Add a new Greenhouse company via `admin add-source` | polling begins on the next tick with **no deployment** |
| 10 | Return a plausibility-violating response | canonical state preserved; zero `JOB_REMOVED`; `SOURCE_FAILED` raised |
| 11 | Bootstrap a source with ≥100 jobs | one summary message; zero individual alerts; `bootstrap_state = complete` |
| 12 | Add a sibling field to a live response | poll succeeds; `API_CHANGED` emitted once |

---

## 32. Implementation roadmap

### Phase 0 — Toolchain and repository (~½ day)

- **Objective:** a reproducible build and a green CI pipeline.
- **Build:** workspace `Cargo.toml`; `rust-toolchain.toml` pinned; empty crate skeletons; `.github/workflows/ci.yml` running `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`; `cargo-lambda` installed locally; AWS account confirmed on the **Paid Plan**; `$5` billing alarm created.
- **Tests:** CI green on an empty workspace.
- **Acceptance:** a clean clone builds and tests in CI.
- **Do NOT build:** any AWS resources, any SAM template, any business logic.
- **Depends on:** nothing.

### Phase 1 — Pure core (~2 days) — *highest value phase*

- **Objective:** all business logic, fully tested, with zero I/O.
- **Build:** `crates/errors` complete (§9); `crates/core`: `model`, `normalize`, `filter`, `shape`, `plausibility`, `diff`, `event_key`, `schedule`, `health`. **All synchronous.**
- **Tests (write the event-key regression test FIRST):** repeat-transition distinct keys; retry-stable keys; all six job transitions; precedence collapsing (INV-13); plausibility matrix incl. `allow_zero`; probe/backoff/`Retry-After`/jitter bounds; every health-state transition incl. `QUARANTINED`; shape-vs-contract behaviour; `criticality` interval validation.
- **Acceptance:** ~70 tests, sub-second, no network, no mocks, no async.
- **Do NOT build:** any I/O, any trait implementations, any AWS types.
- **Depends on:** Phase 0.

### Phase 2 — Adapter contract and fixtures (~1 day)

- **Objective:** two real adapters proving the trait generalizes.
- **Build:** `crates/adapters`: `JobSource`, `AdapterContract`, `greenhouse.rs`, `lever.rs`, registry; captured real payloads plus mutated variants in `tests/fixtures/`.
- **Tests:** both adapters parse real fixtures; required-field-removed → `RequiredFieldMissing`; HTML body → `WrongMediaType`; truncated body → `ParseFailed`; empty array → contract `min_expected` violated; shape change with valid contract → success + `ShapeChanged`.
- **Acceptance:** adapters are pure functions over bytes; no adapter opens a socket.
- **Do NOT build:** Workday, Ashby, pagination, HTTP.
- **Depends on:** Phase 1.

### Phase 3 — In-memory end-to-end engine (~2 days)

- **Objective:** prove the whole design before it costs money.
- **Build:** `crates/ports` (all traits, §17.1); `crates/engine::run_tick` (§11.3); `infra/repo_memory`, `fetch_stub`, `notify_capture`, `heartbeat_stub`; the `FailPoint` harness (§30.3).
- **Tests:** full tick produces correct events; **all seven crash-injection points**; concurrent `run_tick` lease safety; notification recovery sweep; bootstrap with zero `NEW_JOB`; tick-budget expiry leaves uncommitted sources correctly retryable.
- **Acceptance:** every §30.2 test that does not require DynamoDB passes.
- **Do NOT build:** anything AWS.
- **Depends on:** Phase 2.

### Phase 4 — DynamoDB repository (~2 days)

- **Objective:** the real persistence layer, proven against the *same* test suite.
- **Build:** `infra/repo_dynamo` implementing §13.4 exactly — chunked atomic pairs with `ClientRequestToken`, `BatchWriteItem` `UnprocessedItems` loop, dual-condition claim (§11.3), commit marker with `lease_owner` condition, correlation window with `ReturnValues=ALL_NEW`, hourly rollups. Table + GSIs created via SAM. DynamoDB Local in Docker for CI.
- **Tests:** the entire Phase 3 engine suite passes with only the repository swapped; throttling stub exercises the `UnprocessedItems` loop; `ConditionalCheckFailed` on a transition is treated as success; stale-GSI claim rejection.
- **Acceptance:** identical engine behaviour on both repositories.
- **Do NOT build:** the packed job index (§29). `load_job_index` does a full `Query`.
- **Depends on:** Phase 3.

### Phase 5 — Telegram notification (~1 day)

- **Objective:** real alerts on a real phone, with real rate-limit behaviour.
- **Build:** `infra/notify_telegram`; per-chat and global token buckets (§15.1); `retry_after` global parking; health-before-jobs ordering; per-source and per-tick caps with digest fallback; message templates for job alerts, `SOURCE_FAILED`, `SOURCE_RECOVERED`, `QUARANTINED`, daily digest.
- **Tests:** mock server returns 429 → event stays open, source health unaffected; 40 simultaneous events → one digest, all eventually `sent`; health message dequeued before job messages.
- **Acceptance:** a real message arrives on the phone with a working Apply button.
- **Do NOT build:** the daily digest scheduler (Phase 7).
- **Depends on:** Phase 4.

### Phase 6 — AWS deployment (~1 day)

- **Objective:** running in production against one real source.
- **Build:** `bin/lambda`; SAM template — ARM64, 512 MB, 60 s, reserved concurrency 3, **Scheduler `MaximumRetryAttempts: 0`**, Lambda async retries 0, scoped IAM, SSM parameters, log retention 14 days, S3 bucket with versioning off + lifecycle; `bin/admin` `add-source` with validation and a live probe.
- **Tests:** `sam deploy` from a clean checkout; one real source polls successfully for 24 hours; CloudWatch shows structured lines and **no raw bodies**.
- **Acceptance:** deployed, running every minute, logs clean, cost dashboard flat.
- **Do NOT build:** health alerting (Phase 7).
- **Depends on:** Phase 5.

### Phase 7 — Health and dead-man's-switch (~1 day) — *the real gate*

- **Objective:** the system can report its own failures, and its own death is externally visible.
- **Build:** healthchecks.io check (period 1 m, grace 5 m); `heartbeat_http` with success/`/fail` semantics; correlation evaluation and `SYSTEM_DEGRADED` suppression; alert throttling and re-alert suppression; daily digest; `SUB#` subsystem health records.
- **Tests:** **MVP acceptance criteria 1–5 and 12 from §31 must pass against real AWS.**
- **Acceptance:** all of §31 passes. **This is the hard gate — do not proceed to Phase 8 until it does.**
- **Do NOT build:** dashboards, analytics.
- **Depends on:** Phase 6.

### Phase 8 — Real sources (ongoing)

- **Objective:** four real sources running for a week; thresholds tuned from evidence.
- **Build:** Ashby adapter; one custom/in-house adapter; source registrations (§33).
- **Tests:** §31 criteria 6, 9, 10, 11 against real sources.
- **Acceptance:** four sources `HEALTHY` for seven consecutive days; plausibility thresholds tuned from real `POLL#` records; at least one genuine job alert received.
- **Do NOT build:** anything from §29.
- **Depends on:** Phase 7.

### Phase 9 — Trigger-based hardening (not scheduled)

Execute rows from §29 **only when their observable fires.** Nothing in Phase 9 is on a calendar.

---

## 33. Initial integration sources

> ⚠️ **ATS assignments and endpoint shapes are externally volatile.** Companies migrate between ATS vendors without notice. **Must reverify the current careers backend before implementing any adapter or registering any source.** Open DevTools → Network → XHR on the live careers page and confirm the actual endpoint.

**The principle matters more than any company name: begin with one easy contract, prove adapter-family generality, then test a custom source, then add harder boards.**

| # | Role in the plan | Candidate (reverify) | Why this position |
|---|---|---|---|
| 1 | Clean, well-documented contract | **Cohere** — likely Greenhouse (`boards-api.greenhouse.io/v1/boards/<token>/jobs`) | Simplest possible envelope, stable, and genuinely on the owner's target list. Proves the happy path end to end. |
| 2 | Second adapter family | **An Ashby-hosted AI startup** (`api.ashbyhq.com/posting-api/job-board/<name>`) | Different envelope and location representation. Forces the `JobSource` trait to generalize *before* ten things are built around Greenhouse's shape. Also the family most AI startups use. |
| 3 | Custom / in-house, deliberately hard | **Shopify** — in-house board, likely paginated | The source that actually validates the error taxonomy, the plausibility gate, and the diagnostic messages. Added **third, not first**: prove the pipeline before pointing it at the hard case. |
| 4 | Third parse shape | **A Lever-hosted company** (`api.lever.co/v0/postings/<co>?mode=json`) | Flat array with no envelope. Confirms the registry pattern holds across three structurally different families. |

**Deliberately deferred:** **Workday** (POST-based, session/cookie-dependent, paginated — a project in itself and the wrong fight during bootstrap) and **Microsoft** (in-house, and Canadian campus requisitions frequently route through a separate university-recruiting flow — investigate manually before automating).

---

## 34. Operational runbook

| Situation | Procedure |
|---|---|
| **Source failed** | Read the alert: stage + domain + kind identify the fault. `Adapter` domain → fetch the S3 snapshot, diff against `raw_latest`, add the snapshot as a fixture, write a failing test, fix the parser, bump `adapter_version`, deploy. `Upstream` domain → check the endpoint manually; if the ATS changed, re-register with the new `adapter_type`. |
| **Source quarantined** | The company likely deleted or migrated the board. Verify manually. Either re-register against the new backend or `admin disable-source`. Never leave it quarantined and forgotten — it appears in every daily digest until resolved. |
| **Schema changed (`API_CHANGED`)** | Informational. No action unless followed by a contract failure. If the new field is useful, extend normalization and bump `adapter_version`. |
| **Telegram failing** | Events are safe and queued. Check the bot token in SSM and BotFather status. Check for a global 429 park. Events drain automatically on recovery; `NOTIFICATION_RECOVERED` confirms it. |
| **DynamoDB failing** | Check the IAM role first (`DbAccessDenied` is almost always a policy change). Then throttling — inspect consumed vs provisioned capacity; §29 may have been triggered. |
| **Heartbeat missing** | Check, in order: EventBridge schedule enabled → Lambda function exists and is not throttled → reserved concurrency not exhausted → AWS account status → billing. |
| **AWS bill rises unexpectedly** | Work §28.1 top to bottom. VPC/NAT and CloudWatch ingestion account for nearly all realistic surprises. |
| **New company added** | Watch the first three polls. Confirm bootstrap summary arrived, `health_state = HEALTHY`, and `last_job_count` looks sane. Tune `plausibility.min_ratio` after a week of `POLL#` data. |
| **Adapter upgraded** | Bump `adapter_version`, deploy, confirm a snapshot is written on the next poll of each affected source, verify job counts are unchanged. |
| **Filter changed** | Bump `filter_version` in `SYS#CONFIG/FILTER`. Expect exactly one `FILTER_CHANGED` summary. If individual `BECAME_RELEVANT` alerts appear instead, INV-15 is broken — stop and fix. |

---

## 35. Deferred features and explicit non-goals

| Deferred | Why | Trigger to reconsider |
|---|---|---|
| Time-of-day polling multipliers | No evidence; and adaptive polling in V1 would bias the very dataset needed to design it (D17) | ≥3 months of `first_seen_at` data |
| Seasonal multipliers | Same | Same |
| Dashboard / web UI | The Telegram history *is* the dashboard | Never for V1 |
| Browser fetcher | Nothing needs it yet | A high-value board is JS-only |
| Push source implementation | Seam exists (D14); implementation is not needed | A source blocks AWS's ASN |
| Packed job index | Premature; V1 read costs are trivial | `ConsumedReadCapacityUnits > 15` sustained |
| S3 + DuckDB analytics | Nothing to analyse yet | Analysis questions become frequent |
| Workday adapter | Disproportionate effort during bootstrap | A must-have company uses only Workday |
| Cross-source deduplication | Ignoring a duplicate alert is cheap | Duplicates become frequent enough to be noise |
| ML relevance ranking | The keyword filter is sufficient and auditable | False-negative rate becomes measurable and material |
| Automated application submission | Out of scope; adverse to the owner's interests | Never |
| Exact percentile telemetry | Histogram buckets suffice | Offline analysis needs precision |
| Second heartbeat provider | One is adequate | The healthchecks.io single point of failure becomes unacceptable |

---

## 36. Known risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Undocumented careers APIs change without notice | **High** | A source silently stops working | Adapter contracts + shape hashing + immediate first-failure alerts + S3 snapshots at the moment of change |
| AWS/datacenter ASN blocked by a target | Medium | That source becomes unreachable | Push seam (D14); residential fetcher is a known, designed path |
| ATS pagination handled incorrectly | Medium | Partial results look like mass job removal | Plausibility gate (INV-4) prevents state corruption; the failure is loud, not silent |
| Rate limiting (429) | Medium | Delayed polling | `Retry-After` honoured exactly; `DEGRADED` not `FAILED`; no probe |
| Telegram outage | Low-Medium | Delayed alerts | Events persist as `unsent`; automatic recovery; `NOTIFICATION_DEGRADED` also pings healthchecks `/fail` because Telegram itself may be the broken thing |
| healthchecks.io outage | Low | Watchdog silently unable to alert | Accepted residual risk for V1; §35 lists a second provider as the mitigation if it matters |
| DynamoDB capacity exceeded | Medium at scale | Throttling, delayed polls | Capacity budget (§16.4) + §29 trigger + `DbThrottled` alerting |
| Plausibility thresholds set wrong (too tight) | Medium | False `SOURCE_FAILED` alerts | Per-source config; tune from real `POLL#` data after a week |
| Plausibility thresholds set wrong (too loose) | Low | A partial response corrupts state | `min_abs` floor + `allow_zero` defaulting to `false` |
| Filter false negatives (a relevant job not matched) | **Medium** | **Direct violation of priority 1, and invisible** | Weekly manual spot-check of a source's full job list against the filter during Phase 8; expand keyword lists deliberately with `filter_version` bumps |
| Stale configuration (a company migrates ATS) | Medium | Source fails, or worse, returns an empty valid response | Plausibility gate catches the empty case; quarantine forces manual review |
| AWS free-tier or pricing changes | Medium over a year | Unexpected charges | §28 marked volatile; billing alarm; §39.4 reverification protocol |
| Owner stops trusting alerts due to noise | Medium | **Silently defeats the entire system** | Alert throttling, `QUARANTINED`, `SYSTEM_DEGRADED` coalescing, digest caps — noise reduction is a *reliability* feature here, not a comfort feature |

---

## 37. Decision history

Recorded so future AIs understand why seemingly obvious alternatives were rejected, and do not regress to them.

| Superseded approach | Replaced by | Reason |
|---|---|---|
| HTML/DOM diffing | Structured job state keyed on stable external IDs | SPA DOM churn produces daily false positives; every ATS exposes JSON |
| VPN / IP rotation as a default strategy | Plain AWS egress, with a Push seam as the escape hatch | Consumer VPN and cloud exits are *worse*-reputation datacenter ASNs than a plain AWS IP; rotating location is counterproductive when filtering for Canadian postings |
Cloudflare Workers + D1 (initially preferred for TypeScript)
→ AWS Lambda + DynamoDB

The Rust requirement flipped the decision: workers-rs targets the wasm32-unknown-unknown Workers environment without Tokio and with more restricted crate/runtime compatibility, while AWS Lambda provides a native Linux Rust environment with Tokio, reqwest, cargo test, and production-supported Rust tooling.

| `job.notify_state` (job-level notification flag) | Event-level notification state | One job legitimately produces several alert-worthy transitions; a single flag drops all but the first |
| Content-hash-based deterministic event keys | `transition_seq`-based keys | A content hash collides on genuine repeat transitions and silently drops the second one |
| "Two consecutive failures" at the normal interval | Failure-triggered priority probing | A 30-minute source would take 60 minutes to confirm a failure — explicitly unacceptable |
| "50% of sources in one tick" correlation | 10-minute rolling window with an absolute floor of 3 distinct sources | At 1-minute cadence a typical tick has ~2 due sources, so one failure is 50% |
| Sequential job writes → event writes → commit marker | **Atomic transition/event pairs** (§13) | A crash between the two silently and permanently lost the event, because the advanced job state meant the retry could not re-derive the transition |
| `notify_state` set after the event transaction | `notify_state = "unsent"` set **inside** the transaction | Otherwise a crash between event-durable and notify-claim left the event invisible to the recovery sweeper — the same class of silent loss, one layer down |
| Per-job `last_seen_at` / `absent_ticks` writes each poll | `poll_seq` + sparse `absent_since_poll` | ~864,000 writes/day at V1 scale, and a crashed increment was non-idempotent |
| Multiple events per job per poll | Strict precedence, ≤1 transition per job per poll | `TransactWriteItems` forbids two operations on the same item |
| Bootstrap via the transition protocol | Baseline `BatchWriteItem` + one summary event + `bootstrap_state` marker | 300 jobs × 2 actions × 2× transactional WCU would throttle, and would emit 300 events |
| Chunk size 50 transitions (100 actions) | 25 transitions (50 actions) | 100 actions at 2× transactional capacity throttles a 10-WCU table |
| 20-minute watchdog grace | 5-minute grace, period 1 minute | Owner explicitly prefers occasional false positives over slow discovery of system death |
| Latency `p50` in hourly rollups | Additive counters + fixed histogram buckets | A true p50 cannot be reconstructed from count/sum/max; storing a fake one is worse than storing none |
| "Fast retry fixes detection latency" | `criticality` as a validated ceiling on the interval | Fast retry improves confirmation only; the blind spot is bounded by the full polling interval and nothing else |

---

## 38. Current status

```
ARCHITECTURE STATUS:
    FROZEN FOR V1  (v1.0-architecture-frozen, 2026-08-16)

CURRENT IMPLEMENTATION STATUS:
    NOT STARTED

NEXT PHASE:
    Phase 0 — Toolchain and repository

FIRST ACTION:
    Create the Cargo workspace with crates: errors, core, ports,
    adapters, engine, infra, and bins lambda + admin.
    Pin the Rust toolchain in rust-toolchain.toml.
    Configure CI: cargo fmt --check, cargo clippy -- -D warnings, cargo test.
    Confirm the AWS account is on the PAID PLAN (not the six-month Free Plan).
    Create a $5 billing alarm.

IMMEDIATELY AFTER:
    Phase 1 — write crates/core, and write the event-key repeat-transition
    regression test FIRST, before any other test.

DO NOT:
    Reopen any decision in §4 unless a stated requirement changes or a
    verified platform fact in §28 is invalidated.
    Begin Phase 8 (real sources) before every §31 criterion passes.
```

---

## 39. Instructions for future AI chats

**You are reading a handoff document. Behave as a continuation of an existing engineering project, not as the start of a new brainstorm.**

1. **This document is the current source of truth.** It is self-contained. No prior conversation is needed or available.
2. **Do not restart architecture selection.** §4 lists frozen decisions with rationale, rejected alternatives, and explicit reopening criteria. If you find yourself proposing Postgres, Cloudflare, TypeScript, Discord, or a Raspberry Pi, re-read §4 and §37 first — those were all considered and rejected for stated reasons.
3. **Preserve the reliability invariants in §6 absolutely.** They encode the owner's priority ordering (§2). Every one of them was added because a specific failure mode was found and analysed.
4. **Reverify temporally volatile facts** before deployment or cost decisions: everything in §28, ATS assignments and endpoints in §33, AWS free-tier terms, `workers-rs`/Lambda platform status, and third-party rate limits. All were verified **2026-08-16**. If significant time has passed, search for current values and update this document.
5. **Before implementing a phase, read its acceptance criteria first** (§32), then its tests, then write code.
6. **Do not silently change frozen interfaces or semantics.** If a trait signature, key design, or protocol step in §13/§16/§17 needs to change, say so explicitly and explain why.
7. **If you discover a bug that violates an invariant in §6, surface it immediately and stop.** Do not code around it. Do not defer it to "future work." The last five design reviews each found exactly this class of bug, and each one would have caused a silent missed notification.
8. **Update this document when a decision genuinely changes.** Bump the version in §0, add a row to §37, and update §38.
9. **Always distinguish these four categories** in your reasoning and your output:
   - **Frozen decisions** (§4) — do not reopen
   - **V1 defaults / tunable parameters** (§12, §20) — change with evidence
   - **Externally volatile facts** (§28, §33) — reverify before relying on them
   - **Deferred work** (§29, §35) — do not build until the stated trigger fires
10. **Continue from §38 (`CURRENT STATUS`). Do not invent a new roadmap.**

**One more thing worth knowing about this project.** The owner is a student, not a team, and the system's real enemy is not downtime — it is *plausible-looking silence*. A monitor that appears green while quietly failing is strictly worse than no monitor, because it causes the owner to stop checking manually. Nearly every unusual-looking decision in this document — the atomic transition/event pairs, the `/fail` heartbeat, `QUARANTINED`, `SYSTEM_DEGRADED` coalescing, the refusal to let `criticality` contradict the polling interval — exists to make silence unambiguous. When you face a design choice this document does not cover, resolve it in that direction.

---

*End of `JOB_MONITOR_MASTER_SPEC.md` — v1.0-architecture-frozen — 2026-08-16*
