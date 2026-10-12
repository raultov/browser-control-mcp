#!/usr/bin/env bash
# E2E smoke against real, visible Chrome and Firefox over stdio MCP
# (`make e2e`). Not a quality gate: it needs both browsers and a display.
# Linux and macOS only.
set -uo pipefail

cd "$(dirname "$0")/.."

CHROME_PORT="${CHROME_PORT:-9333}"
FIREFOX_PORT="${FIREFOX_PORT:-9444}"
RDP_PORT="${RDP_PORT:-6100}"
export CHROME_PORT FIREFOX_PORT RDP_PORT

started=()
cleanup() {
  # Only stop the browsers this script started, never ones already open.
  for script in "${started[@]}"; do
    "$script" stop > /dev/null 2>&1 || true
  done
}
trap cleanup EXIT

ensure_running() {
  local script="$1"
  if ! "$script" status > /dev/null 2>&1; then
    "$script" start || return 1
    started+=("$script")
  fi
}

echo "Building..."
cargo build --quiet || exit 1
cargo test --quiet --no-run --test e2e_mcp || exit 1

ensure_running tests/chrome.sh || exit 1
ensure_running tests/firefox.sh || exit 1

export BCM_E2E_ATTACH_CHROME="--port $CHROME_PORT"
export BCM_E2E_ATTACH_FIREFOX="--browser firefox --firefox-port $FIREFOX_PORT --firefox-rdp-port $RDP_PORT"

output=$(cargo test --quiet --test e2e_mcp -- --ignored --nocapture --test-threads=1 2>&1)
status=$?

echo
echo "Summary:"
grep -E '^(PASS|FAIL|SKIP) ' <<< "$output"
pass=$(grep -cE '^PASS ' <<< "$output")
fail=$(grep -cE '^FAIL ' <<< "$output")
skip=$(grep -cE '^SKIP ' <<< "$output")
echo "PASS $pass, FAIL $fail, SKIP $skip"

if [ "$status" -ne 0 ] || [ "$fail" -ne 0 ]; then
  grep -E 'panicked|failed steps' <<< "$output"
  exit 1
fi
