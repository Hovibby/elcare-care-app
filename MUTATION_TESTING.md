# Mutation Testing

Mutation testing measures whether the test suite actually detects dangerous
defects rather than merely executing code. This document describes the
configuration, ownership, acceptable thresholds, and known survivors for the
elcare-care-app Rust contracts.

---

## Tool

[cargo-mutants](https://mutants.rs) — the standard Rust mutation testing tool.

```bash
cargo install cargo-mutants
```

Configuration lives in `mutants.toml` at the repo root.

---

## Quick Start

```bash
# PR mode — only mutates files changed vs origin/main (fast, CI-safe)
bash mutation-tests/run-mutation-tests.sh

# Full scheduled analysis — all high-priority modules
FULL=1 bash mutation-tests/run-mutation-tests.sh

# Single-file targeted run
FILE=contracts/soroban-marketplace/src/math.rs \
  bash mutation-tests/run-mutation-tests.sh
```

Results land in `mutation-tests/reports/` and `cargo-mutants-out/`.

---

## Selected Modules

| Module | Rationale | Owner | Threshold |
|---|---|---|---|
| `contracts/soroban-marketplace/src/math.rs` | Fee arithmetic and dust routing. Any arithmetic operator mutation that survives is a direct financial defect. | @contracts-team | **95%** |
| `contracts/soroban-marketplace/src/contract.rs` | Settlement functions (`buy_artwork`, `finalize_auction`, `accept_offer`), access-control guards (`require_admin`, `require_not_paused`, `require_not_revoked`), and two-step admin rotation. Inverted auth checks are the highest-severity class. | @contracts-team | **80%** |
| `contracts/soroban-marketplace/src/storage.rs` | Index correctness for active-listing swap-removal and reentrancy lock acquire/release. Off-by-one mutations in O(1) swap-remove silently corrupt the active-listing index. | @contracts-team | **85%** |
| `contracts/soroban-marketplace/src/escrow.rs` | NFT custody transitions. A surviving mutant could enable double-listing or asset theft. | @contracts-team | **90%** |
| `contracts/launchpad/src/contract.rs` | Deploy-fee collection and WASM clone address derivation. Fee mutations directly affect protocol revenue. | @contracts-team | **75%** |

---

## Exclusions

The following files are excluded from mutation analysis. Each exclusion has a
documented rationale; all are reviewed quarterly.

| Pattern | Reason |
|---|---|
| `*/test.rs`, `*/tests.rs` | Test code itself; mutating it produces noise rather than actionable coverage signals. |
| `*/lib.rs` | Pure re-export files with no logic. Mutations produce compile errors, not meaningful survivors. |
| `*/events.rs` | Event struct definitions and topic-string constants. String-literal mutations are caught trivially by all callers; the meaningful coverage is exercised by indexer integration tests, not contract unit tests. |
| `*/types.rs` | Data-type definitions (`#[contracterror]`, `#[contracttype]`). Mutating `#[repr(u32)]` discriminants breaks XDR serialisation and is caught by every single test that invokes the contract — producing inflated-but-trivially-detected survivor counts. |
| `patches/**` | Vendored `soroban-env-host`; not project code. |
| `contracts/collection_nft_erc721/**` | Thin delegation contract. No custom business logic today; re-evaluate when custom royalty logic is added. |
| `contracts/collection_nft_erc1155/**` | Same as above. |
| `contracts/lazy_mint_erc721/**` | Thin delegation contract. Deferred until lazy-mint voucher verification logic is added. |
| `contracts/lazy_mint_erc1155/**` | Same as above. |

---

## Acceptable Mutation Score

A module's mutation score is:

```
score = killed / (killed + survived) × 100
```

Timeouts and compile errors are excluded from the denominator (they do not
represent true survivors).

The thresholds in the table above are the **minimum acceptable scores**.
Dropping below threshold on a PR blocks the `mutation-pr` CI job.
The nightly `mutation-full` job tracks score trend over time.

---

## Known Survivors

This section documents mutants that survive but are deemed **semantically
equivalent** — i.e., the mutation produces code that is logically indistinguishable
from the original in all reachable program states.

Semantically equivalent survivors are acceptable and should be listed here to
avoid creating tests that only kill them by coincidence.

### math.rs

| Mutant | Description | Classification |
|---|---|---|
| `calc_fee: replace return 0 with return 1` when `bps == 0 \|\| price == 0` | Both branches short-circuit before multiplication; returning 1 here would be caught by `test_calc_fee_zero_price` and `test_calc_fee_zero_bps`. If this survives, add an explicit assertion in the boundary tests. | **Actionable** — add assertion |

### contract.rs

| Mutant | Description | Classification |
|---|---|---|
| `version()` string constant mutations | The version string is only checked in upgrade scripts, not in unit tests. | **Acceptable** — operational check only; add a unit test asserting `version() == "1.1.0"` if this score matters. |
| View-only getter bodies (e.g., `get_total_listings` returning 0) | View functions are pure reads; their return values are tested end-to-end via integration, not in unit tests. | **Acceptable** — covered by integration tests. |

### storage.rs

_No known semantically equivalent survivors as of baseline._

### escrow.rs

_No known semantically equivalent survivors as of baseline._

### launchpad/contract.rs

_Initial run pending — threshold set conservatively at 75% to account for
currently incomplete test coverage. Survivors from the first full run will be
documented here._

---

## Survivor Triage: keep / fix / retire

A surviving mutant is a decision, not an open-ended backlog item. Every survivor
from a `mutation-pr`, `mutation-full`, or `mutation-file` run gets exactly one
disposition — **keep**, **fix**, or **retire** — recorded with enough evidence
that a different reviewer reaches the same decision from the record alone.

### Reproducible procedure

1. **Collect the inputs** from the run artifact (`mutation-pr-<run_id>` /
   `mutation-full-<run_id>`) or the local output directory:
   - `mutation-tests/reports/survivors-<timestamp>.md` — the survivor list written by `run-mutation-tests.sh`
   - `cargo-mutants-out/outcomes.json` — per-mutant outcome (`killed` / `survived` / `timeout` / `missed`)
2. **Pick one survivor** and copy its `file`, function, and mutant text
   **verbatim** from the report — never paraphrased, because the record must be
   re-matchable against a later run.
3. **Reproduce it on the same test family** with the narrowest matching mode:

   ```bash
   FILE=contracts/soroban-marketplace/src/math.rs bash mutation-tests/run-mutation-tests.sh   # single file
   bash mutation-tests/run-mutation-tests.sh                                                  # PR mode (changed files)
   FULL=1 bash mutation-tests/run-mutation-tests.sh                                           # nightly full run
   ```

   The mutant must still be listed as `survived`. If it cannot be reproduced,
   record **retire** (invalid result) instead of guessing.
4. **Decide** using the dispositions table below: equivalent behaviour → `keep`;
   a test would catch it if it were stronger → `fix`; the result is noise or the
   mutant is not observable at this layer → `retire`.
5. **Record** the decision in the evidence record (template below): §Known Survivors
   for `keep`, a test change for `fix`, a `mutants.toml` change for `retire`.
6. **Re-run** the same command, capture the before/after score, and confirm the
   module still meets its threshold.

### Dispositions and required evidence

| Decision | When it applies | Required evidence (every field mandatory) | Where the evidence lands |
|---|---|---|---|
| **keep** | The mutant is semantically equivalent — behaviour identical to the original in every reachable state | (a) verbatim mutant text + file/function; (b) the equivalence argument, naming the guard or invariant that makes the two forms indistinguishable and covering all reachable inputs; (c) test family run + reproduction command showing `survived`; (d) reviewer + date | Row in §Known Survivors with `Classification: Acceptable (semantically equivalent)` |
| **fix** | The mutant exposes a **weak assertion** or a missing test — a real defect would survive | (a) verbatim mutant text; (b) the concrete input/behaviour a correct test must reject; (c) the assertion added or strengthened **in the same test family**; (d) re-run output showing this mutant moved to `killed` while the module still meets its threshold; (e) reviewer + date | Test change in the same PR + a row in §Known Survivors marked `Actionable → fixed` |
| **retire** | The result is invalid, or the mutant is not observable at this layer — no unit test *should* catch it here | (a) verbatim mutant text; (b) why the signal is invalid (compile artifact, timeout noise, or layer mismatch such as behaviour covered by indexer integration tests rather than contract unit tests); (c) the `mutants.toml` change that removes it (`exclude_globs` / `skip_mutations`) with a rationale comment; (d) owner approval + quarterly review date | `mutants.toml` change + §Exclusions rationale + a row in §Known Survivors marked `Retired` |

A decision missing any of its evidence fields is **not triaged**: the survivor stays
open and continues to count against the module's threshold.

### Evidence record template (copyable)

```markdown
| Mutant (verbatim) | Test family | Decision | Evidence summary | Repro command | Reviewer | Date |
|---|---|---|---|---|---|---|
| `<copy from survivors-<ts>.md>` | `<module + test fns>` | keep / fix / retire | `<for keep: equivalence argument; for fix: input + test changed + before/after score; for retire: invalidity reason + mutants.toml change>` | `FILE=<path> bash mutation-tests/run-mutation-tests.sh` | @<reviewer> | YYYY-MM-DD |
```

### Worked example — `calc_fee` boundary family (`math.rs`)

Real test family: the `calc_fee` tests in
[`contracts/soroban-marketplace/src/math.rs`](contracts/soroban-marketplace/src/math.rs)
— `test_calc_fee_zero_price`, `test_calc_fee_zero_bps`, `test_calc_fee_normal`,
`test_calc_fee_single_stroop_truncates`, `test_calc_fee_max_bps_equals_price`,
`test_calc_fee_near_max_no_overflow`, `test_calc_fee_overflow_returns_zero`.
The mutant below uses the operator from §Mutation Operator Reference (*replace
boolean operator*); when triaging a live run, copy the mutant text verbatim from
`survivors-<timestamp>.md`.

| Field | Value |
|---|---|
| Mutant (verbatim) | `contracts/soroban-marketplace/src/math.rs` — `calc_fee`: replace `\|\|` with `&&` in `if bps == 0 \|\| price == 0` |
| Outcome (why it surfaces) | `survived` — no test in the family can distinguish the two forms, so the suite passes either way |
| Repro command | `FILE=contracts/soroban-marketplace/src/math.rs bash mutation-tests/run-mutation-tests.sh` |
| Decision | **keep** |
| Equivalence argument | With `&&` the guard fires only when *both* inputs are zero; otherwise control falls through to `price.checked_mul(bps).checked_div(10_000).unwrap_or(0)`. When either input is 0 the product is 0, multiplication by zero cannot overflow, and `0 / 10_000 == 0` — so `calc_fee` returns 0 on every input, exactly as the original guard does. No reachable `(price, bps)` pair distinguishes the two forms. |
| Evidence lands | §Known Survivors → `math.rs`, `Classification: Acceptable (semantically equivalent)` |
| Reviewer / date | `@<reviewer>` / `YYYY-MM-DD` |

The same three-way test applies to the family's weak assertion:
`test_calc_fee_near_max_no_overflow` only asserts `fee >= 0 && fee <= price`, so a
mutant whose sole observable effect stays inside that range survives it — that
record is a **fix** (tighten to an exact expected value), not a keep.

**Retire precedent (real):** string-literal mutants inside `emit_*` event
functions were retired through `[[skip_mutations]] source_file_re = "events\.rs$"`
in `mutants.toml`, on the rationale that indexer integration tests — not contract
unit tests — observe those strings. New retire decisions must follow the same
pattern: config change + rationale comment + owner approval.

---

## Recurring Survivor Ownership

When a module's score drops below threshold in the nightly report, the owners
listed in the module table above are responsible for:

1. Reviewing the `mutation-tests/reports/survivors-<timestamp>.md` file.
2. Triaging every survivor as **keep**, **fix**, or **retire** using
   [Survivor Triage](#survivor-triage-keep--fix--retire), with the full evidence
   set for that disposition.
3. Landing the evidence (§Known Survivors row, a test change, or a `mutants.toml`
   change) and re-running the same mode to capture the new score, within
   **5 business days**.

The nightly CI job posts a trend comment on the merge commit to make score
changes visible without requiring direct inspection of the artifact.

---

## CI Integration

| Workflow | File | Trigger | Scope |
|---|---|---|---|
| `mutation-pr` | `.github/workflows/mutation-tests.yml` | PR touching a high-priority module | Changed files only — bounded, non-blocking |
| `mutation-full` | `.github/workflows/mutation-tests.yml` | Nightly 02:00 UTC / manual dispatch | All include_globs from `mutants.toml` |
| `mutation-file` | `.github/workflows/mutation-tests.yml` | Manual dispatch with `mode=file` | Single specified file |

PR mode never runs the full workspace — it only analyses the high-priority
files that were actually modified, so it completes in 5–15 minutes and does not
delay developer feedback.

---

## Trend Report

Score history is maintained in `mutation-tests/reports/trend.json` and committed
by the nightly job. To view the last 5 runs:

```bash
jq '.[-5:] | map({ timestamp: .timestamp, scores: .files | map({ (.file | split("/") | last): .score }) | add })' \
  mutation-tests/reports/trend.json
```

---

## Mutation Operator Reference

`cargo-mutants` applies the following operators by default:

| Operator | Example |
|---|---|
| Replace arithmetic operator | `+` → `-`, `*` → `/`, etc. |
| Replace comparison operator | `<` → `<=`, `==` → `!=`, etc. |
| Replace boolean operator | `&&` → `\|\|` |
| Return early with default value | `return Ok(x)` → `return Ok(Default::default())` |
| Delete statement | Remove a `save_listing()` call |
| Replace integer literal | `10_000` → `0` or `1` |

Operators that produce compile errors (type mismatches, borrow violations) are
automatically filtered by cargo-mutants and do not appear in results.
