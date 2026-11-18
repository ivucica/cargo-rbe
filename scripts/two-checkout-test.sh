#!/usr/bin/env bash
# Acceptance test for the cache-only wrapper (D2).
#
#  1. Digest stability: build the sample workspace from two different directories, with different
#     CARGO_HOME and separate wrapper cache dirs, in digest-only mode (no network). Every cacheable
#     unit must get the same Action digest in both checkouts.
#  2. Cache hits (only with --remote-cache URL): build in checkout A in cache mode (publishes),
#     then in a fresh checkout B (empty target dir, other CARGO_HOME): cacheable units must be
#     restored from the cache, and both binaries must print the same output.
#
# usage: scripts/two-checkout-test.sh [--remote-cache grpc://host:port] [extra cargo-rbe flags...]
# env:   BIN_DIR  directory containing cargo-rbe and cargo-rbe-wrap (default: target/release, then target/debug)
#        EXTERNAL_TARGET=1  use a target directory outside each checkout (tests target-root detection)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN_DIR="${BIN_DIR:-}"
if [ -z "$BIN_DIR" ]; then
  for d in "$ROOT/target/release" "$ROOT/target/debug"; do [ -x "$d/cargo-rbe-wrap" ] && BIN_DIR="$d" && break; done
fi
[ -x "$BIN_DIR/cargo-rbe" ] && [ -x "$BIN_DIR/cargo-rbe-wrap" ] || { echo "build first: cargo build --release" >&2; exit 1; }

REMOTE=""
if [ "${1:-}" = "--remote-cache" ]; then REMOTE="$2"; shift 2; fi
EXTRA=("$@")

T="$(mktemp -d)"
[ "${KEEP:-0}" = 1 ] || trap 'rm -rf "$T"' EXIT
echo "work dir: $T" >&2
A="$T/checkout-a/work/dir"; B="$T/elsewhere/checkout-b"
mkdir -p "$A" "$B"
cp -r "$ROOT/examples/sample-ws/." "$A/"
cp -r "$ROOT/examples/sample-ws/." "$B/"

# build DIR NAME MODE HOMEKEY: run cargo through the launcher. CARGO_HOME and the wrapper cache dir
# are per HOMEKEY (a checkout), so repeated builds in one checkout share them; NAME only labels logs.
build() {
  local dir="$1" name="$2" mode="$3" home="$4"; shift 4
  ( cd "$dir"
    export CARGO_HOME="$T/home-$home" CARGO_RBE_CACHE_DIR="$T/cache-$home"
    # The test needs one target directory per checkout. A CARGO_TARGET_DIR inherited from the
    # caller would make both checkouts share one (the second would find units already built).
    # EXTERNAL_TARGET=1 puts each checkout's target directory outside the checkout instead.
    unset CARGO_TARGET_DIR CARGO_BUILD_TARGET_DIR
    [ -n "${EXTERNAL_TARGET:-}" ] && export CARGO_TARGET_DIR="$T/target-$home"
    local flags=("${EXTRA[@]}")
    [ -n "$REMOTE" ] && flags+=(--remote_cache="$REMOTE")
    "$BIN_DIR/cargo-rbe" "${flags[@]}" cargo --mode="$mode" --log="$T/log-$name.jsonl" build -q "$@" 2>"$T/stderr-$name.txt" ) \
    || { echo "build $name failed:"; cat "$T/stderr-$name.txt"; exit 1; }
}

# appbin HOMEKEY CHECKOUT: where cargo put the sample binary
appbin() { if [ -n "${EXTERNAL_TARGET:-}" ]; then echo "$T/target-$1/debug/app"; else echo "$2/target/debug/app"; fi; }

echo "== 1. digest stability (digest-only, no network)"
build "$A" a1 digest-only a1
build "$B" b1 digest-only b1
python3 - "$T/log-a1.jsonl" "$T/log-b1.jsonl" <<'PY'
import json, sys
def load(p):
    return [json.loads(l) for l in open(p) if l.strip()]
a, b = load(sys.argv[1]), load(sys.argv[2])
def units(recs):
    return {(r["crate"], r["unit"]): r for r in recs}
ua, ub = units(a), units(b)
elig = sorted(k for k, r in ua.items() if r.get("eligible"))
print(f"{len(ua)} compile invocations in A, {len(ub)} in B; cacheable in A: {len(elig)}")
bad = 0
for k in sorted(ua):
    ra, rb = ua[k], ub.get(k)
    if rb is None:
        print(f"  MISSING in B: {k}"); bad += 1; continue
    if ra.get("eligible") != rb.get("eligible"):
        print(f"  ELIGIBILITY DIFFERS {k}: {ra.get('reason')} vs {rb.get('reason')}"); bad += 1; continue
    if not ra.get("eligible"):
        print(f"  {k[0]:<12} not cacheable: {ra['reason'][:90]}")
        continue
    same = ra["action_full"] == rb["action_full"]
    print(f"  {k[0]:<12} {'SAME ' if same else 'DIFF '} {ra['action']}  {rb['action']}")
    if not same:
        bad += 1
        for comp in ra["key"]:
            if ra["key"][comp] != rb["key"][comp]:
                print(f"      differs: {comp}\n        A: {ra['key'][comp]}\n        B: {rb['key'][comp]}")
if not elig:
    print("FAIL: no cacheable units found"); sys.exit(1)
if bad:
    print(f"FAIL: {bad} unit(s) differ between checkouts"); sys.exit(1)
print("PASS: identical Action digests across checkouts")
PY

if [ -z "$REMOTE" ]; then
  echo "== 2. skipped (no --remote-cache given)"; exit 0
fi

echo "== 2. cache hits across checkouts (remote cache: $REMOTE)"
A2="$T/second/a"; B2="$T/second/b"; mkdir -p "$A2" "$B2"
cp -r "$ROOT/examples/sample-ws/." "$A2/"; cp -r "$ROOT/examples/sample-ws/." "$B2/"
# A unique line per run: a cache that already holds entries from an earlier run (a long-lived server,
# or a second variant in the same job) must still see a miss for the workspace crate in checkout A.
RUN_ID="$(date +%s%N)-$RANDOM"
for d in "$A2" "$B2"; do echo "// test-run: $RUN_ID" >> "$d/samplelib/src/lib.rs"; done
build "$A2" a2 cache a2
build "$B2" b2 cache b2
python3 - "$T/log-a2.jsonl" "$T/log-b2.jsonl" <<'PY'
import json, sys
def load(p): return [json.loads(l) for l in open(p) if l.strip()]
a, b = load(sys.argv[1]), load(sys.argv[2])
def show(tag, recs):
    for r in recs:
        extra = r.get("publish") or r.get("reason") or "; ".join(r.get("notes") or [])
        print(f"  {tag} {r['crate']:<12} {r['outcome']:<20} {extra[:80]}")
show("A", a); show("B", b)
cacheable_a = [r for r in a if r.get("eligible")]
cacheable_b = [r for r in b if r.get("eligible")]
published = [r for r in a if r["outcome"] == "miss_published"]
hits_a = [r for r in a if r["outcome"] == "hit"]
hits_b = [r for r in b if r["outcome"] == "hit"]
print(f"A: {len(published)} published, {len(hits_a)} already cached, of {len(cacheable_a)} cacheable; B restored {len(hits_b)} of {len(cacheable_b)}")
if not published: print("FAIL: nothing was published in A (the run-unique crate should always miss)"); sys.exit(1)
if len(hits_a) + len(published) != len(cacheable_a): print("FAIL: a cacheable unit in A neither hit nor published"); sys.exit(1)
if len(hits_b) != len(cacheable_b): print("FAIL: B did not restore every cacheable unit"); sys.exit(1)
print("PASS: every cacheable unit in B was restored (published by A or an earlier run)")
PY
OUT_A="$("$(appbin a2 "$A2")")"; OUT_B="$("$(appbin b2 "$B2")")"
echo "  app output A: $OUT_A"; echo "  app output B: $OUT_B"
[ "$OUT_A" = "$OUT_B" ] && [ -n "$OUT_A" ] || { echo "FAIL: binaries differ in output"; exit 1; }
echo "PASS: binaries built from cached artifacts behave identically"

# --- more checks on the second pair of builds
grep -q "unused variable" "$T/stderr-a2.txt" || { echo "FAIL: warning missing in A's output"; exit 1; }
grep -q "unused variable" "$T/stderr-b2.txt" || { echo "FAIL: compiler warning was not replayed on the cache hit in B"; exit 1; }
echo "PASS: compiler warning appears in both builds (replayed from the cache in B)"

build "$B2" b2again cache b2
N=$( [ -f "$T/log-b2again.jsonl" ] && wc -l < "$T/log-b2again.jsonl" || echo 0 )
[ "$N" -eq 0 ] || { echo "FAIL: Cargo re-ran rustc $N time(s) after restoring from the cache (units not considered fresh)"; cat "$T/log-b2again.jsonl"; exit 1; }
echo "PASS: Cargo considers the restored units fresh (no rustc invocations on rebuild)"

echo "== 3. a source edit misses, then another checkout hits"
sed -i 's/GENERATED + 1/GENERATED + 2/' "$B2/samplelib/src/lib.rs"
build "$B2" b3 cache b2
sed -i 's/GENERATED + 1/GENERATED + 2/' "$A2/samplelib/src/lib.rs"
build "$A2" a3 cache a2
python3 - "$T/log-b3.jsonl" "$T/log-a3.jsonl" <<'PY'
import json, sys
def one(p, crate):
    recs = [json.loads(l) for l in open(p) if l.strip()]
    r = [x for x in recs if x["crate"] == crate]
    return r[0]["outcome"] if r else "(not invoked)"
b, a = one(sys.argv[1], "samplelib"), one(sys.argv[2], "samplelib")
print(f"  edited in B: samplelib {b}; same edit in A: samplelib {a}")
if b != "miss_published": print("FAIL: an edited source should miss and publish"); sys.exit(1)
if a != "hit": print("FAIL: the same edit in another checkout should hit"); sys.exit(1)
print("PASS: edit -> miss+publish; same edit elsewhere -> hit")
PY
OUT_A="$("$(appbin a2 "$A2")")"; OUT_B="$("$(appbin b2 "$B2")")"
[ "$OUT_A" = "answer=43 platform=unix" ] && [ "$OUT_B" = "$OUT_A" ] || { echo "FAIL: unexpected output after the edit: A='$OUT_A' B='$OUT_B'"; exit 1; }
echo "PASS: edited code is what runs ($OUT_A)"
