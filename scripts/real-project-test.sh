#!/usr/bin/env bash
# Run the cache-only wrapper on a real project, twice, from two different directories.
#
#   scripts/real-project-test.sh GIT_URL REF [cargo-rbe flags...]
#   scripts/real-project-test.sh https://github.com/BurntSushi/ripgrep.git 15.2.0 --remote_cache=grpc://127.0.0.1:9092
#
# Build A (empty target dir, own CARGO_HOME) publishes; build B (another directory, another
# CARGO_HOME, empty target dir) should restore. Prints a report and writes logs to $RESULTS_DIR for
# sharing. The logs contain crate names, digests and paths, no header values.
#
# env: BIN_DIR     directory with cargo-rbe and cargo-rbe-wrap (default target/release, then target/debug)
#      CARGO_ARGS  cargo arguments (default "build"; e.g. "build --release", "test --no-run")
#      RESULTS_DIR where to write results (default ./real-project-out). Not named OUT_DIR on purpose:
#                  Cargo uses that variable for build-script output and rustc would inherit ours.
# Warning: this uploads the project's library artifacts (about 100-150 MB for a small project in the
# debug profile) to the configured cache.
set -euo pipefail
[ $# -ge 2 ] || { sed -n '2,15p' "$0"; exit 1; }
URL="$1"; REF="$2"; shift 2
EXTRA=("$@")
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN_DIR="${BIN_DIR:-}"
if [ -z "$BIN_DIR" ]; then
  for d in "$ROOT/target/release" "$ROOT/target/debug"; do [ -x "$d/cargo-rbe-wrap" ] && BIN_DIR="$d" && break; done
fi
[ -x "$BIN_DIR/cargo-rbe" ] && [ -x "$BIN_DIR/cargo-rbe-wrap" ] || { echo "build first: cargo build --release" >&2; exit 1; }
RESULTS_DIR="$(mkdir -p "${RESULTS_DIR:-./real-project-out}" && cd "${RESULTS_DIR:-./real-project-out}" && pwd)"
unset OUT_DIR
read -r -a CARGO_ARGS <<< "${CARGO_ARGS:-build}"
T="$(mktemp -d)"; [ "${KEEP:-0}" = 1 ] || trap 'rm -rf "$T"' EXIT; [ "${KEEP:-0}" = 1 ] && echo "work dir: $T"
unset CARGO_TARGET_DIR CARGO_BUILD_TARGET_DIR

for X in a b; do
  mkdir -p "$T/$X"
  git clone -q --depth 1 --branch "$REF" "$URL" "$T/$X/proj" 2>/dev/null \
    || { git clone -q "$URL" "$T/$X/proj" && git -C "$T/$X/proj" checkout -q "$REF"; }
done
echo "project: $URL @ $REF   cargo ${CARGO_ARGS[*]}   toolchain: $(rustc --version)"

for X in a b; do
  rm -f "$RESULTS_DIR/log-$X.jsonl"
  S=$(date +%s)
  ( cd "$T/$X/proj"
    export CARGO_HOME="$T/home-$X" CARGO_RBE_CACHE_DIR="$T/cache-$X"
    "$BIN_DIR/cargo-rbe" "${EXTRA[@]}" cargo --mode=cache --log="$RESULTS_DIR/log-$X.jsonl" "${CARGO_ARGS[@]}" ) \
    > "$RESULTS_DIR/build-$X.txt" 2>&1 || { echo "build $X failed; tail of output:"; tail -20 "$RESULTS_DIR/build-$X.txt"; exit 1; }
  echo "build $X: $(( $(date +%s) - S )) s wall"
done

python3 - "$RESULTS_DIR" "$T/b/proj" <<'PY' | tee "$RESULTS_DIR/summary.txt"
import collections, glob, json, os, sys
out, bproj = sys.argv[1], sys.argv[2]
load = lambda p: [json.loads(l) for l in open(p) if l.strip()]
a, b = load(f"{out}/log-a.jsonl"), load(f"{out}/log-b.jsonl")
cnt = lambda recs: dict(collections.Counter(r["outcome"] for r in recs))
print(f"\nA (publishes): {len(a)} rustc invocations {cnt(a)}")
print(f"B (restores) : {len(b)} rustc invocations {cnt(b)}")
published = {(r["crate"], r["unit"]): r for r in a if r["outcome"] in ("miss_published", "hit")}
hits_b = [r for r in b if r["outcome"] == "hit"]
elig_b = [r for r in b if r.get("eligible")]
print(f"B restored {len(hits_b)} of {len(elig_b)} cacheable units; "
      f"{sum(r.get('bytes_down', 0) for r in hits_b) / 1e6:.0f} MB downloaded, "
      f"{sum(r.get('bytes_up', 0) for r in a) / 1e6:.0f} MB uploaded by A")
miss = [r for r in elig_b if r["outcome"] != "hit"]
if miss:
    print("\ncacheable units that did NOT hit in B (same sources, so the key differed):")
    for r in miss:
        ra = published.get((r["crate"], r["unit"]))
        diff = sorted(set(ra["key"]["closure"]) ^ set(r["key"]["closure"])) if ra else ["(unit not published by A)"]
        print(f"  {r['crate']:<24} {r['outcome']:<14} differing dependency files: {[d.split('=')[0] for d in diff][:4]}")
reasons = collections.Counter()
for r in b:
    if r["outcome"] == "local_only": reasons[(r.get("reason") or "")[:95]] += 1
    elif r.get("publish") not in (None, "ok"): reasons["not published: " + str(r["publish"])[:80]] += 1
if reasons:
    print("\nnot cached (B), by reason:")
    for k, n in reasons.most_common(8): print(f"  {n:>3} x {k}")
kinds = collections.Counter(r["crate_type"] for r in b if r["outcome"] == "local_only")
print("  by crate type:", dict(kinds))
# approximate build-script run times from file timestamps in B's target directory
comp, run = {}, {}
for bd in glob.glob(f"{bproj}/target/*/build"):
    for d in os.listdir(bd):
        n, files = d.rsplit("-", 1)[0], os.listdir(f"{bd}/{d}")
        exe = [f for f in files if f.startswith("build_script_build-") and not f.endswith(".d")]
        if exe: comp[n] = os.path.getmtime(f"{bd}/{d}/{exe[0]}")
        if "output" in files:
            t = os.path.getmtime(f"{bd}/{d}/output")
            for rt, _, xs in os.walk(f"{bd}/{d}/out"):
                for x in xs: t = max(t, os.path.getmtime(os.path.join(rt, x)))
            run[n] = t
rows = sorted(((run[n] - comp[n], n) for n in run if n in comp), reverse=True)
if rows:
    print(f"\nbuild scripts executed in B (not cached by this version): {len(rows)}, about {sum(max(0, s) for s, _ in rows):.0f} s total; slowest:")
    for s, n in rows[:4]: print(f"  {n:<26} ~{s:.0f} s")
PY
echo; echo "results in $RESULTS_DIR (summary.txt, log-a.jsonl, log-b.jsonl, build-a.txt, build-b.txt)"
