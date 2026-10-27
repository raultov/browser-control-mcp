#!/usr/bin/env bash
# Starts/stops a dedicated Chrome for the attach-mode E2E smoke. Headless
# by default; E2E_VISIBLE=1 opens a window.
set -euo pipefail

E2E_BASE="${BROWSER_CONTROL_MCP_E2E_HOME:-$HOME/.local/share/browser-control-mcp/e2e}"
PROFILE_DIR="$E2E_BASE/chrome"
PID_FILE="$E2E_BASE/chrome.pid"
LOG_FILE="$E2E_BASE/chrome.log"

# Distinct from the usual 9222 so the human's own sessions do not clash.
CHROME_PORT="${CHROME_PORT:-9333}"

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

chrome_bin() {
  if [ -n "${CHROME_PATH:-}" ]; then
    echo "$CHROME_PATH"
    return
  fi
  for candidate in google-chrome google-chrome-stable chromium chromium-browser; do
    if command -v "$candidate" > /dev/null; then
      command -v "$candidate"
      return
    fi
  done
  if [ -x "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" ]; then
    echo "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
    return
  fi
  echo "Chrome not found; set CHROME_PATH" >&2
  exit 1
}

start_chrome() {
  if pid=$(running_pid); then
    echo "Chrome already running with PID $pid"
    return 0
  fi
  rm -f "$PID_FILE"
  mkdir -p "$PROFILE_DIR"
  local bin
  bin=$(chrome_bin)
  local mode=() label="headless"
  if [ "${E2E_VISIBLE:-0}" = 1 ]; then
    label="visible"
  else
    mode=(--headless=new)
  fi
  echo "Starting Chrome ($label) with CDP on :$CHROME_PORT..."
  "$bin" "${mode[@]}" --remote-debugging-port="$CHROME_PORT" --user-data-dir="$PROFILE_DIR" \
    --no-first-run --no-default-browser-check about:blank > "$LOG_FILE" 2>&1 &
  local pid=$!
  echo "$pid" > "$PID_FILE"
  if ! wait_port "$CHROME_PORT"; then
    cat "$LOG_FILE" >&2
    kill -9 "$pid" 2>/dev/null || true
    rm -f "$PID_FILE"
    exit 1
  fi
  echo "Chrome ready (PID $pid)."
}

stop_chrome() {
  local pid
  if ! pid=$(running_pid); then
    rm -f "$PID_FILE"
    echo "Chrome not running."
    return 0
  fi
  echo "Stopping Chrome PID $pid..."
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
  echo "Chrome stopped."
}

case "${1:-}" in
  start) start_chrome ;;
  stop) stop_chrome ;;
  status) if pid=$(running_pid); then echo "Chrome running with PID $pid"; else echo "Chrome not running"; exit 1; fi ;;
  *) echo "Usage: $0 {start|stop|status}" >&2; exit 1 ;;
esac
