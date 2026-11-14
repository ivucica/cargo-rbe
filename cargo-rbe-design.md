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

**D2. Cache-only wrapper.** `RUSTC_WRAPPER` that computes canonical Action digests, consults `GetActionResult`, executes locally on a miss and publishes the result. It includes the two-checkout digest-stability acceptance test and the JSONL action log.

**D3. Remote execution of one real rustc unit** (a registry dependency such as `cfg-if`), with eager downloads.

Later chunks follow the rollout in section 13.

### Invocation IDs and results URLs

These depend on the service, so none of it is hard-coded.

- The tool always generates an invocation ID (a UUID) and attaches it to every REAPI call as request metadata (section 16). Servers may use it for grouping or ignore it.
- A results URL is printed only if `results_url` is configured. It is a template with `{invocation_id}` (and possibly other placeholders later), so it works for a cloud service, a local copy of a service, or a custom UI. If none is configured, only the ID is printed.
- Some services build their invocation page from Build Event Stream data rather than from REAPI calls. For those, REAPI metadata alone may not produce a page. If `bes_backend` is configured, a minimal BES publisher (started and finished events) can be added in a later chunk. Whether it is needed is unknown until we see results from a real server, and some servers may have no such page at all.
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
| BES `PublishBuildToolEventStream` | Optional invocation reporting, only if `bes_backend` is configured and a server needs it | later, if needed |

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

**Recommendation: Rust**, for both the daemon and the thin wrapper, including the D1 smoke test, so that D1 code is not thrown away. The wrapper-to-daemon boundary (a local socket protocol) keeps the daemon replaceable, so if Go turns out to be better, it can be swapped without redesigning the wrapper. This is a judgment call, and I will go with Go or another language if you prefer. Python is reserved for throwaway experiments and CI glue, if needed at all.

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
