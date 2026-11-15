#!/usr/bin/env bash
# Local REAPI stack for testing cargo-rbe: BuildBuddy server + executor (+ Redis), no containers.
#
#   scripts/local-buildbuddy.sh start    # download (once), start, wait until the executor registers
#   scripts/local-buildbuddy.sh status
#   scripts/local-buildbuddy.sh stop
#   scripts/local-buildbuddy.sh logs     # tail all logs
#
# Then:  cargo-rbe smoke --remote_executor=grpc://127.0.0.1:1985 \
#          --results_url='http://localhost:8080/invocation/{invocation_id}'
#
# Notes
# - Remote execution needs the *enterprise*-labeled server and executor binaries from the
#   BuildBuddy release; the plain OSS server binary starts but does not implement Execution.
#   Check BuildBuddy's license terms for those binaries before using them in your own CI.
# - Needs `redis-server` on PATH (apt-get install redis-server) and curl. Linux only.
# - Executor runs actions directly on the host ("bare" runner, no isolation): local testing only.
set -euo pipefail

VERSION="${LBB_VERSION:-v2.325.0}"
DIR="${LBB_DIR:-$HOME/.cache/local-buildbuddy}"
BIN="${LBB_BIN_DIR:-$DIR/bin}"
RUN="$DIR/run"
HTTP_PORT="${LBB_HTTP_PORT:-8080}"
GRPC_PORT="${LBB_GRPC_PORT:-1985}"
MON_PORT="${LBB_MON_PORT:-9090}"
REDIS_PORT="${LBB_REDIS_PORT:-6379}"
EXEC_HTTP_PORT="${LBB_EXEC_HTTP_PORT:-8082}"
EXEC_GRPC_PORT="${LBB_EXEC_GRPC_PORT:-1986}"
EXEC_MON_PORT="${LBB_EXEC_MON_PORT:-9092}"
HOST_ID="${LBB_EXECUTOR_HOST_ID:-local-exec-$$}"

case "$(uname -m)" in
  x86_64|amd64) ARCH=amd64 ;;
  aarch64|arm64) ARCH=arm64 ;;
  *) echo "unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac
SERVER_BIN="buildbuddy-enterprise-linux-$ARCH"
EXEC_BIN="executor-enterprise-linux-$ARCH"
BASE="https://github.com/buildbuddy-io/buildbuddy/releases/download/$VERSION"

strip_ansi() { sed 's/\x1b\[[0-9;]*m//g'; }

fetch() {
  mkdir -p "$BIN"
  for f in "$SERVER_BIN" "$EXEC_BIN"; do
    if [ ! -x "$BIN/$f" ]; then
      echo "downloading $f ($VERSION)"
      curl -fsSL -o "$BIN/$f.tmp" "$BASE/$f"
      chmod +x "$BIN/$f.tmp"; mv "$BIN/$f.tmp" "$BIN/$f"
    fi
  done
}

wait_for() { # description, timeout-seconds, command...
  local what="$1" t="$2"; shift 2
  for _ in $(seq 1 "$t"); do "$@" >/dev/null 2>&1 && return 0; sleep 1; done
  echo "timed out waiting for: $what" >&2
  return 1
}

stop_pid() {
  local f="$1"
  [ -f "$f" ] || return 0
  local pid; pid="$(cat "$f")"
  if kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 1 30); do kill -0 "$pid" 2>/dev/null || break; sleep 1; done
    kill -9 "$pid" 2>/dev/null || true
  fi
  rm -f "$f"
}

start() {
  command -v redis-server >/dev/null || { echo "redis-server not found (apt-get install redis-server)" >&2; exit 1; }
  fetch
  stop >/dev/null 2>&1 || true
  rm -rf "$DIR/data" "$DIR/exroot"
  mkdir -p "$RUN" "$DIR/data/storage" "$DIR/data/cache" "$DIR/exroot/cache"

  setsid redis-server --port "$REDIS_PORT" --save "" --appendonly no >"$RUN/redis.log" 2>&1 &
  echo $! >"$RUN/redis.pid"

  setsid "$BIN/$SERVER_BIN" \
    --port="$HTTP_PORT" --grpc_port="$GRPC_PORT" --monitoring_port="$MON_PORT" \
    --database.data_source="sqlite3://$DIR/data/db.sqlite" \
    --storage.disk.root_directory="$DIR/data/storage" \
    --cache.disk.root_directory="$DIR/data/cache" \
    --app.build_buddy_url="http://localhost:$HTTP_PORT" \
    --remote_execution.enable_remote_exec=true \
    --remote_execution.redis_target="localhost:$REDIS_PORT" \
    >"$RUN/server.log" 2>&1 &
  echo $! >"$RUN/server.pid"
  wait_for "server to come up" 90 grep -aq "up and running\|Your BuildBuddy server is running" "$RUN/server.log" \
    || { strip_ansi <"$RUN/server.log" | tail -20; exit 1; }

  # The two parallelism flags avoid an input-download stall seen on a 1-CPU machine
  # (goroutine dump showed a worker pool that never started); harmless on larger machines.
  setsid "$BIN/$EXEC_BIN" \
    --port="$EXEC_HTTP_PORT" --grpc_port="$EXEC_GRPC_PORT" --monitoring_port="$EXEC_MON_PORT" \
    --cache.client.filecache_link_parallelism=4 \
    --cache.client.input_download_concurrency=16 \
    --executor.app_target="grpc://127.0.0.1:$GRPC_PORT" \
    --executor.root_directory="$DIR/exroot" \
    --executor.local_cache_directory="$DIR/exroot/cache" \
    --executor.enable_bare_runner=true \
    --executor.default_isolation_type=none \
    --executor.host_id="$HOST_ID" \
    >"$RUN/executor.log" 2>&1 &
  echo $! >"$RUN/executor.pid"
  wait_for "executor to register" 90 bash -c "sed 's/\x1b\[[0-9;]*m//g' '$RUN/server.log' | grep -aq 'registered executor.*$HOST_ID'" \
    || { echo "--- server"; strip_ansi <"$RUN/server.log" | tail -15; echo "--- executor"; strip_ansi <"$RUN/executor.log" | tail -15; exit 1; }

  cat <<MSG
local BuildBuddy is up (version $VERSION)
  REAPI endpoint : grpc://127.0.0.1:$GRPC_PORT   (cache + execution + BES)
  UI             : http://localhost:$HTTP_PORT/
  results_url    : http://localhost:$HTTP_PORT/invocation/{invocation_id}
  logs           : $RUN/*.log      stop with: $0 stop
MSG
}

stop() {
  for n in executor server redis; do stop_pid "$RUN/$n.pid"; done
}

status() {
  for n in redis server executor; do
    if [ -f "$RUN/$n.pid" ] && kill -0 "$(cat "$RUN/$n.pid")" 2>/dev/null; then echo "$n: running (pid $(cat "$RUN/$n.pid"))"; else echo "$n: not running"; fi
  done
}

case "${1:-}" in
  start) start ;;
  stop) stop ;;
  status) status ;;
  logs) tail -n 50 "$RUN"/*.log | strip_ansi ;;
  *) echo "usage: $0 start|stop|status|logs" >&2; exit 1 ;;
esac
