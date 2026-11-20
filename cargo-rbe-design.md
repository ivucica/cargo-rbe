# Cargo + Remote Execution API (REAPI): Design and Implementation Plan

Status: draft for approval. Date: 2026-11-14.
Toolchain used for the experiments below: Cargo/rustc 1.75 (apt, Ubuntu 24.04). Re-run on your target toolchain.

## 1. Summary of the design

Cargo stays the planner and scheduler, and the build plugs in at `RUSTC_WRAPPER`. Every rustc invocation Cargo schedules becomes one REAPI Action, and build scripts get a small shim that does the same. A long-lived local daemon owns the gRPC connections and digest caches. `cargo build` stays the entry point. `--unit-graph` is used only as a prefetch hint, never as the source of truth.

Goals from the request:

- Distribute the build through the Remote Execution API.
- Make efficient use of the CAS.
- Break a project into many concurrent tasks with well-defined inputs and outputs.
- Keep Cargo as the thing that triggers the build.
- Take inspiration from non-Bazel REAPI clients (recc, reninja).
- Stay server-agnostic: any REAPI implementation (cloud or local BuildBuddy, Buildbarn, Buildfarm, NativeLink, custom). Endpoints, headers, TLS, platform properties and results URLs are configuration, never hard-coded.

## 2. The discarded alternative: export the plan (`--build-plan`) and execute it externally

The obvious alternative is to ask Cargo for its plan, then have an external tool execute that plan against REAPI. I discarded it for two reasons.

1. `--build-plan` no longer exists. Cargo removed the unstable feature entirely in 1.93 (changelog, 2026-01-22). The Cargo team points to plumbing commands, `--unit-graph` and structured logging as its replacements.
2. The nearest survivor, `--unit-graph`, carries only the unit DAG and per-unit metadata. A plan-driven tool must therefore reimplement rustc command-line construction: flags, `-C metadata`, `extra-filename`, env vars, link args from build scripts. That duplicates Cargo's logic and drifts from it.

The wrapper route lets Cargo keep resolving features, env and flags. It is also the stance cargo-reapi takes: Cargo remains the planner, and the wrapper observes the exact compiler commands Cargo schedules. An early Cargo issue (rust-lang/cargo#7468) argued that this feature will eventually need to live in Cargo proper because it affects how many jobs can run in parallel. That is the upstream path in the rollout (section 13).

`--unit-graph` is still useful, but as a hint only. Prefetch uses it, together with `cargo metadata`, to pre-upload the toolchain and registry trees before the first action needs them.

## 3. What the prior art settles

- **reninja** (BuildBuddy) sidesteps absolute-path problems by changing the remote container's working directory so CMake's absolute paths stay valid. Its maintainer advises running cmake inside the same container used for remote execution (see the reninja issue #22 comment; project root can be set explicitly when no `.git` is found). It also solves undeclared inputs with a two-stage cache lookup: explicit inputs map to a discovered list of implicit deps, and those deps' hashes map to the output. Rust has the same problem, because modules and `include_str!` files are not declared up front.
- **recc** (BuildGrid) is the right pattern, a wrapper client for REAPI, but the project is marked obsolete and merged into buildbox. Borrow the idea, not the code.
- **cargo-reapi** is the closest Rust analogue and a useful warning. Its qualified public result is still a shared local cache. Remote link actions are ineligible until the native input set can be represented. Its REAPI adapter (via reclient's rewrapper) has not been validated against a live service. Its dogfooding also showed that ignoring more env vars raises hit rates while opening a route to stale outputs.
- **rules_rust and Buck2** hit the pipelining problem. Bazel cannot unblock dependents mid-action, so metadata and full compilation need two compiler invocations. `--emit=metadata` and `--emit=metadata,link` also yield different `.rmeta` files. Newer rules_rust work uses `-Zno-codegen` hollow rlibs, as Buck2 does, which needs `RUSTC_BOOTSTRAP=1` on stable.

## 4. What was verified

I built a small workspace (path deps, a proc macro, a build script, one registry dependency `cfg-if`) from two different checkout paths with a logging `RUSTC_WRAPPER`. Cargo/rustc 1.75.

- After rewriting only the checkout root, the invocations were identical. The only differences were the jobserver fds inside `CARGO_MAKEFLAGS` (must be excluded from the env) and scheduling order. `-C metadata` and `extra-filename` matched across checkouts, so canonical Action digests are feasible.
- Cargo passes absolute paths in `--out-dir`, `-L dependency=`, `--extern`, `-C incremental=`, `CARGO_MANIFEST_DIR` and `OUT_DIR`. Workspace members get relative source paths and registry crates get absolute ones. For workspace members the cwd is the workspace root. For registry crates it is the crate directory.
- Library deps arrive as `.rmeta` (pipelining) and proc macros as `.so`.
- Flags seen: `--error-format=json`, `--json=diagnostic-rendered-ansi,artifacts,future-incompat`, `--emit=dep-info,metadata,link`.
- dep-info lists the files rustc actually read, including the generated file in `OUT_DIR`. It also records `# env-dep:` lines, including an absolute `CARGO_MANIFEST_DIR` when `env!` is used. That is both the hook for a two-stage lookup and the hazard for path canonicalization.

Not yet verified: SVH equality between metadata-only and full compilation; behavior on current Cargo; Server-specific behavior (section 15).

## 5. Architecture

```
cargo build -j128                      (user-facing, unchanged)
   |  RUSTC_WRAPPER=cargo-rbe-wrap
   v
wrapper (thin) --unix socket--> cargo-rbe daemon --gRPC--> CAS / ActionCache / Execution
   | ineligible?                  - digest index: unit outputs -> Digest
   v                              - stat cache: (path,ino,mtime,size) -> Digest
 real rustc (local)               - in-flight dedup, upload batching
build.rs binary = shim ---------> same daemon (remote-runs the real build script)

prefetch: cargo build --unit-graph + cargo metadata -> daemon pre-uploads toolchain + registry trees
```

The wrapper is just a process spawn per unit, so all state lives in the daemon. Cargo's `-j` still bounds concurrency. For remote builds set it far above the core count, since wrapper processes mostly wait.

## 6. Documentation and user-facing configuration

Users run `cargo build` as before. Nothing in the tool is tied to one service. Endpoints, headers, TLS, instance name, platform properties and the results URL are all configuration, because the target may be BuildBuddy (cloud or a local copy), Buildbarn, Buildfarm, NativeLink, a custom CAS plus executor, or something else.

Bazel's flag names are reused where they map cleanly, so an existing `.bazelrc` is easy to translate. We do not import Bazel's toolchain and platform selection flags (`--host_platform`, `--platforms`, `--extra_execution_platforms`, `--crosstool_top`, `--javabase`, `--define` ...). Those select Bazel toolchains. The equivalent here is plain execution properties set on the Action, plus the toolchain declared in the input root or the worker image.

### Flags

Every flag can also be set in config files. Precedence, highest first: CLI flag, environment (`CARGO_RBE_*`), project config, user config.

| Flag | Meaning |
|---|---|
| `--config=<name>` | Apply the named config section (like Bazel's `build:<name>`). `[common]` always applies. |
| `--remote_executor=<url>` | Execution endpoint. `grpcs://` is TLS, `grpc://` is plaintext (for local servers). |
| `--remote_cache=<url>` | CAS and ActionCache endpoint. Defaults to the executor if unset. |
| `--remote_instance_name=<s>` | REAPI instance name. |
| `--remote_header=<name>=<value>` | Repeatable. Sent as gRPC metadata on every call. Any name, any value. |
| `--remote_timeout=<dur>` | Per-action and per-RPC timeout. |
| `--remote_default_exec_properties=<k>=<v>` | Repeatable. Platform properties placed on every Action. Names are defined by the server. |
| `--tls_certificate`, `--tls_client_certificate`, `--tls_client_key` | For self-signed or mTLS servers. |
| `--remote_download=<minimal\|toplevel\|all>` | Output materialization policy. |
| `--noremote_upload_local_results` | Read-only cache mode (like Bazel). |
| `--bes_backend=<url>` | Optional. Build Event Stream endpoint, only for services that have one. |
| `--bes_header=<name>=<value>` | Repeatable. Extra headers for the BES connection only; BES also receives every `--remote_header`. |
| `--build_metadata=<KEY>=<VALUE>` | Repeatable. Published in the BuildMetadata event (for example `VISIBILITY=PUBLIC`). |
| `--results_url=<template>` | Optional. URL template, for example `https://ui.example/invocation/{invocation_id}`. |
| `-j`, `--jobs` | Passed through to Cargo. |

### Headers and secrets

- Headers are generic `name=value` pairs, repeatable, and accumulate across config files (as repeated Bazel flags do). Names are lowercased for gRPC. Values are never logged and are redacted in the action log.
- A value may reference its source instead of containing the secret: `@env:NAME` reads an environment variable and `@file:path` reads a file. That suits CI, where the secret arrives as an environment variable.
- A config file may import another with `import = ["?path"]`, where `?` means ignore if missing. This mirrors `try-import %workspace%/.bazelrc.buildbuddysecrets`, so a gitignored secrets file can hold the key.
- The same headers are sent to the REAPI services and, if configured, the BES backend. Header content is never assumed to be an API key. It may be a bearer token, a custom auth header, or an instance selector.

### Config file

Cargo reads `.cargo/config.toml`. cargo-rbe reads its own `.cargo-rbe.toml`, checked in and free of secrets:

```toml
# .cargo/config.toml
[build]
rustc-wrapper = "cargo-rbe-wrap"
jobs = 128

# .cargo-rbe.toml
import = ["?.cargo-rbe.secrets.toml"]        # optional, gitignored

[common]
remote_timeout = "3600s"
remote_download = "toplevel"
paths = "canonical"                           # or "identity" per package

[config.remote]                               # --config=remote : cache + execution
remote_executor = "grpcs://rbe.example.com"
remote_cache    = "grpcs://rbe.example.com"
remote_instance_name = "main"
remote_header   = ["x-example-auth=@env:RBE_AUTH"]
results_url     = "https://ui.example.com/invocation/{invocation_id}"   # optional
bes_backend     = "grpcs://rbe.example.com"                              # optional
exec_properties = { OSFamily = "linux", Arch = "x86_64", "container-image" = "docker://...@sha256:..." }
workspace = "local"                           # keep incremental for crates you are editing

[config.remotecache]                          # --config=remotecache : cache only
remote_cache = "grpcs://rbe.example.com"
remote_header = ["x-example-auth=@env:RBE_AUTH"]

[config.local-dev]                            # plaintext local server
remote_executor = "grpc://localhost:8980"
remote_cache    = "grpc://localhost:8980"
```

```toml
# .cargo-rbe.secrets.toml  (gitignored)
[config.remote]
remote_header = ["x-example-auth=SOME_KEY_HERE"]
```

All names above (header names, property names, URLs) are placeholders. A flag-compatible `.bazelrc`-style reader (`build:remote --remote_header=...`) is easy to add later because the flag names align, but it is not planned for the first chunks.

### Documentation deliverables

Written alongside the code rather than at the end:

- `README`: quickstart (cache-only first), the same-container requirement, and how to read the JSONL action log.
- `docs/config.md`: every flag and key above, defaults, and which strategy applies to what.
- `docs/servers.md`: notes per server implementation (what worked, which API versions, which platform properties), filled in from test results.
- `docs/hermeticity.md`: path modes, env allowlist, `extra inputs` overrides, and `do_not_cache` policy.
- `docs/troubleshooting.md`: how to interpret fallback reasons from the action log.
- `docs/ci.md`: the GitHub Actions setup (section 15), including secret handling.

## 7. Decomposition into actions

| Cargo unit | REAPI action | Key inputs | Outputs |
|---|---|---|---|
| build.rs compile (host) | rustc, bin | build.rs, dep rlibs, native toolchain | executable |
| build.rs run | shim -> exec of real binary | exe, package tree, filtered env | `OUT_DIR` dir, stdout (directives), stderr |
| proc-macro compile (host) | rustc, proc-macro | sources, dep metadata closure | `.so` |
| lib compile | rustc, split in two (section 10) | sources, `OUT_DIR`, dep metadata closure | `.rmeta`, `.rlib`, `.d` |
| bin/test/cdylib link | rustc, link | sources, dep rlib closure, native toolchain | executable |

The build-script shim is a new proposal; no prior art for it was found in the sources checked. When rustc compiles `build_script_build`, the wrapper runs it remotely. It then writes a tiny shim executable at the path Cargo expects, carrying the real binary's digest. When Cargo runs it, the shim packages Cargo's env (`OUT_DIR`, `CARGO_FEATURE_*`, `DEP_*`, ...), executes remotely, relays stdout and stderr, and materializes `OUT_DIR`. It also reverse-maps virtual paths inside `cargo:` directives back to local ones.

## 8. Well-defined inputs and CAS efficiency

### Layered Merkle input roots

```
/                           input root
+- tc/                      rustc + sysroot            (one digest per rustc version)
+- src/registry/<crate>-<ver>/   immutable; digest memoized by Cargo.lock checksum
+- src/ws/<pkg>/            workspace package; stat-cached
+- deps/<crate>-<hash>/     one DirectoryNode per upstream unit (rmeta or rlib)
+- gen/<pkg>/out/           OUT_DIR from the build-script ActionResult
```

The linker, libc and `cc` come from the pinned worker image or toolchain, selected through server-defined platform properties (property names differ between servers). This follows reninja's advice to configure inside a container and treat its toolchain as the declared environment. Platform properties belong on the Action itself as of API v2.2. One subdirectory per upstream unit makes a unit's `deps/` a list of stable child digests. Rustc still finds everything through repeated `-L dependency=` flags.

### Two-stage input scan

Sending the whole package directory is sound but gives false misses, since editing a README would invalidate the compile.

1. The first run uses the full package tree.
2. The wrapper reads dep-info and stores a small manifest (files read plus `env-dep` vars) under a stage-1 key: the hash of command, entry file and dep digests.
3. Later runs build a precise input root from the manifest.
4. If a precise run fails on a missing file, retry with the full tree.

Trimming is only unsafe for proc macros that read untracked files and degrade silently. Those get a per-package `extra inputs = [...]` override or stay on full-tree mode.

### Per-action flow

1. Compute the Action digest from the digest index alone, with no uploads.
2. Call `GetActionResult`. On a hit you are done, with zero bytes uploaded.
3. On a miss, run `FindMissingBlobs` over the closure. Prefetch has usually already uploaded the toolchain and registry trees. Upload only what is missing: `BatchUpdateBlobs` for small blobs, ByteStream for large ones. Negotiate digest function and compression through `GetCapabilities`.
4. Execute, then record output digests in the index.
5. Materialize outputs according to the download policy.

### Further levers

- Memoize Directory protos per unit.
- Feed libraries `.rmeta` (or hollow rlibs) instead of full rlibs wherever linking is not needed.
- Keep `output_paths` sorted and deduplicated, because the API requires it for consistent Action hashing.
- Prefer directory outputs that can feed downstream actions without the client re-uploading Directory protos. Newer API revisions have an output-format option for this; check server support via capabilities (recalled from the proto, not verified).
- The Remote Asset API maps URIs to CAS digests and lets a server fetch dynamically, which could let servers pull crate sources directly. Support varies by server.
- Server choice matters, and the tool is server-agnostic. The remote-apis README lists many implementations with differing API version ranges, and bazel-remote is listed without remote execution, so it fits the cache-only phase but not later ones. Capabilities are negotiated at startup rather than assumed.

## 9. Hermeticity: paths, env, platform

- **Paths.** The default is canonical mode: map the workspace, registry, target dir and toolchain onto fixed virtual prefixes, add `--remap-path-prefix`, and rewrite dep-info back to local paths on return, since Cargo's fingerprints read it. That gives cross-user cache hits. Identity mode mounts the same absolute path remotely (reninja's approach). It is needed for crates that use `env!("CARGO_MANIFEST_DIR")` for runtime lookups, which the experiment confirmed gets baked into the output.
- **Env.** Use an allowlist (`CARGO_*`, `OUT_DIR`, target vars), sorted, with `HOME` and `TMPDIR` pinned and the jobserver vars dropped. Cross-check against the `env-dep` lines. Never ignore a variable just to raise the hit rate.
- **Platform parity.** In v1 the host triple must equal the worker's. Proc macros and build scripts are host artifacts, and Cargo takes the host from the local rustc. The clean way is to run Cargo in the same container image the workers use, as the reninja maintainer recommends.
- **Non-hermetic build scripts** (embedding timestamps or git state) are marked `do_not_cache` or kept local by policy.

## 10. Pipelining and parallelism

REAPI actions return outputs only at completion, so Cargo's single-process pipelining cannot carry over directly. The wrapper splits each library into a metadata action and a full-codegen action. When the metadata action completes, the wrapper prints rustc's `{"artifact":...,"emit":"metadata"}` JSON notification to stderr so Cargo unblocks dependents. It exits only after the codegen action finishes.

Two options for the metadata step:

- **Option A: `--emit=metadata`.** Cheap, but it duplicates frontend work. Pick one `.rmeta` flavor and verify empirically that the SVH matches what the final rlib carries.
- **Option B: hollow rlib via `-Zno-codegen`.** Matches Buck2's approach, but requires nightly or `RUSTC_BOOTSTRAP`.

Decision: start with A and make B opt-in.

Parallelism: Cargo's `-j` bounds how many wrapper processes run at once. Set it high for remote builds; wrapper processes are thin and mostly waiting.

## 11. Outputs and materialization

- **Replay diagnostics.** Store rustc's JSON stderr in the ActionResult and replay it on a hit, so a fresh checkout still shows warnings.
- **Stripping `-C incremental`.** Remove `-C incremental` from remote actions, because its state is path-bound and stateful. rules_rust's incremental support needed Bazel-managed tree artifacts so the path is identical on local and remote workers. Instead use the `workspace = "local"` hybrid: dependencies and build scripts are remote and cached, while the crates you edit compile locally with incremental.
- **Rewrite dep-info** back to local paths on return, so Cargo's fingerprint logic works.
- **Builds without the bytes.** Keep only digests in the index and write stub files at the paths Cargo expects. Materialize on demand for local fallback and top-level binaries, as Bazel's download modes do. This is risky, because rustdoc, rust-analyzer and local fallback all read real files, so it comes late. Download policy is `minimal | toplevel | all`.
- **Cargo layout.** Derive everything from rustc's arguments (`--out-dir`, `--emit`, `--extern`, `-L`), not from Cargo's directory layout. `build-dir` was stabilized in 1.91, and its internal layout is explicitly an implementation detail that may change.

## 12. Failure model

The wrapper is fail-closed. It rejects unknown rustc flags, paths it cannot map, env it cannot classify and platform mismatches. In those cases it runs the real rustc locally and logs the reason as JSONL. The log doubles as the tuning signal for what to make eligible next.

## 13. Rollout (implementation plan)

0. **Cache-only.** Execute locally and publish ActionResults. This validates key stability with no execution risk. Acceptance test: two different checkout paths produce identical Action digests for the same unit.
1. **Remote execution of registry dependencies** with eager downloads, running Cargo in the worker image so local build scripts see the same environment.
2. **Remote build scripts via the shim, and proc macros.**
3. **Metadata/codegen split and the two-stage input scan.**
4. **Workspace crates, link actions, tests, clippy and check, then stub-based minimal downloads.**
5. **Upstream.** Propose a Cargo executor hook so Cargo can size parallelism itself and skip the wrapper hop.

## 14. Open risks

- Link actions are the hardest part. Native inputs (system libs, SDKs, response files) are why cargo-reapi has not made them remote-eligible.
- Untracked file reads by proc macros.
- SVH consistency between the metadata and codegen actions.
- Cross-OS workers (macOS laptop to Linux workers), which this design does not solve.
- Rustc flag drift across toolchains, so the arg parser must be conservative.
- Server-specific behavior, such as platform property names, API version gaps, and whether an invocation or results page needs more than REAPI metadata (see section 15).

## 15. Delivery plan and test harness (requested workflow)

### Working agreement

- Deliverables are incremental and atomic. Each chunk is presented with `present_files`, and a tarball of the repo state is created and presented with every chunk.
- Chunks are delivered before hitting token limits, never half-finished.
- The goal is to get something you can run quickly against real REAPI implementations. You send back results (logs, failures, URLs if any), and the next chunk is built from them.
- The tool must work against any REAPI implementation. BuildBuddy is one of several candidates, and its cloud service is only one deployment of it. We are unlikely to test only against that.
- This document is updated as the design changes. Your requests from this conversation live in this section.

### Chunk plan

**D0. This document**, presented for approval.

**D1. REAPI smoke test, plus GitHub Actions workflow.**

- A small `cargo-rbe` crate with a `smoke` subcommand. It takes the same flags and config as the real tool (section 6), so the smoke test exercises the real configuration path. It does the following, printing each step so failures are diagnosable:
  1. `GetCapabilities`: print supported API versions, digest functions, compressors, and whether execution is enabled.
  2. Upload a trivial Command and an input root to the CAS.
  3. `Execute` a small remote action (for example a shell command printing `uname -a`). This is the "do one small thing remotely" test of the API itself.
  4. Fetch stdout, stderr and exit code from the ActionResult.
  5. Re-run the identical action and report whether it was served from the action cache.
  6. Print the invocation ID, and the results URL only if a `results_url` template is configured.
- Cache-only servers are supported. If `GetCapabilities` says execution is not available, steps 3 to 5 are reported as skipped, and the CAS and ActionCache are still exercised directly (upload, read back, `UpdateActionResult`, `GetActionResult`).
- Workflow design, with no service hard-coded:
  - The endpoint comes from a repository variable (for example `RBE_REMOTE_EXECUTOR`). Headers come from secrets, mapped onto environment variables and referenced as `@env:NAME` in the config. Secret and variable names are yours to choose.
  - The remote job is skipped cleanly when the variable or secret is absent (for example on forks), and never echoes secret values.
  - Optional second job: start a local REAPI server as a service container in the runner and run the same smoke test against it with no secrets. This lets pull requests exercise execution without any cloud account. Candidate servers include those listed with remote execution support in the remote-apis README (Buildbarn, Buildfarm, BuildBuddy OSS, NativeLink and others). Which one to use is a decision for you, since each needs its own configuration and I have not verified any of those setups.

**D1.1. Local BuildBuddy stack** (done): script and CI job for cache, execution and BES locally, with no secrets.

**D1.2. Minimal BES publisher** (done): publishes the smoke run as an invocation so servers that build pages from build events have a record.

**D2. Cache-only wrapper** (done): `RUSTC_WRAPPER` that computes canonical Action digests, consults `GetActionResult`, executes locally on a miss and publishes the result. Includes the two-checkout acceptance test and the JSONL action log.

**D3. Build scripts** (done): shim, cache and remote execution, fallback to local; see the D3 status above. Remote execution of an ordinary rustc unit remains open.

Later chunks follow the rollout in section 13.

### D1 status (delivered 2026-11-15)

Contents: `cargo-rbe-D1.tar.gz` (crate, checked-in generated stubs, pinned protos, workflow, README, example config).

Verified in the sandbox (Rust 1.91, against bazel-remote 2.6.4, a cache-only server):

- Clean build from the tarball with `--locked`.
- `GetCapabilities` (that server reports API 2.0 to 2.3, SHA256, ZSTD, no execution), CAS batch calls with deduplication, ByteStream write and read of about 1 MiB, ActionCache update and get.
- Config layering (`[common]`, `--config`, imports with `?`, environment, CLI), `@env:` and whole-spec header sources, unknown-config and missing-secret errors, and a clean skip when a server has no Execution service.
- From the pinned protos: `Command.output_directory_format` exists (section 8), and the request-metadata header is `build.bazel.remote.execution.v2.requestmetadata-bin` (section 16).

Not verified (needs your runs): `Execute`/`WaitExecution` against a real executor, platform property handling, TLS and custom headers on a real service, and anything about results pages.

### D1.1 status (delivered 2026-11-15): local BuildBuddy stack

Contents: `cargo-rbe-D1.1.tar.gz` (D1 plus `scripts/local-buildbuddy.sh` and a `local-buildbuddy` CI job).

Verified in the sandbox against BuildBuddy v2.325.0 (Linux amd64, no containers):

- All six smoke steps pass, including `Execute` (stages QUEUED, CACHE_CHECK, EXECUTING, COMPLETED), fetching `out.txt` from the CAS, and a second identical `Execute` answered with `cached_result=true`. This closes the main gap left by D1.
- The plain OSS `buildbuddy-linux-amd64` binary does not implement the Execution service, even with `--remote_execution.enable_remote_exec=true`. Execution needed the `*-enterprise-*` server and executor from the same release, plus Redis (`--remote_execution.redis_target`) for the scheduler. The executor ran with the bare runner (no isolation), which is fine for local testing only. License terms for the enterprise binaries should be checked before relying on them in CI.
- Capabilities advertised by this server: API 2.0 to 2.11, SHA256/SHA384/SHA512/SHA1/BLAKE3, IDENTITY and ZSTD compression, max batch 4 MB, execution enabled.
- On a 1-CPU machine the executor stalled at "Downloading inputs" (goroutine dump showed an errgroup that never started workers). Setting `--cache.client.filecache_link_parallelism` and `--cache.client.input_download_concurrency` fixed it. The script sets both; I did not isolate which one matters.
- Invocation IDs: the server linked the execution to the invocation ID sent in request metadata (one row in its `Executions` table), but created no invocation record, so no populated invocation page exists until build events are published. A minimal BES publisher (started and finished events) is therefore the way to get a visible invocation page on this server, and is proposed as D1.2.
- The `local-buildbuddy` CI job is written but has not run on GitHub Actions yet.

Not evaluated: lighter alternatives (for example NativeLink or Buildbarn). BuildBuddy needs three processes plus Redis and about 780 MB of downloads (cached in CI), so a lighter stack that offers execution would still be worth a look, but none was tried.

### Field results for D1 and D1.1 (reported 2026-11-16)

Received from your runs (GitHub Actions and a local Linux host):

- `local-cache-only`: 4 ok, 1 skipped, as expected, twice (two runs).
- `local-buildbuddy` on a GitHub-hosted runner: 6 ok. Execution reached the executor, stages QUEUED, CACHE_CHECK, EXECUTING, COMPLETED, in about 24 ms, then `cached_result=true` in under 1 ms.
- `remote` against BuildBuddy cloud (`grpcs://remote.buildbuddy.io`): 6 ok, twice. Capabilities were identical to the local server (API 2.0 to 2.11, five digest functions, IDENTITY and ZSTD, execution enabled, no node properties). The first execution took 0.7 to 1.1 s, the repeat 60 to 100 ms with `cached_result=true`. The worker was a Linux container, so a default `/bin/sh` and `uname` need no platform properties there.
- Local host, same stack: all six steps ok, plus the screenshot and console log of the "Invocation not found!" page.

What this settles: the `Execute`/`WaitExecution` path, header and TLS handling against a real service, and the cache-hit behavior work on both a local and a cloud BuildBuddy. Still unseen: any server other than BuildBuddy (and bazel-remote for cache only), and non-default platform properties.

Why the page said "Invocation not found!": the page asks the server for an invocation record. The server only creates one from build events. Our REAPI calls carried the invocation ID in request metadata, which was enough for the server to link the execution to that ID (one row in its executions table) but not to create the invocation. No events were published, so there was nothing to show. D1.2 publishes them.

### D1.2 status (delivered 2026-11-16): minimal BES publisher

Contents: `cargo-rbe-D1.2.tar.gz`. New: `src/bes.rs`, pinned BES protos (googleapis build v1 and Bazel 9.3.2 `build_event_stream.proto` with its imports), `--bes_header`, `--build_metadata`, a `bes` smoke step, unit tests, and the CI changes below.

Verified in the sandbox against local BuildBuddy v2.325.0:

- `bes: published 16 events, 16 acknowledged`.
- The server now has an invocation record for the ID we sent: command `smoke`, success, exit code `SUCCESS`, and the role taken from `--build_metadata`. Fetching it through the server's own API returned the structured command line, workspace status, build metadata, and an attached `cargo-rbe-report.json`. The console text from the smoke run was stored as a log chunk and read back.
- A header value passed on the command line (`--remote_header=x-demo=...`) appears nowhere in the server database or logs. Unit tests cover the redaction.
- Negative cases: a server without BES (bazel-remote) gives `[FAIL] bes` with the server's `Unimplemented` messages and exit code 1; an unreachable endpoint gives `could not connect to bes_backend`.

Not verified: the page in a browser (no browser in the sandbox; the data the page loads is confirmed), BES on BuildBuddy cloud (headers apply to BES, but untested there), any other BES server.

Design choices worth knowing: lifecycle events are best effort because a BES server may implement only the tool stream; progress events are emitted one step behind so each can declare the next as its child, and a final empty progress event closes the chain; the command line is redacted before publishing; the CI keys (`ROLE`, `REPO_URL`, `COMMIT_SHA`, `BRANCH_NAME`) are added only under GitHub Actions.

CI changes in the same chunk (your caching request): the toolchain is pinned to 1.91 and `~/.rustup` plus `~/.cargo/bin` are cached under a key that names that version, so the key describes its contents (`actions/cache` never overwrites an existing key, so a floating `stable` would either freeze or thrash). `~/.cargo/registry` and `target/` are cached under a key with the Cargo.lock hash and a prefix fallback; source-only changes reuse the entry, which still saves the dependency builds. The bazel-remote binary is cached and its download is skipped when present and executable; the asset architecture now comes from `uname -m`, which also fixes the amd64-versus-`runner.arch` mismatch flagged in your review. Unit tests run in the build job. Redis is still installed through apt on each `local-buildbuddy` run, which I left alone because caching apt state is not worth the complexity for one small package.

### Field results for D1.2 and fixes (reported 2026-11-16)

Your runs: `local-cache-only` 4 ok and 1 skipped; `local-buildbuddy` 6 ok; `remote` (BuildBuddy cloud) 6 ok and `bes: published 16 events, 16 acknowledged`. The local analysis and screenshot show the invocation page fully populated (command line, workspace status, metadata, console, attached report). This settles BES against both a local and a cloud BuildBuddy.

Observations that led to fixes in this drop:

- **Tool name.** The page showed "bazel v0.0.1 smoke" with an empty "Tool tag". The event stream has no tool-name field; the only tool identity is `OptionsParsed.tool_tag`, which we did not send. We now send `OptionsParsed` with `tool_tag=cargo-rbe`. The leading word "bazel" is chosen by the server's UI and cannot be changed from our side. (REAPI request metadata already carried `tool_name=cargo-rbe`; servers display that on execution and cache views, not on the invocation header.)
- **Duplicated command in the command line.** The structured command line rendered `smoke -- smoke ...` because the residual section repeated the subcommand. It is now split into executable, command, an options list, and residual. Because the options now arrive as a parsed option list, servers can also derive badges such as "remote execution on" from them (not yet confirmed in a browser).
- Deprecated-field warnings are silenced where we set those fields on purpose for older servers, and doctests are off for the generated code.

### D2 status (delivered 2026-11-16): cache-only rustc wrapper

Contents: `cargo-rbe-D2.tar.gz`. New: `cargo-rbe-wrap` (the `RUSTC_WRAPPER`), `cargo-rbe cargo ...` (launcher), a persistent stat cache, `examples/sample-ws`, `scripts/two-checkout-test.sh`, a `wrapper-cache-test` CI job, and the wrapper test also in the `local-buildbuddy` job. The README has usage and caveats.

What it does is in the README. Verified in the sandbox (Rust 1.91; bazel-remote 2.6.4 and local BuildBuddy v2.325.0):

- Digest stability: the two cacheable units of the sample (a registry crate and a workspace crate with a build script and `OUT_DIR`) get identical Action digests in two checkouts with different paths and different `CARGO_HOME`s.
- Publish in checkout A, restore in checkout B (fresh target directory): both units hit; both binaries print the same output; the compiler warning is replayed on the hit; Cargo treats restored units as fresh (no rustc invocations on rebuild).
- Editing a source file misses and publishes; the same edit in the other checkout hits; the edited code is what runs.
- Through the launcher with BES against BuildBuddy: 2 units published in one checkout and 2 restored in the other; each cargo invocation appears as a `build` invocation with 16 of 16 events acknowledged.
- Timings on the tiny sample crates: a hit costs 6 to 8 ms, a local compile 25 to 36 ms, a miss including hashing and upload 57 to 72 ms. These say little about real projects.

Findings while building it, worth recording:

- **Artifacts are not location-independent by default.** The same registry crate compiled under two `CARGO_HOME`s gave `.rmeta` files that differed in 33 bytes (the embedded source path and hashes derived from it). Because dependents' keys include their dependencies' contents, one dependency's difference cascades into misses of everything above it. Remapping the registry prefix (`--remap-path-prefix`) fixes it; the canonical command carries a fixed virtual form of the flag. Workspace crates are not remapped, since that changes the paths users see in their own diagnostics; `.rlib` files (debuginfo) still embed the working directory, which only matters where an rlib is itself a dependency input.
- **A dependency closure is needed for a sound key.** rustc may load any transitive dependency's metadata from the search path, so an action's inputs are not just its direct `--extern` files. The wrapper records, per built artifact, the closure of its dependencies in a local index, and a unit whose closure is unknown (built before the wrapper saw it) is not cached. A consequence: running the wrapper against an already-built `target/` caches little until a clean build. In the longer run this belongs in the daemon or in a unit graph.
- **Cargo's own fingerprint is sensitive to registry source paths**, so reusing one `CARGO_HOME` per checkout matters; a test bug of mine (a different `CARGO_HOME` on a rebuild) showed it as "path to the source changed".
- **A shared target directory breaks the test, not the wrapper.** My own final check ran the acceptance test with `CARGO_TARGET_DIR` exported, so both checkouts shared one target directory, the second never invoked the build script, and the test reported a unit "missing in B". I presented the tarball before investigating; the cause was confirmed, the script now unsets the variable, and an `EXTERNAL_TARGET=1` mode (target directories outside the checkouts) passes all three phases too, which also exercises target-root detection.
- **Test infrastructure lesson:** deleting a server's data directory under a running server makes the next run fail with "blob not found". It looked like a wrapper bug for a few minutes.

Differences from the design, on purpose: no daemon yet (each wrapper process connects itself and keeps its state on disk; a circuit breaker skips the network for 30 seconds after a failed connect); input hashing is the whole package directory rather than the precise two-stage lookup; only library units are cached; inputs are not uploaded to the CAS (nothing executes them yet), only the Command, Action and outputs. Remote execution of one real rustc unit (D3) needs the input tree and the toolchain on the worker.

Not verified: any real-world project (large dependency graphs, features, proc-macro heavy code, build scripts that probe the system), BuildBuddy cloud for the wrapper, cache eviction behavior, and parallel builds under heavy `-j` (the wrapper is thread-safe by construction: separate processes, atomic file writes, but untested at scale).

### Field results for D2 and real-project runs (reported 2026-11-17)

Your GitHub Actions runs: smoke 7/7 on BuildBuddy cloud (17 events acknowledged, one more than before because of `OptionsParsed`) and on the local stack; the wrapper acceptance test passed on both the local cache-only server and local BuildBuddy; the external-target variant failed with "nothing was published in A". Cause: both variants ran against one server in one job, so the second found the first run's entries already cached. The test assumed an empty cache. Fixed: the workspace crate now carries a run-unique line, and the check accepts "already cached" for the registry crate while still requiring at least one publish.

Real projects (debug profile, one-CPU sandbox, local cache-only server, build A in one directory then build B in another with a different `CARGO_HOME`):

| | ripgrep 15.2.0 | fd v10.5.2 |
|---|---|---|
| rustc invocations | 44 | 71 |
| cacheable and published by A | 33 | 56 |
| restored by B | 33 of 33 | 56 of 56 (after the fix below; 54 of 56 before) |
| not cached | 10 build scripts, the `rg` binary | 12 binaries (11 build scripts, `fd`), 1 proc macro, 2 crates needing native link flags (jemalloc) |
| uploaded by A / restored by B | 148 MB | 131 MB |
| wall time A / B | about 55 s / 17 s | about 140 s / 108 s |
| average cost of a hit | | 134 to 229 ms |

What it showed:

- **A locally built proc macro poisons its dependents.** In fd, `clap` and `clap_complete` missed in B with identical sources and commands; the only difference was `libclap_derive-...so`, built locally in each checkout into different bytes. With serde derive the same thing would hit most of a dependency graph. The cause was that path remapping was applied only to units the wrapper caches; passed-through units, including proc macros, embedded `CARGO_HOME` and the working directory. Remapping every registry-crate compile (registry path and working directory) makes the `.so` byte-identical across checkouts (same digest in both) and fd goes to 56 of 56.
- **Build-script execution dominates once libraries are cached.** In fd, `tikv-jemalloc-sys`'s build script (compiling jemalloc's C sources) took about 94 s of the roughly 108 s in B; the 56 restored units cost 7 s in total. In ripgrep, build scripts took a few seconds, so the cache pays off immediately. Estimated from file timestamps, one-CPU machine, indicative only.
- **Restored data is large in the debug profile** (130 to 150 MB for a small project), mostly `.rlib` debuginfo. Transfer volume, not lookup latency, will govern real use; `--release` and `.rmeta`-only dependency edges are the levers.

Two bugs of mine surfaced on the way, both in test tooling, and one led to a hardening:

- The first version of the real-project script took its results directory from an environment variable named `OUT_DIR`, which is also what Cargo uses for build-script output. rustc inherited it, the wrapper hashed that directory as a crate's generated code, and B missed 24 of 33 units. Renamed to `RESULTS_DIR`, and the wrapper now refuses an `OUT_DIR` that is not inside the target directory (the unit is then simply not cached, with the reason logged). The workflow had the same collision and is fixed.
- A shared `CARGO_TARGET_DIR` and a pre-warmed server each broke the acceptance test earlier; see above.

Where D3 should start, given this: **running build scripts remotely or caching their output** (the biggest remaining cost where native dependencies are built from source, and a prerequisite for remote execution of everything downstream), then **proc-macro and link units** (proc macros are now reproducible, but still compiled locally), then crates with native link flags. Remote execution of a single rustc unit, as proposed before, would show the mechanism but not the largest cost.

### Field results for D2.1 (reported 2026-11-18) and the bug they exposed

Your GitHub Actions real-project run (ripgrep, four vCPUs) showed 24 of 33 libraries published in build A, with nine library units rejected as "dependency closure unknown", cascading up the dependency graph. Root cause: Cargo **pipelines**, starting a dependent as soon as a dependency announces its `.rmeta` on stderr, before the dependency's rustc process exits. The wrapper recorded a unit's dependency closure only at process exit, so on a multi-core machine dependents raced ahead. My sandbox has one CPU, so Cargo ran one job at a time and never exposed it. Reproduced here with `-j8` (18 of 33 published, 15 units "closure unknown"); fixed by recording the closure when the `.rmeta` notification appears, after which `-j8` gives 33 of 33 published and 33 of 33 restored. `scripts/real-project-test.sh` now defaults to `-j8`.

### D3 status (delivered 2026-11-18): build scripts

Contents: `cargo-rbe-D3.tar.gz`. New: the build-script shim and flow (`src/buildscript.rs`), directory-tree collection and restore (`src/tree.rs`), an `Execute` helper, launcher flags `--build-scripts off|cache|remote`, `--build-script-skip`, `--build-script-path`, `--env-passthrough`, a sample crate whose build script compiles C with the `cc` crate, `scripts/build-script-test.sh`, and CI steps for it. The README describes the mechanism, the environment policy, requirements for remote mode and the limits. Also in this drop: the MSRV change (Rust 1.85, see below) and the pipelining fix above.

Verified here against bazel-remote 2.6.4 and a local BuildBuddy v2.325.0 (enterprise server, bare-runner executor on the same host):

- The acceptance test passes in both modes: scripts go through the shim (published, or executed on the executor with stages QUEUED, CACHE_CHECK, EXECUTING, COMPLETED); a second checkout at another path restores all of them including the C library compiled by `cc`, and the binary prints the same output; a rebuild runs nothing; editing the C source reruns the script and the same edit in the other checkout hits; with an unreachable server the build succeeds with local runs and a correct binary.
- fd v10.5.2 with `--build-scripts=remote`: all 11 build scripts executed on the executor in build A (including jemalloc's C build) and all 11 restored in build B; B restored 56 of 56 libraries as well. Build B took 23 s against 108 s with D2 alone. Build A took 191 s, but that number is not representative: the executor shares one CPU with the client, so remote tasks queue behind each other and behind local compiles (`anyhow`'s trivial script reports 151 s of queueing).

Bugs found while building it, all fixed:

- **Stale-looking executable.** The shim is a hard link to the wrapper binary and inherited its old modification time; Cargo treats a unit as stale when an output is older than a source file, so every rebuild reran the build-script compile. The shim's mtime is now set to the current time (a copy if that is not permitted).
- **Output paths are relative to the working directory.** I set the working directory to the package, so `out` named `pkg/out` and the server returned no output at all. The working directory is now the input root, and the script changes into the package itself.
- **A silent failure.** An unfound output directory was skipped without error, leaving `OUT_DIR` empty while reporting success. It is now an error (listing what the server returned), which triggers the local fallback.

Design decisions to know about: a hard link per build script rather than a dedicated shim binary (the wrapper is about 15 MB in release; copies are used where links fail); remote results and local-run results live in different key namespaces, because a local run depends on the local C toolchain; the `rustc` identity is part of every key; `--remap-path-prefix` for the working directory is applied to a workspace crate's build-script compile only when `--build-scripts` is on, because `--remap-path-scope` (which would exclude diagnostics) is nightly-only on 1.85 and 1.91.

Not verified: a real worker pool whose image and host differ from the client (the local executor shares the client's filesystem and toolchain, which hides missing-tool problems), BuildBuddy cloud for build scripts (its default image may lack a C compiler and the same rustc; set `container-image` via exec properties), scripts that probe the system (`pkg-config`, `bindgen` with libclang), large `OUT_DIR`s over a real network (fd's jemalloc restore took about 10 s locally), and the new CI jobs on GitHub Actions.

### MSRV (Rust 1.85)

Applied from your note: `uuid` pinned to 1.26.1 (`cargo update uuid@1.29.2 --precise 1.26.1`), `rust-version = "1.85"` in both manifests, `resolver = "3"` so future `cargo update` stays within it, the sample workspace locked the same way, and CI pinned to 1.85. Verified with a Rust 1.85.1 toolchain: the full workspace builds with `--locked` and the unit tests pass. No code needed changing; all of D3 builds on 1.85 as well.

### Where to go after D3

What is left uncached or local: binaries and tests (links), proc macros (now reproducible but still compiled locally), crates with native link flags, and compile steps of build scripts. Candidates: (1) link and proc-macro units, which needs the linker and native inputs treated as part of the key or the worker image; (2) a long-lived daemon, since a hit costs 130 to 230 ms of mostly connection setup; (3) the precise two-stage input scan; (4) remote execution of ordinary rustc units; (5) transfer volume (a debug build restores 130 to 150 MB). Your results on a real worker pool and a larger project should decide the order.

### Invocation IDs and results URLs

These depend on the service, so none of it is hard-coded.

- The tool always generates an invocation ID (a UUID) and attaches it to every REAPI call as request metadata (section 16). Servers may use it for grouping or ignore it.
- A results URL is printed only if `results_url` is configured. It is a template with `{invocation_id}` (and possibly other placeholders later), so it works for a cloud service, a local copy of a service, or a custom UI. If none is configured, only the ID is printed.
- Servers that build their invocation page from Build Event Stream data need events as well; REAPI metadata alone only links executions to the ID. If `bes_backend` is configured, the smoke test publishes them (D1.2). Some servers may have no such page at all, in which case only the ID is meaningful.
- Platform property names (container image and so on) and whether they are required are defined by each server. D1 reports what `GetCapabilities` and the first execution attempt reveal, and these are recorded in `docs/servers.md`.

### What I need from you after D1

- The workflow run log, or the printed output of `cargo-rbe smoke`.
- Which server implementation and version you ran it against, and whether it was cloud, local or custom.
- The `GetCapabilities` output (API versions, compression, digest functions, execution enabled).
- Any error text from the remote step, especially container or platform-property errors.
- Whether a results URL was configured and, if so, whether it showed anything.

## 16. REAPI surface: which RPCs are used, and for what

Only what is needed is used, introduced in the chunk where it first matters. Everything is negotiated through capabilities, since servers differ in API version (the remote-apis README lists a range of supported versions per server).

| API / RPC | Used for | Chunk |
|---|---|---|
| `Capabilities.GetCapabilities` | Discover API versions, digest functions, compressors, whether execution and cache update are allowed | D1 |
| `ContentAddressableStorage.FindMissingBlobs` | Find which input blobs the server lacks, before any upload | D1 |
| `ContentAddressableStorage.BatchUpdateBlobs` | Upload many small blobs (Directory and Command protos, small files) in few round trips | D1 |
| `ContentAddressableStorage.BatchReadBlobs` | Download many small outputs | D1 |
| `ByteStream.Write` / `Read` | Upload and download large blobs; compressed variants if the server advertises them | D1 |
| `ActionCache.GetActionResult` | Cache lookup before anything is uploaded | D1 / D2 |
| `ActionCache.UpdateActionResult` | Publish locally produced results (cache-only mode) | D2 |
| `Execution.Execute` (and `WaitExecution`) | Run an Action remotely and stream operation updates; reconnect after dropped streams | D1 / D3 |
| `ContentAddressableStorage.GetTree` | Fetch a directory tree by digest when materializing directory outputs | D3 |
| Remote Asset API `Fetch` | Optionally let the server fetch crate sources by URI and checksum, if supported | later |
| Remote Logstream API | Optionally stream stdout and stderr of long-running actions | later |
| BES `PublishLifecycleEvent` and `PublishBuildToolEventStream` | Optional invocation reporting, only if `bes_backend` is configured | D1.2 (done) |

Cross-cutting details:

- **Request metadata.** Each call carries the standard request metadata header (tool name and version, invocation ID, action ID). This is how a server can correlate calls into one invocation without BES.
- **Action shape.** Platform properties go on the Action (API v2.2 and later), and for older servers also on the Command, as the API describes. `output_paths` is used rather than the deprecated output file and directory fields, and requires v2.1 or later, so servers older than that are detected via capabilities and reported as unsupported.
- **Digest function.** SHA-256 by default, negotiated through capabilities. It must be one the server supports. It is fixed per run, and all digests in a run use the same function.
- **Retries and timeouts.** Idempotent RPCs (`FindMissingBlobs`, reads, `GetActionResult`) are retried with backoff. Uploads are resumable where ByteStream supports it. `Execute` streams are re-attached with `WaitExecution` when the connection drops.
- **Cache poisoning rule.** Only successful results are written to the ActionCache. Actions marked non-hermetic are executed with `do_not_cache`.

## 17. Implementation language and proto tooling

Cargo being written in Rust does not force our tool to be. The question is what fits a wrapper that is spawned once per rustc invocation, plus a daemon that does a lot of concurrent gRPC I/O. My assessment of the options:

| Language | For | Against |
|---|---|---|
| **Rust** | Users already have the toolchain. Fast process startup, which matters because the wrapper runs once per unit. One static binary. Good async gRPC (tonic and prost). Can share dep-info and rustc-flag parsing with the Rust ecosystem, and is the only realistic option for eventual upstreaming into Cargo. Other REAPI projects exist in Rust (a Rust REAPI server and client code are mentioned in the Cargo issue on this topic, and NativeLink ships its own proto crate). | Smaller REAPI client ecosystem than Go. We generate stubs ourselves. |
| **Go** | The strongest REAPI ecosystem. The remote-apis repo ships generated Go code in-tree, and reninja, reclient and many tools are Go. Fast startup, easy static binaries. | A second language in a Cargo project's toolchain. No path to upstream into Cargo. Reusing Rust-side parsing would mean reimplementing it. |
| **Python** | Fastest to prototype. Good for scripting experiments. | Interpreter startup cost on every rustc call, and distribution is awkward (virtualenv, grpcio wheels). Not suitable for the wrapper hot path. |

**Recommendation: Rust**, for both the daemon and the thin wrapper, including the D1 smoke test, so that D1 code is not thrown away. The wrapper-to-daemon boundary (a local socket protocol) keeps the daemon replaceable, so if Go turns out to be better, it can be swapped without redesigning the wrapper. This is a judgment call. **Decision (user):** start with Rust, treated as an experiment, since it may be more than the problem needs. A Go implementation of the same smoke test and cache-only wrapper is planned afterwards as a comparison, using the same config and flags (section 6), so the two can be judged on real results rather than guesses. Python is reserved for throwaway experiments and CI glue, if needed at all.

**Proto management.**

- The protos come from `bazelbuild/remote-apis`, plus their dependencies (googleapis, protobuf well-known types). The buf registry hosts the REAPI module you linked, so `buf` can pin it as a dependency.
- Proposal: pin the protos to a specific commit (vendored snapshot or buf dependency lock), and **check the generated Rust code into the repository**. The remote-apis repo does the same for Go. That way contributors and CI do not need `protoc` or `buf` just to build.
- Use `buf` as optional tooling for lint, breaking-change checks and regeneration (a `buf.yaml` and `buf.gen.yaml` with prost and tonic plugins), not as a build-time requirement. If we prefer fewer tools, `tonic-build` with vendored protos and `protoc` in a regeneration script is the fallback. Plugin choice is something I would verify when D1 is written.
- BES and Remote Asset protos are added only when those chunks start, so D1 stays small.
- As a cross-check during development, any standard REAPI client (for example `grpcurl` against the same endpoint, or Bazel itself) can be used to confirm that a server behaves as the smoke test reports.

## 18. Sources

- bazelbuild/remote-apis: https://github.com/bazelbuild/remote-apis
- REAPI proto docs (Buf): https://buf.build/bazel/remote-apis/docs/main:build.bazel.remote.execution.v2
- buildbuddy-io/reninja: https://github.com/buildbuddy-io/reninja
- reninja issue #22 comment: https://github.com/buildbuddy-io/reninja/issues/22#issuecomment-4014988207
- recc (BuildGrid, obsolete): https://gitlab.com/BuildGrid/recc
- cargo-reapi: https://github.com/TamedTornado/cargo-reapi/blob/main/docs/introducing-cargo-reapi.md
- Cargo changelog (build-plan removal, build-dir): https://doc.rust-lang.org/cargo/CHANGELOG.html
- Cargo unstable features (`--unit-graph`): https://doc.rust-lang.org/cargo/reference/unstable.html
- rust-lang/cargo#7468 (REAPI support): https://github.com/rust-lang/cargo/issues/7468
- rules_rust pipelining (metadata vs. full, PR #1207): https://github.com/bazelbuild/rules_rust/pull/1207
- rules_rust hollow rlib / `-Zno-codegen` (issue #3977): https://github.com/bazelbuild/rules_rust/issues/3977
- rules_rust incremental via tree artifacts (PR #3925): https://github.com/bazelbuild/rules_rust/pull/3925
