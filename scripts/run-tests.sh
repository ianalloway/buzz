#!/usr/bin/env bash
# =============================================================================
# run-tests.sh — Run Buzz test suite
# =============================================================================
# Usage:
#   ./scripts/run-tests.sh              # run all tests (default)
#   ./scripts/run-tests.sh unit         # unit tests only (no infra needed)
#   ./scripts/run-tests.sh integration  # integration tests only
#   ./scripts/run-tests.sh all          # explicit all
# =============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
MODE="${1:-all}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m'

log()    { echo -e "${BLUE}[run-tests]${NC} $*"; }
success(){ echo -e "${GREEN}[run-tests]${NC} $*"; }
warn()   { echo -e "${YELLOW}[run-tests]${NC} $*"; }
error()  { echo -e "${RED}[run-tests]${NC} $*" >&2; }
section(){ echo -e "\n${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"; echo -e "${CYAN}  $*${NC}"; echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"; }

cd "${REPO_ROOT}"

# ---- Load .env if present ---------------------------------------------------

if [[ -f ".env" ]]; then
  log "Loading .env..."
  set -o allexport
  # shellcheck disable=SC1091
  source .env
  set +o allexport
else
  # Use defaults matching docker-compose.yml
  export DATABASE_URL="postgres://buzz:buzz_dev@localhost:5432/buzz" # sadscan:disable np.postgres.1
  export PGHOST=localhost
  export PGPORT=5432
  export PGUSER=buzz
  export PGPASSWORD=buzz_dev
  export PGDATABASE=buzz
  export REDIS_URL="redis://localhost:6379"
fi

# ---- Track results ----------------------------------------------------------

declare -a PASSED=()
declare -a FAILED=()

run_test_step() {
  local name="$1"
  shift
  log "Running: ${name}"
  if "$@"; then
    success "${name} passed"
    PASSED+=("${name}")
  else
    error "${name} FAILED"
    FAILED+=("${name}")
  fi
}

# ---- Check / start infra (for integration tests) ----------------------------

ensure_infra() {
  "${REPO_ROOT}/bin/just" _ensure-migrations
}

# ---- Unit tests (no infra needed) -------------------------------------------

run_unit_tests() {
  section "Unit Tests (no infra required)"

  run_test_step "buzz-core tests" \
    cargo test -p buzz-core --lib -- --nocapture

  run_test_step "buzz-auth unit tests" \
    cargo test -p buzz-auth --lib -- --nocapture

  # buzz-db migrator/lint unit tests (no infra): guard the embedded-migrator
  # invariant (exactly the consolidated 0001; cutover/backfill stays an operator
  # script, not startup state) and the tenant-scoping lints. The Postgres-backed
  # buzz-db tests are #[ignore]d, so --lib keeps this step infra-free. Those
  # Postgres-backed tests still have no gate: the `Isolated DB Tests`
  # (`db-integration`) job builds its database from schema/schema.sql, which is
  # missing four tables the migrations create, so buzz-db's git_repo tests fail
  # there. See that job's comment.
  #
  # If you run them by hand: build the database from migrations/*.sql, not from
  # schema/schema.sql. And note `setup_db` reads TEST_DATABASE_URL while the
  # product_feedback tests read BUZZ_TEST_DATABASE_URL or DATABASE_URL — both
  # fall back to a hardcoded localhost default, so setting only one sends part
  # of the suite to a different database than you think. Set all three.
  run_test_step "buzz-db unit tests" \
    cargo test -p buzz-db --lib -- --nocapture

  # buzz-audit hash-chain unit tests. The Postgres-backed chain tests are
  # #[ignore]d, so --lib keeps this step infra-free while still covering digest
  # determinism, tenant binding, and the guard that the hashed `created_at` is
  # already at the microsecond precision Postgres stores. Before this step the
  # crate ran in neither mode, so a broken audit round-trip shipped green. The
  # chain tests themselves now run in the `Isolated DB Tests` CI job.
  run_test_step "buzz-audit unit tests" \
    cargo test -p buzz-audit --lib -- --nocapture

  # Multi-tenant conformance gate: independent replay checker + golden
  # fixtures (buzz-conformance). Pure in-process trace replay, no infra.
  run_test_step "buzz-conformance tests" \
    cargo test -p buzz-conformance -- --nocapture

  run_test_step "buzz-push-gateway tests" \
    cargo test -p buzz-push-gateway -- --nocapture
}

# ---- DB / integration tests (infra required) --------------------------------

run_integration_tests() {
  section "Integration Tests (requires running services)"

  ensure_infra

  run_test_step "buzz-db tests" \
    cargo test -p buzz-db -- --nocapture

  if find crates/buzz-auth/tests -maxdepth 1 -name '*.rs' -print -quit 2>/dev/null | grep -q .; then
    run_test_step "buzz-auth integration tests" \
      cargo test -p buzz-auth --test '*' -- --nocapture
  else
    run_test_step "buzz-auth (no integration tests found)" true
  fi

  run_test_step "workspace integration tests" \
    cargo test --test '*' -- --nocapture 2>/dev/null || \
    run_test_step "workspace integration tests (none found)" true
}

# ---- Main -------------------------------------------------------------------

START_TIME=$(date +%s)

case "${MODE}" in
  unit)
    run_unit_tests
    ;;
  integration)
    run_integration_tests
    ;;
  all|*)
    run_unit_tests
    run_integration_tests
    ;;
esac

END_TIME=$(date +%s)
ELAPSED=$((END_TIME - START_TIME))

# ---- Summary ----------------------------------------------------------------

section "Test Summary"
echo ""
echo -e "  Duration: ${ELAPSED}s"
echo ""

if [[ ${#PASSED[@]} -gt 0 ]]; then
  echo -e "  ${GREEN}Passed (${#PASSED[@]}):${NC}"
  for t in "${PASSED[@]}"; do
    echo -e "    ${GREEN}pass${NC} ${t}"
  done
fi

if [[ ${#FAILED[@]} -gt 0 ]]; then
  echo ""
  echo -e "  ${RED}Failed (${#FAILED[@]}):${NC}"
  for t in "${FAILED[@]}"; do
    echo -e "    ${RED}fail${NC} ${t}"
  done
  echo ""
  exit 1
fi

echo ""
success "All tests passed!"
exit 0
