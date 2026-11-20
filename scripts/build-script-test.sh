#!/usr/bin/env bash
# Acceptance test for D3: build scripts through the shim.
#
#   scripts/build-script-test.sh --mode cache  --remote-cache grpc://host:port
#   scripts/build-script-test.sh --mode remote --remote-executor grpc://host:port [--remote-cache URL]
#
# Uses examples/sample-ws: two build scripts, one of which compiles C code with the `cc` crate and
# links it. Checks, per mode:
#  1. checkout A (empty target dir): every build script is published (cache) or executed on the
#     executor (remote); in remote mode the stages show the action really ran there;
#  2. checkout B (another path, another CARGO_HOME): every build script is restored from the cache,
#     including the compiled C library, and the binary prints the same output;
#  3. rebuilding in B is a no-op for Cargo (the shim does not disturb freshness);
#  4. editing the C source reruns the script (miss) in B, and the same edit in A hits;
#  5. with an unreachable server the build still succeeds and the scripts run locally (fallback).
# Extra arguments are passed to cargo-rbe (e.g. --remote_header=@env:RBE_HEADERS).
# env: BIN_DIR (cargo-rbe + cargo-rbe-wrap), BUILD_SCRIPT_PATH (PATH entries for the worker; default: <rustc sysroot>/bin)
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN_DIR="${BIN_DIR:-}"
if [ -z "$BIN_DIR" ]; then
  for d in "$ROOT/target/release" "$ROOT/target/debug"; do [ -x "$d/cargo-rbe-wrap" ] && BIN_DIR="$d" && break; done
fi
[ -x "$BIN_DIR/cargo-rbe" ] && [ -x "$BIN_DIR/cargo-rbe-wrap" ] || { echo "build first: cargo build --release" >&2; exit 1; }

MODE=""; CACHE=""; EXEC=""
while [ $# -gt 0 ]; do
  case "$1" in
    --mode) MODE="$2"; shift 2 ;;
    --remote-cache) CACHE="$2"; shift 2 ;;
    --remote-executor) EXEC="$2"; shift 2 ;;
    *) break ;;
  esac
done
EXTRA=("$@")
[ "$MODE" = cache ] || [ "$MODE" = remote ] || { echo "--mode cache|remote required" >&2; exit 1; }
[ "$MODE" = remote ] && [ -z "$EXEC" ] && { echo "--remote-executor required for remote mode" >&2; exit 1; }
[ -z "$CACHE" ] && CACHE="${EXEC:-}"
[ -n "$CACHE" ] || { echo "--remote-cache required" >&2; exit 1; }
BS_PATH="${BUILD_SCRIPT_PATH:-$(rustc --print sysroot)/bin}"
EXPECT_FIRST=bs_published; [ "$MODE" = remote ] && EXPECT_FIRST=bs_remote

T="$(mktemp -d)"; [ "${KEEP:-0}" = 1 ] && echo "work dir: $T" >&2 || trap 'rm -rf "$T"' EXIT
unset CARGO_TARGET_DIR CARGO_BUILD_TARGET_DIR OUT_DIR
RUN_ID="$(date +%s%N)-$RANDOM"

mk() { # DIR: a copy of the sample with a run-unique line in both build scripts (fresh cache entries)
  mkdir -p "$1"; cp -r "$ROOT/examples/sample-ws/." "$1/"
  for f in samplelib/build.rs nativelib/build.rs; do echo "// test-run: $RUN_ID" >> "$1/$f"; done
}
A="$T/a/dir"; B="$T/elsewhere/b"; C="$T/c"
mk "$A"; mk "$B"; mk "$C"

# build DIR NAME HOMEKEY [cargo-rbe flags before the subcommand]
build() {
  local dir="$1" name="$2" home="$3"; shift 3
  ( cd "$dir"
    export CARGO_HOME="$T/home-$home" CARGO_RBE_CACHE_DIR="$T/cache-$home"
    local flags=("${EXTRA[@]}" --remote_cache="$CACHE" "$@")
    [ -n "$EXEC" ] && flags+=(--remote_executor="$EXEC")
    "$BIN_DIR/cargo-rbe" "${flags[@]}" cargo --mode=cache --build-scripts="$MODE" --build-script-path="$BS_PATH" --log="$T/log-$name.jsonl" build -q 2>"$T/stderr-$name.txt" ) \
    || { echo "build $name failed:"; tail -30 "$T/stderr-$name.txt"; exit 1; }
}
bs_outcomes() { python3 - "$1" <<'PY'
import json, sys
recs = [json.loads(l) for l in open(sys.argv[1]) if l.strip()] if __import__("os").path.exists(sys.argv[1]) else []
print(" ".join(f"{r['crate']}={r['outcome']}" for r in recs if r.get("kind") == "build_script"))
PY
}
check_all() { # LOGFILE EXPECTED_OUTCOME LABEL
  local got; got="$(bs_outcomes "$1")"
  python3 - "$got" "$2" "$3" <<'PY'
import sys
got, want, label = sys.argv[1].split(), sys.argv[2], sys.argv[3]
bad = [g for g in got if g.split("=")[1] != want]
print(f"  {label}: {' '.join(got) or '(no build scripts ran)'}")
if len(got) < 2 or bad:
    print(f"FAIL: expected at least 2 build scripts, all {want}; got {got}"); sys.exit(1)
PY
}
appbin() { echo "$1/target/debug/app"; }

echo "== build scripts, mode: $MODE   cache: $CACHE   executor: ${EXEC:-none}   worker PATH: $BS_PATH"
echo "== 1. checkout A: first build"
build "$A" a a
check_all "$T/log-a.jsonl" "$EXPECT_FIRST" "A"
if [ "$MODE" = remote ]; then
  python3 - "$T/log-a.jsonl" <<'PY'
import json, sys
recs = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
for r in recs:
    if r.get("kind") == "build_script":
        st = r.get("stages") or []
        print(f"    {r['crate']}: stages {st}, worker {r.get('worker')}, uploaded {r.get('inputs_uploaded')}, {r.get('exec_millis')} ms")
        if "EXECUTING" not in st: print("FAIL: no EXECUTING stage; the script did not run on the executor"); sys.exit(1)
PY
fi
echo "PASS: every build script went through the shim ($EXPECT_FIRST)"

echo "== 2. checkout B: restored from the cache"
build "$B" b b
check_all "$T/log-b.jsonl" bs_hit "B"
OUT_A="$("$(appbin "$A")")"; OUT_B="$("$(appbin "$B")")"
echo "  app A: $OUT_A"; echo "  app B: $OUT_B"
[ "$OUT_A" = "answer=42 platform=unix native=7" ] && [ "$OUT_B" = "$OUT_A" ] || { echo "FAIL: unexpected or differing output"; exit 1; }
echo "PASS: all build scripts restored from the cache; the C library built there links and runs"

echo "== 3. Cargo freshness"
build "$B" b-again b
N=$( [ -f "$T/log-b-again.jsonl" ] && wc -l < "$T/log-b-again.jsonl" || echo 0 )
[ "$N" -eq 0 ] || { echo "FAIL: $N tool invocation(s) on a no-op rebuild"; cat "$T/log-b-again.jsonl"; exit 1; }
echo "PASS: a rebuild runs nothing (the shim does not disturb Cargo's freshness check)"

echo "== 4. edit the C source: rerun in B, hit in A"
sed -i 's/return 7;/return 9;/' "$B/nativelib/src/native.c"
build "$B" b-edit b
echo "  B after edit: $(bs_outcomes "$T/log-b-edit.jsonl")"
echo "$(bs_outcomes "$T/log-b-edit.jsonl")" | grep -q "nativelib=$EXPECT_FIRST" || { echo "FAIL: edited script should be $EXPECT_FIRST in B"; exit 1; }
sed -i 's/return 7;/return 9;/' "$A/nativelib/src/native.c"
build "$A" a-edit a
echo "  A after the same edit: $(bs_outcomes "$T/log-a-edit.jsonl")"
echo "$(bs_outcomes "$T/log-a-edit.jsonl")" | grep -q "nativelib=bs_hit" || { echo "FAIL: the same edit in A should hit"; exit 1; }
[ "$("$(appbin "$B")")" = "answer=42 platform=unix native=9" ] && [ "$("$(appbin "$A")")" = "answer=42 platform=unix native=9" ] || { echo "FAIL: edited C code is not what runs"; exit 1; }
echo "PASS: edit -> script reruns; same edit elsewhere -> hit; edited code is what runs"

echo "== 5. fallback with an unreachable server"
( cd "$C"; export CARGO_HOME="$T/home-c" CARGO_RBE_CACHE_DIR="$T/cache-c"
  "$BIN_DIR/cargo-rbe" --remote_cache=grpc://127.0.0.1:9 --remote_executor=grpc://127.0.0.1:9 cargo --mode=cache --build-scripts="$MODE" --log="$T/log-c.jsonl" build -q 2>"$T/stderr-c.txt" ) || { echo "FAIL: the build failed with an unreachable server"; tail -20 "$T/stderr-c.txt"; exit 1; }
check_all "$T/log-c.jsonl" bs_fallback "C"
[ "$("$(appbin "$C")")" = "answer=42 platform=unix native=7" ] || { echo "FAIL: fallback build produced a wrong binary"; exit 1; }
echo "PASS: unreachable server -> scripts ran locally, build succeeded, binary correct"
