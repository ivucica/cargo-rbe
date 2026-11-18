#!/usr/bin/env bash
# BASIC_USAGE.sh — cargo-rbe basics, as manual steps you can follow one by one.
#
# Usage: ./BASIC_USAGE.sh [section]     (no argument prints every section)
#
# This script only PRINTS instructions; it never runs anything itself.
# All commands assume the repository root as your working directory and a
# local build first (see the "build" section), so the binaries live in
# target/release/. For CI-downloaded artifacts, substitute their path.
set -euo pipefail

section() { # $1 = name, rest = body lines
  local name=$1; shift
  if [ -z "${WANT:-}" ] || [ "$WANT" = all ] || [ "$name" = "$WANT" ]; then
    printf '\n=== %s ===\n' "$name"
    cat
  fi
}

case "${1:-all}" in
  toolchain|build|bazel-remote-up|smoke-cache-only|wrapper-test-local|bazel-remote-down|\
  buildbuddy-up|smoke-bes-local|wrapper-test-buildbuddy|buildbuddy-down|remote-smoke|e2e)
    WANT=$1 ;;
  all|"") WANT=all ;;
  *) echo "unknown section: $1 (try: ./BASIC_USAGE.sh)" >&2; exit 2 ;;
esac

section toolchain <<'EOF'
Check whether your system Rust is new enough — the project requires >= 1.85:
  rustc --version    # e.g. "rustc 1.85.1 (4eb161250 2025-03-15) (built from a source tarball)"
If it is >= 1.85, there is nothing to install — use the system toolchain as-is.
Otherwise deploy the pinned toolchain (idempotent, sha256-verified; default pin 1.85.1):
  .opencode/skills/rust-toolchain/rust-toolchain.sh deploy
Put it on PATH for your shell (repeat in each new shell):
  eval "$( .opencode/skills/rust-toolchain/rust-toolchain.sh env )"
Verify:
  rustc --version    # expect >= 1.85
EOF

section build <<'EOF'
Requires the toolchain on PATH (see "toolchain"). From the repo root:
  cargo test --locked
  cargo build --release --locked
Binaries land in target/release/cargo-rbe and target/release/cargo-rbe-wrap.
EOF

section bazel-remote-up <<'EOF'
Start a local cache-only REAPI server (bazel-remote; no execution, no secrets):
  case "$(uname -m)" in x86_64) A=amd64 ;; aarch64) A=arm64 ;; *) echo "unsupported arch"; exit 1 ;; esac
  mkdir -p ~/.cache/bazel-remote /tmp/brdata
  BR=~/.cache/bazel-remote/bazel-remote
  if [ ! -x "$BR" ]; then
    curl -fsSL -o "$BR" "https://github.com/buchgr/bazel-remote/releases/download/v2.6.4/bazel-remote-2.6.4-linux-$A"
  fi
  chmod +x "$BR"
  nohup "$BR" --dir /tmp/brdata --max_size 2 --grpc_address 127.0.0.1:9092 --http_address 127.0.0.1:8080 > br.log 2>&1 &
  sleep 3
Cache endpoint: grpc://127.0.0.1:9092 (HTTP UI on http://localhost:8080).
EOF

section smoke-cache-only <<'EOF'
Requires: toolchain on PATH, project built ("build"), bazel-remote running ("bazel-remote-up").
Runs the cache/ByteStream/ActionCache steps against the local server; execution is reported as skipped.
  ./target/release/cargo-rbe smoke --remote_cache=grpc://127.0.0.1:9092 --json-out report.json
Expect every step ok and a populated report.json. Re-running shows cache hits.
EOF

section wrapper-test-local <<'EOF'
D2 acceptance test against the local bazel-remote (Action digests stable across checkouts,
cross-checkout restore). Requires: toolchain on PATH, project built, bazel-remote running.
  BIN_DIR="$PWD/target/release" scripts/two-checkout-test.sh --remote-cache grpc://127.0.0.1:9092
Variant with target directories outside the checkouts:
  EXTERNAL_TARGET=1 BIN_DIR="$PWD/target/release" scripts/two-checkout-test.sh --remote-cache grpc://127.0.0.1:9092
EOF

section bazel-remote-down <<'EOF'
Stop the server started by "bazel-remote-up":
  pkill -f 'bazel-remote --dir /tmp/brdata'
Optional, do it yourself if you want a fresh cache: delete /tmp/brdata.
EOF

section buildbuddy-up <<'EOF'
Start a full local BuildBuddy stack (server + executor + Redis; exercises execution and BES).
Requires redis-server installed (e.g. apt-get install -y redis-server) with its service stopped,
and the toolchain on PATH for later steps. If port 6379 is busy on your machine, set
LBB_REDIS_PORT to a free one (CI uses 6380):
  LBB_REDIS_PORT=6380 scripts/local-buildbuddy.sh start
Binaries are cached under ~/.cache/local-buildbuddy/bin; warm starts take seconds.
EOF

section smoke-bes-local <<'EOF'
Requires: toolchain on PATH, project built, local BuildBuddy running ("buildbuddy-up").
Runs cache + execution + build events against the local stack:
  ./target/release/cargo-rbe smoke --remote_executor=grpc://127.0.0.1:1985 \
    --bes_backend=grpc://127.0.0.1:1985 --remote_timeout=120s \
    --build_metadata=VISIBILITY=PUBLIC \
    --results_url='http://localhost:8080/invocation/{invocation_id}' --json-out report.json
Expect execution ok, "bes: published N events, N acknowledged", and an invocation page at the
printed results URL. Re-running shows cached_result=true on the execute step.
EOF

section wrapper-test-buildbuddy <<'EOF'
D2 acceptance test against the local BuildBuddy cache (same script as "wrapper-test-local"):
  BIN_DIR="$PWD/target/release" scripts/two-checkout-test.sh --remote-cache grpc://127.0.0.1:1985
EOF

section buildbuddy-down <<'EOF'
Stop the stack started by "buildbuddy-up":
  LBB_REDIS_PORT=6380 scripts/local-buildbuddy.sh stop
If something failed, inspect first:
  LBB_REDIS_PORT=6380 scripts/local-buildbuddy.sh logs
EOF

section remote-smoke <<'EOF'
Run against a third-party RBE (e.g. BuildBuddy cloud) with BES for an invocation page.
Requires: toolchain on PATH, project built, and your API key exported in this shell only —
never commit it:
  export RBE_HEADERS='x-buildbuddy-api-key=<your key>'
Then (adjust host/UI to your server; grpcs:// = TLS):
  ./target/release/cargo-rbe smoke \
    --remote_executor=grpcs://<host> \
    --remote_cache=grpcs://<host> \
    --bes_backend=grpcs://<host> \
    --results_url='https://<ui-host>/invocation/{invocation_id}' \
    --build_metadata=VISIBILITY=PUBLIC \
    --remote_header=@env:RBE_HEADERS \
    --json-out report.json
--remote_header=@env:RBE_HEADERS sends every name=value line of that variable as gRPC metadata.
Optional: --nonce=<value> to force cache hits across runs; --remote_instance_name=<name> if your
server uses REAPI instances. The same flags drive the "remote" job in .github/workflows/smoke.yml,
where they come from repository variables (RBE_REMOTE_EXECUTOR et al.) and the RBE_HEADERS secret.
EOF

section e2e <<'EOF'
There is no separate "e2e" CI job; the end-to-end coverage is:
  1. Unit tests:            cargo test --locked        (needs toolchain on PATH)
  2. Cache path, local:     "smoke-cache-only" + "wrapper-test-local" sections above
  3. Execution + BES, local: "smoke-bes-local" + "wrapper-test-buildbuddy" sections above
Run them in that order against whichever stack you have up; each prints PASS/FAIL lines and
writes report.json / wrapper logs for inspection.
EOF
