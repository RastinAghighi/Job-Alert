# Phase 0 runbook — job-monitor

Spec: `JOB_MONITOR_MASTER_SPEC.md` §32 Phase 0.
Acceptance: **a clean clone builds and tests in CI.**

Ground rules:

- Claude Code edits files and installs tools. It never commits, pushes, or creates branches.
- All git operations are run by the owner.
- All AWS console actions are performed by the owner.
- The scaffold is spec-derived. Claude Code must report build failures, not work around them.

Sequence: reconcile tree → toolchain and build → commit and push → CI green → cargo-lambda → AWS.

---

## Reconcile the working tree

Paste into Claude Code after `/clear`:

```
This is the job-monitor project. The repo folder is named Job-Alert. It follows a
frozen engineering spec, JOB_MONITOR_MASTER_SPEC.md, which should be somewhere in this
tree. Read its §32 "Phase 0" and §17 "Rust workspace" before doing anything.

I extracted a scaffold zip into this repo, so the layout may be nested or duplicated.

Do not commit, do not push, do not create branches, do not touch git config. I handle
all git operations myself.

Reconcile the working tree so that the Cargo workspace root IS the repo root.

Start by printing the current tree, excluding .git and target, so I can see what is
actually here.

Then, if the scaffold landed inside a nested subdirectory such as job-monitor/, move
its entire contents up to the repo root — including dotfiles like .gitattributes,
.gitignore and the .github directory, which are easy to miss — and remove the emptied
subdirectory. Delete job-monitor-phase0.zip if it is present. If there are two
README.md files, keep the one containing the heading "# job-monitor" and a section
titled "Deviations from §17", and delete the other.

Do not create, rename, or edit anything else. In particular do not modify Cargo.toml,
rust-toolchain.toml, rustfmt.toml, clippy.toml, any crate manifest, or any .rs file.
Those are derived from a frozen spec.

Finish by verifying and printing:
  - the repo root holds Cargo.toml, rust-toolchain.toml, rustfmt.toml, clippy.toml,
    README.md, JOB_MONITOR_MASTER_SPEC.md, .gitattributes, .gitignore, and
    .github/workflows/ci.yml
  - these directories exist: crates/errors, crates/core, crates/ports, crates/adapters,
    crates/engine, crates/infra, bin/lambda, bin/admin, tests/fixtures
  - the total file count excluding .git and target is exactly 41
  - the output of `git status --short`

If the count is not 41 or anything is missing, tell me what differs. Do not invent or
regenerate files to make the count match.
```

**Gate:** file count is 41 and nothing is reported missing.

---

## Toolchain, lockfile, and the first real build

Paste into Claude Code after `/clear`:

```
Same job-monitor project. Still no commits, no pushes, no branches from you.

Install the pinned toolchain and prove the workspace builds. I am on Windows
PowerShell: use `py` rather than `python`, `py -m pip` rather than `pip`, and never
chain commands with `&&`.

Check whether rustup is installed. If it is not, stop and tell me — I will install it.

If rustup is present, run `rustup show` from the repo root so it installs the toolchain
pinned in rust-toolchain.toml. Then confirm and print each of:
  - `rustc --version` reports 1.97.1
  - `cargo --version` reports the matching cargo
  - `rustup component list --installed` includes rustfmt and clippy
  - `rustup target list --installed` includes aarch64-unknown-linux-gnu

Then run `cargo generate-lockfile` and print the `version =` line from the top of
Cargo.lock along with how many packages it contains.

Then run these four gates in order, printing the full output of each:
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo test --workspace --all-features
  cargo doc --workspace --no-deps

Important: this scaffold has never been compiled on Rust 1.97.1 with edition 2024 and
resolver 3. It was only ever validated against an older toolchain on a different
machine. If any gate fails, STOP and report the exact error verbatim. Do not fix it by
editing Cargo.toml, rust-toolchain.toml, rustfmt.toml, clippy.toml, any crate manifest,
or any .rs file — those come from a frozen spec and I need to approve any change.

Expect zero tests to run. Phase 0's acceptance criterion is "CI green on an empty
workspace", so 0 tests across 14 targets is the correct result, not a problem.

Only if all four gates pass, make one small edit: in .github/workflows/ci.yml, add
`--locked` to the clippy step and the test step, so a stale lockfile fails CI. Show me
the diff and re-run those two commands with `--locked` to confirm they still pass.
```

**Gate:** `rustc 1.97.1`, all four gates pass, `Cargo.lock` exists, ci.yml diff shown.

---

## First commit and push (owner runs these)

Check what will be committed, and confirm the branch name matches GitHub's default:

```powershell
git status --short
git branch --show-current
```

If the branch is `master`, rename it:

```powershell
git branch -M main
```

Confirm `target/` is **not** in `git status`. If it is, `.gitignore` did not land at the
repo root — go back to the reconcile step.

Stage, commit, wire the remote, push:

```powershell
git add -A
git commit -m "Set up the Cargo workspace, pin the Rust toolchain, and add CI"
git remote -v
git remote add origin https://github.com/RastinAghighi/<REPO-NAME>.git
git push -u origin main
```

If the push is rejected because GitHub created the repo with a README or licence:

```powershell
git pull --rebase origin main --allow-unrelated-histories
git push -u origin main
```

Then open the **Actions** tab on GitHub. The `CI` workflow should run `fmt · clippy ·
test · doc` and finish green with 0 tests.

**This green run is the Phase 0 acceptance criterion.**

If CI fails on `--locked`, the fix is to remove that flag from the two steps, push, and
tell me — do not regenerate the lockfile on a different toolchain.

---

## cargo-lambda and Zig

Paste into Claude Code after `/clear`:

```
Same job-monitor project. No commits, no pushes.

Install cargo-lambda and its Zig dependency, then prove ARM64 cross-compilation
actually works. Windows PowerShell: `py` not `python`, `py -m pip` not `pip`, no `&&`
chaining.

First check whether it is already present with `cargo lambda --version`. If it is not,
install it with `cargo install cargo-lambda --locked`. Tell me before you start that
this compiles from source and usually takes more than ten minutes.

cargo-lambda links Linux binaries using Zig, and `cargo install` does not bring Zig with
it. Once cargo-lambda is installed, run `cargo lambda system` and check whether Zig is
detected. If it is not, run `cargo lambda system --install-zig`. If that fails, fall
back to `py -m pip install ziglang` and check `cargo lambda system` again.

Then verify the whole cross-compilation toolchain by building the currently-empty lambda
binary for ARM64:
  cargo lambda build --arm64 -p lambda

Print `cargo lambda --version`, the full `cargo lambda system` output, and the path of
the artifact that was produced.

This proves only that the toolchain works. It does not build any Lambda code.
bin/lambda/src/main.rs stays as `fn main() {}` until Phase 6. Do not add lambda_runtime
or any other dependency, do not write a handler, and do not create a SAM template or any
AWS resource — Phase 0 explicitly forbids all of that.

Build output goes to target/, which is gitignored, so `git status --short` should still
be clean afterwards. Confirm that it is and show me.
```

**Gate:** `cargo lambda build --arm64 -p lambda` succeeds, `git status` clean. Nothing to
commit — cargo-lambda is a global install, not a repo change.

---

## AWS: confirm the account plan

Nothing is deployed in Phase 0. Only two AWS items belong here, and the plan check is the
one that matters most — §28.1 lists it as the footgun that takes the whole system with it.

Sign in and open **Billing and Cost Management** → **Account settings**. Look for whether
the account is on a **Free plan** or a **Paid plan**.

Three possible outcomes:

- **Account created before 15 July 2025** — legacy 12-month model, no automatic closure.
  Nothing to do.
- **Paid plan** — correct. Nothing to do.
- **Free plan** — must be upgraded. A Free plan account expires six months after signup or
  when the $100–$200 of credits run out, whichever comes first. On expiry the account
  closes, running resources are shut down, and after a 90-day grace period everything is
  permanently deleted. Upgrade via **Account settings → Upgrade to Paid Plan**. Remaining
  credits survive the upgrade and stay valid up to twelve months from the original signup
  date. The upgrade is one-way — you cannot go back to Free.

A monitor that dies because the AWS account auto-closed is the exact "plausible-looking
silence" failure the whole spec is built to prevent.

Report the plan type and the account creation date.

---

## AWS: $5 billing alarm

Use **AWS Budgets**, not a CloudWatch alarm. Console → **Billing and Cost Management** →
**Budgets** → **Create budget**.

- Budget type: **Cost budget**
- Period: **Monthly**, recurring
- Budgeted amount: **$5.00 USD**
- Alert threshold: **80% of budgeted amount** (actual, not forecast)
- Notification: **your email address**, not Telegram

Two notes:

- Use email deliberately. §26 keeps the watchdog off Telegram so it does not share a
  failure domain with the primary alert channel; the money alarm deserves the same
  treatment.
- If you use a CloudWatch alarm on `AWS/Billing → EstimatedCharges` instead, it **must be
  created in `us-east-1`**. That metric is published only there, so an alarm created in
  `ca-central-1` can never fire.

Setting up a cost budget is also one of the five $20 AWS onboarding credit activities, so
this may pay for itself.

---

## Phase 0 complete when

- [ ] Workspace root is the repo root, 41 files, `git status` clean
- [ ] `rustc 1.97.1`, aarch64 target installed, `Cargo.lock` committed
- [ ] fmt / clippy / test / doc all pass locally
- [ ] Pushed to GitHub, **CI green**
- [ ] `cargo lambda build --arm64 -p lambda` succeeds
- [ ] AWS account confirmed on Paid plan (or legacy pre-July-2025)
- [ ] $5 monthly budget with email alert

Then Phase 1: `crates/errors` and `crates/core`, everything synchronous, and the
**event-key repeat-transition regression test written first** (§13.2, INV-2) — before any
other test and before the implementation.
