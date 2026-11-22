#!/usr/bin/env bash
# mutation-tests/run-mutation-tests.sh
#
# Mutation testing runner for Rust contracts.
#
# Supports two modes:
#
#   PR mode (default):
#     Runs cargo-mutants only on files changed in the current git diff vs the
#     merge-base.  Bounded to prevent blocking developer feedback loops.
#     Fails the build when any HIGH-PRIORITY module drops below its threshold.
#
#   Scheduled full mode (FULL=1):
#     Runs the complete workspace analysis using the include_globs from
#     mutants.toml.  Writes a timestamped report and updates the trend file.
#     Intended for nightly CI; takes ~15–30 min on a 4-core runner.
#
# Usage:
#   bash mutation-tests/run-mutation-tests.sh             # PR mode
#   FULL=1 bash mutation-tests/run-mutation-tests.sh      # Full scheduled mode
#   FILE=contracts/soroban-marketplace/src/math.rs \
#     bash mutation-tests/run-mutation-tests.sh           # Single-file targeted
#
# Prerequisites:
#   cargo install cargo-mutants
#   Rust toolchain with wasm32v1-none target (for workspace build check)
#
# Exit codes:
#   0 — all thresholds met (or no mutations generated for changed files)
#   1 — one or more thresholds violated
#   2 — tool setup error

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
OUT_DIR="${REPO_ROOT}/cargo-mutants-out"
REPORT_DIR="${REPO_ROOT}/mutation-tests/reports"
TREND_FILE="${REPORT_DIR}/trend.json"

mkdir -p "${OUT_DIR}" "${REPORT_DIR}"

FULL="${FULL:-0}"
EXPLICIT_FILE="${FILE:-}"

# ── Threshold map ─────────────────────────────────────────────────────────────
# module_basename → minimum kill percentage (0-100)
declare -A THRESHOLDS
THRESHOLDS["math.rs"]=95
THRESHOLDS["contract.rs"]=80
THRESHOLDS["storage.rs"]=85
THRESHOLDS["escrow.rs"]=90
THRESHOLDS["launchpad_contract.rs"]=75   # launchpad/src/contract.rs → keyed by unique name

# ── Helpers ───────────────────────────────────────────────────────────────────

log()  { echo "[$(date +%H:%M:%S)] $*"; }
fail() { echo "[ERROR] $*" >&2; exit 1; }

require_tool() {
  command -v "$1" >/dev/null 2>&1 || fail "$1 is not installed. $2"
}

require_tool cargo-mutants "Install with: cargo install cargo-mutants"
require_tool jq            "Install with: apt-get install jq / brew install jq"

# ── Determine target files ────────────────────────────────────────────────────

declare -a TARGET_FILES=()

if [[ -n "${EXPLICIT_FILE}" ]]; then
  TARGET_FILES=("${EXPLICIT_FILE}")
  log "Single-file mode: ${EXPLICIT_FILE}"

elif [[ "${FULL}" == "1" ]]; then
  log "Full scheduled mode — using include_globs from mutants.toml"
  # Leave TARGET_FILES empty; cargo-mutants reads include_globs from mutants.toml

else
  # PR mode: find Rust source files changed vs merge-base
  BASE_BRANCH="${BASE_BRANCH:-main}"
  MERGE_BASE=$(git merge-base HEAD "origin/${BASE_BRANCH}" 2>/dev/null || git merge-base HEAD HEAD~5 2>/dev/null || echo "HEAD~1")
  log "PR mode — merge base: ${MERGE_BASE}"

  # High-priority globs that always run when any .rs file in the module changed
  HIGH_PRIORITY=(
    "contracts/soroban-marketplace/src/math.rs"
    "contracts/soroban-marketplace/src/contract.rs"
    "contracts/soroban-marketplace/src/storage.rs"
    "contracts/soroban-marketplace/src/escrow.rs"
    "contracts/launchpad/src/contract.rs"
  )

  CHANGED_RS=$(git diff --name-only "${MERGE_BASE}" HEAD -- '*.rs' 2>/dev/null || true)

  for hp in "${HIGH_PRIORITY[@]}"; do
    if echo "${CHANGED_RS}" | grep -qF "${hp}"; then
      TARGET_FILES+=("${hp}")
    fi
  done

  # Also include any other changed .rs file that isn't excluded
  while IFS= read -r f; do
    [[ -z "${f}" ]] && continue
    # Skip excluded patterns
    if echo "${f}" | grep -qE '(test\.rs|lib\.rs|events\.rs|types\.rs|patches/)'; then
      continue
    fi
    if ! printf '%s\n' "${TARGET_FILES[@]}" | grep -qF "${f}"; then
      TARGET_FILES+=("${f}")
    fi
  done <<< "${CHANGED_RS}"

  if [[ ${#TARGET_FILES[@]} -eq 0 ]]; then
    log "No high-priority Rust files changed — skipping mutation tests"
    exit 0
  fi
  log "PR mode targets: ${TARGET_FILES[*]}"
fi

# ── Build cargo-mutants command ───────────────────────────────────────────────

TS="$(date +%Y%m%d_%H%M%S)"
JSON_OUT="${OUT_DIR}/mutants-${TS}.json"

MUTANTS_ARGS=(
  "--workspace"
  "--output" "${OUT_DIR}"
  "--json"
  "--jobs" "${MUTANTS_JOBS:-4}"
  "--timeout-multiplier" "3"
)

# Add explicit file targets for PR / single-file mode
if [[ ${#TARGET_FILES[@]} -gt 0 ]]; then
  for f in "${TARGET_FILES[@]}"; do
    MUTANTS_ARGS+=("--file" "${f}")
  done
fi

log "Running: cargo mutants ${MUTANTS_ARGS[*]}"
cd "${REPO_ROOT}"
cargo mutants "${MUTANTS_ARGS[@]}" 2>&1 | tee "${OUT_DIR}/run-${TS}.log" || true
# cargo-mutants exits non-zero when survivors exist; we handle that ourselves

# ── Parse results ─────────────────────────────────────────────────────────────

# cargo-mutants writes outcomes.json in the output directory
OUTCOMES="${OUT_DIR}/outcomes.json"
if [[ ! -f "${OUTCOMES}" ]]; then
  log "No outcomes.json found — no mutants were generated"
  exit 0
fi

# Aggregate per-file kill rates using jq
log "Parsing outcomes…"

# outcomes.json schema: array of { "file": "...", "mutant": "...", "outcome": "killed"|"survived"|"timeout"|... }
SUMMARY=$(jq -r '
  group_by(.file) |
  map({
    file: .[0].file,
    total:    length,
    killed:   map(select(.outcome == "killed"))   | length,
    survived: map(select(.outcome == "survived")) | length,
    timeout:  map(select(.outcome == "timeout"))  | length,
    missed:   map(select(.outcome == "missed"))   | length
  }) |
  map(. + { score: (if .total > 0 then (.killed * 100 / .total) else 100 end) })
' "${OUTCOMES}")

log "Per-file mutation scores:"
echo "${SUMMARY}" | jq -r '.[] | "  \(.file): \(.killed)/\(.total) killed (\(.score | floor)%)"'

# ── Threshold enforcement ─────────────────────────────────────────────────────

VIOLATIONS=()

check_threshold() {
  local file_path="$1"
  local score="$2"
  # Normalize key: use basename, but disambiguate launchpad/contract.rs
  local key
  if echo "${file_path}" | grep -q "launchpad"; then
    key="launchpad_contract.rs"
  else
    key="$(basename "${file_path}")"
  fi

  local threshold="${THRESHOLDS[${key}]:-0}"
  if [[ "${threshold}" -eq 0 ]]; then
    return  # no threshold configured for this file
  fi

  local score_int
  score_int=$(echo "${score}" | jq 'floor' 2>/dev/null || echo "0")
  if [[ "${score_int}" -lt "${threshold}" ]]; then
    VIOLATIONS+=("${file_path}: score=${score_int}% < threshold=${threshold}%")
  else
    log "OK  ${file_path}: ${score_int}% >= ${threshold}%"
  fi
}

while IFS= read -r row; do
  file_path=$(echo "${row}" | jq -r '.file')
  score=$(echo "${row}" | jq '.score')
  check_threshold "${file_path}" "${score}"
done < <(echo "${SUMMARY}" | jq -c '.[]')

# ── Survivors report ──────────────────────────────────────────────────────────

SURVIVORS_FILE="${REPORT_DIR}/survivors-${TS}.md"
SURVIVED_ROWS=$(jq '[.[] | select(.outcome == "survived")]' "${OUTCOMES}" 2>/dev/null || echo "[]")
SURVIVED_COUNT=$(echo "${SURVIVED_ROWS}" | jq 'length')

cat > "${SURVIVORS_FILE}" <<SURVIVORS_MD
# Surviving Mutants — ${TS}

> Run mode: ${FULL:+full scheduled}${EXPLICIT_FILE:+single-file}${FULL:-${EXPLICIT_FILE:-PR}}
> Total survivors: ${SURVIVED_COUNT}

${SURVIVED_COUNT} mutant(s) survived this run. Each needs exactly one triage
decision — keep, fix, or retire — with the evidence defined in
MUTATION_TESTING.md §Survivor Triage:

  keep   — semantically equivalent mutant; record the equivalence argument in
           MUTATION_TESTING.md §Known Survivors
  fix    — weak assertion or missing test; add or strengthen a test in the same
           family, then re-run until the mutant is killed
  retire — invalid or unobservable-at-this-layer result; exclude it in
           mutants.toml with a rationale comment and owner approval

## Survivor Details

\`\`\`json
$(echo "${SURVIVED_ROWS}" | jq '.' 2>/dev/null || echo "[]")
\`\`\`
SURVIVORS_MD

log "Survivors report: ${SURVIVORS_FILE}"

# ── Trend tracking ────────────────────────────────────────────────────────────

TREND_ENTRY=$(echo "${SUMMARY}" | jq --arg ts "${TS}" '{
  timestamp: $ts,
  files: map({ file: .file, score: (.score | floor), killed: .killed, total: .total })
}')

# Append to trend array (create file if missing)
if [[ -f "${TREND_FILE}" ]]; then
  jq --argjson entry "${TREND_ENTRY}" '. += [$entry]' "${TREND_FILE}" > "${TREND_FILE}.tmp"
  mv "${TREND_FILE}.tmp" "${TREND_FILE}"
else
  echo "[${TREND_ENTRY}]" > "${TREND_FILE}"
fi
log "Trend updated: ${TREND_FILE}"

# ── Final outcome ─────────────────────────────────────────────────────────────

echo
if [[ ${#VIOLATIONS[@]} -gt 0 ]]; then
  echo "═══ MUTATION TESTING FAILED ══════════════════════════════"
  echo "  ${#VIOLATIONS[@]} threshold violation(s):"
  for v in "${VIOLATIONS[@]}"; do
    echo "  ✗  ${v}"
  done
  echo ""
  echo "  Next steps:"
  echo "  1. Review surviving mutants in: ${SURVIVORS_FILE}"
  echo "  2. Add tests that kill meaningful survivors"
  echo "  3. Document semantically-equivalent survivors in MUTATION_TESTING.md"
  echo "══════════════════════════════════════════════════════════"
  exit 1
else
  echo "═══ MUTATION TESTING PASSED ══════════════════════════════"
  echo "  All configured thresholds met."
  echo "  Survivors report: ${SURVIVORS_FILE}"
  echo "══════════════════════════════════════════════════════════"
  exit 0
fi
