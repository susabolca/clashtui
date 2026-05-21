#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${CLASHTUI_BIN:-"$ROOT_DIR/target/debug/clashtui"}"
HELPER="${CLASHTUI_TUN_HELPER_BIN:-"$ROOT_DIR/target/debug/clashtui-tun-helper"}"
PLATFORM="$(uname -s)"
TEST_SECONDS="${TUN_TEST_SECONDS:-60}"
PROXY_URL="${CLASHTUI_TEST_PROXY_URL:-http://127.0.0.1:7070}"
TEST_URL="${CLASHTUI_TEST_URL:-https://google.com}"

timer_pid=""
start_ts=0
proxy_host_port="${PROXY_URL#*://}"
proxy_host_port="${proxy_host_port%%/*}"
proxy_host="${proxy_host_port%%:*}"
proxy_port="${proxy_host_port##*:}"

case "$PLATFORM" in
  Darwin)
    INSTALLED_HELPER="/Library/PrivilegedHelperTools/com.clashtui.tun-helper"
    ;;
  Linux)
    INSTALLED_HELPER="/usr/local/libexec/clashtui-tun-helper"
    ;;
  *)
    echo "This guarded TUN test supports macOS and Linux only." >&2
    exit 2
    ;;
esac

need_install() {
  echo "$PLATFORM TUN helper is not installed or is not current." >&2
  echo "Run this, then rerun the test:" >&2
  echo "  $BIN tun-install --path $HELPER" >&2
  exit 2
}

cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  if [[ -n "$timer_pid" ]]; then
    kill "$timer_pid" 2>/dev/null || true
    wait "$timer_pid" 2>/dev/null || true
    timer_pid=""
  fi
  "$BIN" stop >/tmp/clashtui-tun-guarded-stop.log 2>&1 || true
  exit "$status"
}

watchdog() {
  sleep "$((TEST_SECONDS + 10))"
  echo "TUN guarded test timed out after $((TEST_SECONDS + 10))s; stopping clashtui" >&2
  kill -TERM "$$" 2>/dev/null || true
}

wait_for_proxy_port() {
  local deadline=$(( $(date +%s) + 30 ))
  while (( $(date +%s) < deadline )); do
    if nc -z "$proxy_host" "$proxy_port" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "proxy port did not open: $proxy_host:$proxy_port" >&2
  return 1
}

[[ -x "$BIN" ]] || {
  echo "clashtui binary is missing: $BIN" >&2
  echo "Run: cargo build --bins" >&2
  exit 2
}

[[ -x "$HELPER" ]] || {
  echo "clashtui-tun-helper binary is missing: $HELPER" >&2
  echo "Run: cargo build --bins" >&2
  exit 2
}

[[ -x "$INSTALLED_HELPER" ]] || need_install
cmp -s "$HELPER" "$INSTALLED_HELPER" || need_install

trap cleanup EXIT INT TERM HUP
watchdog &
timer_pid=$!
start_ts=$(date +%s)

"$BIN" stop >/tmp/clashtui-tun-guarded-prestop.log 2>&1 || true
if [[ "$PLATFORM" == "Linux" ]]; then
  CLASHTUI_LINUX_TUN_EXPERIMENTAL_ROUTES=1 "$BIN" start
else
  "$BIN" start
fi
wait_for_proxy_port

curl --fail --show-error --head --max-time 15 --proxy "$PROXY_URL" "$TEST_URL"
curl --fail --show-error --head --max-time 15 --noproxy '*' "$TEST_URL"

while (( $(date +%s) - start_ts < TEST_SECONDS )); do
  sleep 1
done
