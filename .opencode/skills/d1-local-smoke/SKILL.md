---
name: d1-local-smoke
description: "Use when verifying cargo-rbe D1 locally or collecting the report data Claude asked for (trigger keywords: run smoke test, verify D1, bazel-remote local server, cache-only REAPI test, collect D1 report, d1-info). Records the exact procedure used on 2026-11-15 to build the crate and run `cargo-rbe smoke` against a local bazel-remote cache-only server, where artifacts go (d1-info/ at repo root), and what to configure for BuildBuddy cloud."
---

# D1 local verification (smoke test against a cache-only server)

## Scope

D1 is the smoke-test crate only. The Execute path is OUT of scope for D1 — it needs a
real executor (the D1.1 local BuildBuddy stack or a cloud run). Expected result:
`4 ok, 1 skipped, 0 failed`, exit code 0, with execute reported as skip.

## Prerequisites

- Rust toolchain >= 1.89. Cargo.lock pins uuid@1.29.2 which requires it; the system
  rustc may be older (e.g. 1.85). Install a standalone toolchain into a scratch dir
  instead of modifying the system: fetch the dist tarball from static.rust-lang.org,
  extract, run `./install.sh --destdir=<scratch>/rust-home` — binaries land under
  `<scratch>/rust-home/usr/local/bin`. (The new component-style tarballs have no bare
  bin/ dir; install.sh is required.)
- curl. No redis needed for D1 (that is D1.1).

## Steps

1. Copy the committed tree to a local-disk scratch dir — the repo lives on NFS, so
   building in place is slow and pollutes the tree with target/: `cp -a <repo> <scratch>/cargo-rbe-build`.
2. Build: `<toolchain>/bin/cargo build --release --locked` inside the copy (seconds to a minute). Two non-blocking warnings around an unused `platform` field near src/smoke.rs are expected.
3. Fetch bazel-remote v2.6.4 — the same pin as .github/workflows/smoke.yml:
   `curl -fsSL -o <scratch>/bazel-remote https://github.com/buchgr/bazel-remote/releases/download/v2.6.4/bazel-remote-2.6.4-linux-amd64`
   Verify the sha256 against the GitHub API release digest (assets[].digest of /releases/tags/v2.6.4).
4. Start it detached. CRITICAL: redirect stdin from /dev/null — otherwise the shell
   call hangs on the backgrounded daemon and kills it at timeout:
   `setsid <scratch>/bazel-remote --dir <scratch>/brdata --max_size 2 --grpc_address 127.0.0.1:9092 --http_address 127.0.0.1:8080 > <scratch>/br.log 2>&1 < /dev/null &`
5. Run from the repo root so artifacts land in-repo:
   `./target/release/cargo-rbe smoke --remote_cache=grpc://127.0.0.1:9092 --json-out d1-info/report-cache-only.json`

## Expected output (verified 2026-11-15)

capabilities ok (API 2.0.0–2.3.0, SHA256, ZSTD); cas-batch ok with deduplication
(uploaded N then 0); cas-bytestream ~1 MiB round-trip ok; action-cache-roundtrip
miss→update→hit ok; execute SKIP ("no remote_executor configured"); summary
`4 ok, 1 skipped, 0 failed`; exit code 0.

## Data to collect (what Claude asked for in claude-log-004)

Save under `d1-info/` at the repo root: the report JSON + the printed log (normalize
scratch paths — no absolute paths in repo files), plus written answers to: which
server/version and cloud/local/custom; execute error text if any (a skip is not an
error); whether a results URL was configured and what it showed. See d1-info/notes.md
for the filled-in example from 2026-11-15.

## BuildBuddy cloud (finishing D1)

Repo variables: RBE_REMOTE_EXECUTOR=grpcs://remote.buildbuddy.io,
RBE_RESULTS_URL=https://app.buildbuddy.io/invocation/{invocation_id}; leave
RBE_REMOTE_CACHE / RBE_INSTANCE_NAME / RBE_EXEC_PROPERTIES empty initially. Repo
secret: RBE_HEADERS = one line `x-buildbuddy-api-key=<key>` (key from the BuildBuddy
Quickstart page). After pushing the repo, run Actions → smoke → Run workflow (manual
dispatch) or open a PR; the remote job self-skips on fork PRs. If Execute fails with
platform/toolchain errors, RBE_EXEC_PROPERTIES is the knob (one key=value per line).

## Downloaded artifacts (all hash-verified 2026-11-15)

| Artifact | Size (bytes) | sha256 | Verified against |
|---|---|---|---|
| rust-1.91.1-x86_64-unknown-linux-gnu.tar.gz | 371,298,415 | 1c955c040dd087e4751d15588ddec288b4208bea16f8ec5046c164877e55fff7 | official .sha256 sidecar on static.rust-lang.org — MATCH |
| bazel-remote-2.6.4-linux-amd64 | 24,318,062 | 6a6b64666c2238ec6b49b7b29f5e41e4149de09417a0028897c9bffe40376876 | GitHub API release asset digest — MATCH |
| buildbuddy-enterprise-linux-amd64 (v2.325.0) | 313,224,624 | 2293199e089a469ef5ed849d41055be02281d1979a4e11ae83def28d9fd92144 | GitHub API release asset digest — MATCH |
| executor-enterprise-linux-amd64 (v2.325.0) | 370,593,872 | c9b570c7fae0d241e679ac44ab3e5d432d3ae76a9c5eccab2c531b260621788d | GitHub API release asset digest — MATCH |

Download URLs:
- https://static.rust-lang.org/dist/rust-1.91.1-x86_64-unknown-linux-gnu.tar.gz (sidecar: same URL + `.sha256`)
- https://github.com/buchgr/bazel-remote/releases/download/v2.6.4/bazel-remote-2.6.4-linux-amd64
- https://github.com/buildbuddy-io/buildbuddy/releases/download/v2.325.0/buildbuddy-enterprise-linux-amd64 and .../executor-enterprise-linux-amd64 (fetched by scripts/local-buildbuddy.sh into ~/.cache/local-buildbuddy/bin)

Verification method: `sha256sum` on the local file, compared to the official sidecar
(rust) or the GitHub API release asset digest field (`/releases/tags/<tag>` →
assets[].digest). Always verify before running a downloaded binary.

## Known issue parked for D1.1

scripts/local-buildbuddy.sh (D1.1 artifact) failed to start its stack against the
v2.325.0 enterprise binaries on 2026-11-15: the server's gRPC listener attempted to
bind the monitoring port while --port/--grpc_port appeared ignored; root cause not yet
confirmed (a minimal-flag probe died earlier on "Invalid Remote Execution Redis
config", i.e. remote exec is on by default and needs a valid redis target). Investigate
when D1.1 is executed.
