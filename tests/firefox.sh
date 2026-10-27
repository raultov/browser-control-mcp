#!/usr/bin/env bash
# Starts/stops a dedicated Firefox for the attach-mode E2E smoke (adapted
# from bidi-lite/tests/firefox.sh). Headless by default; E2E_VISIBLE=1 opens
# a window.
set -euo pipefail

# Snap Firefox only reads profiles under $HOME/snap/firefox/common.
if [ -d "$HOME/snap/firefox/common" ]; then
  DEFAULT_BASE="$HOME/snap/firefox/common/browser-control-mcp"
else
  DEFAULT_BASE="$HOME/.local/share/browser-control-mcp/e2e"
fi
E2E_BASE="${BROWSER_CONTROL_MCP_E2E_HOME:-$DEFAULT_BASE}"
PROFILE_DIR="$E2E_BASE/firefox"
PID_FILE="$E2E_BASE/firefox.pid"
LOG_FILE="$E2E_BASE/firefox.log"

# Distinct from the usual 9222/6000 so the human's own sessions do not clash.
FIREFOX_PORT="${FIREFOX_PORT:-9444}"
RDP_PORT="${RDP_PORT:-6100}"

wait_port() {
  local port="$1" start
  start=$(date +%s)
  until (echo > /dev/tcp/127.0.0.1/"$port") 2>/dev/null; do
    if [ $(($(date +%s) - start)) -ge 20 ]; then
      echo "Timeout waiting for port $port" >&2
      return 1
    fi
    sleep 0.2
  done
}

running_pid() {
  [ -f "$PID_FILE" ] || return 1
  local pid
  pid=$(cat "$PID_FILE")
  kill -0 "$pid" 2>/dev/null && echo "$pid"
}

start_firefox() {
  if pid=$(running_pid); then
    echo "Firefox already running with PID $pid"
    return 0
  fi
  rm -f "$PID_FILE"
  mkdir -p "$PROFILE_DIR"
  # --start-debugger-server needs remote-enabled and chrome.enabled; with
  # prompt-connection=false any local process can drive this browser through
  # the RDP port: this profile is for tests only.
  cat <<'EOF' > "$PROFILE_DIR/user.js"
user_pref("devtools.debugger.remote-enabled", true);
user_pref("devtools.chrome.enabled", true);
user_pref("devtools.debugger.prompt-connection", false);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("app.update.auto", false);
user_pref("app.update.enabled", false);
EOF
  local firefox_bin="${FIREFOX_PATH:-$(command -v firefox || echo /usr/bin/firefox)}"
  local mode=() label="headless"
  if [ "${E2E_VISIBLE:-0}" = 1 ]; then
    label="visible"
  else
    mode=(--headless)
  fi
  echo "Starting Firefox ($label) with BiDi on :$FIREFOX_PORT and RDP on ws:$RDP_PORT..."
  "$firefox_bin" "${mode[@]}" --no-remote --profile "$PROFILE_DIR" \
    --remote-debugging-port="$FIREFOX_PORT" \
    --start-debugger-server "ws:$RDP_PORT" > "$LOG_FILE" 2>&1 &
  local pid=$!
  echo "$pid" > "$PID_FILE"
  if ! wait_port "$FIREFOX_PORT" || ! wait_port "$RDP_PORT"; then
    cat "$LOG_FILE" >&2
    kill -9 "$pid" 2>/dev/null || true
    rm -f "$PID_FILE"
    exit 1
  fi
  echo "Firefox ready (PID $pid)."
}

stop_firefox() {
  local pid
  if ! pid=$(running_pid); then
    rm -f "$PID_FILE"
    echo "Firefox not running."
    return 0
  fi
  echo "Stopping Firefox PID $pid..."
  kill "$pid" 2>/dev/null || true
  local start
  start=$(date +%s)
  while kill -0 "$pid" 2>/dev/null; do
    if [ $(($(date +%s) - start)) -ge 10 ]; then
      kill -9 "$pid" 2>/dev/null || true
      break
    fi
    sleep 0.2
  done
  rm -f "$PID_FILE"
  echo "Firefox stopped."
}

case "${1:-}" in
  start) start_firefox ;;
  stop) stop_firefox ;;
  status) if pid=$(running_pid); then echo "Firefox running with PID $pid"; else echo "Firefox not running"; exit 1; fi ;;
  *) echo "Usage: $0 {start|stop|status}" >&2; exit 1 ;;
esac
