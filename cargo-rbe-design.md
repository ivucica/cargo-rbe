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

## 2. The discarded alternative: export the plan (`--build-plan`) and execute it externally

The obvious alternative is to ask Cargo for its plan, then have an external tool execute that plan against REAPI. I discarded it for two reasons.

1. `--build-plan` no longer exists. Cargo removed the unstable feature entirely in 1.93 (changelog, 2026-01-22). The Cargo team points to plumbing commands, `--unit-graph` and structured logging as its replacements.
2. The nearest survivor, `--unit-graph`, carries only the unit DAG and per-unit metadata. A plan-driven tool must therefore reimplement rustc command-line construction: flags, `-C metadata`, `extra-filename`, env vars, link args from build scripts. That duplicates Cargo's logic and drifts from it.

The wrapper route lets Cargo keep resolving features, env and flags. It is also the stance cargo-reapi takes: Cargo remains the planner, and the wrapper observes the exact compiler commands Cargo schedules. An early Cargo issue (rust-lang/cargo#7468) argued that this feature will eventually need to live in Cargo proper because it affects how many jobs can run in parallel. That is the upstream path in the rollout (section 12).

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

Not yet verified: SVH equality between metadata-only and full compilation; behavior on current Cargo; BuildBuddy-specific behavior (section 14).

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

Users run `cargo build` as before. Configuration is a sketch like this:

```toml
# .cargo/config.toml
[build]
rustc-wrapper = "cargo-rbe-wrap"
jobs = 128

# .cargo-rbe.toml
[remote]
executor = "grpcs://rbe.example.com"
platform = { OSFamily = "linux", Arch = "x86_64", container-image = "docker://...@sha256:..." }
[strategy]
default   = "remote"       # remote | cache-only | local
workspace = "local"        # keep incremental for crates you're editing
download  = "toplevel"     # minimal | toplevel | all
[paths]
mode = "canonical"         # or "identity" per package
```

Documentation deliverables, written alongside the code rather than at the end:

- `README`: quickstart (cache-only first), the same-container requirement, and how to read the JSONL action log.
- `docs/config.md`: every key above, defaults, and which strategy applies to what.
- `docs/hermeticity.md`: path modes, env allowlist, `extra inputs` overrides, and `do_not_cache` policy.
- `docs/troubleshooting.md`: how to interpret fallback reasons from the action log.
- `docs/ci.md`: the GitHub Actions setup for BuildBuddy, including secret handling (section 14).

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

The linker, libc and `cc` come from the pinned container image, set as a platform property. This follows reninja's advice to configure inside a container and treat its toolchain as the declared environment. Platform properties belong on the Action itself as of API v2.2. One subdirectory per upstream unit makes a unit's `deps/` a list of stable child digests. Rustc still finds everything through repeated `-L dependency=` flags.

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
- Server choice matters: bazel-remote is listed without remote execution, so it fits the cache-only phase but not later ones.

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
- BuildBuddy-specific behavior, in particular what is needed for an invocation page (see section 15).

## 15. Delivery plan and test harness (requested workflow)

### Working agreement

- Deliverables are incremental and atomic. Each chunk is presented with `present_files`, and a tarball of the repo state is created and presented with every chunk.
- Chunks are delivered before hitting token limits, never half-finished.
- The goal is to get something you can run quickly against real REAPI implementations. You send back results (logs, failures, invocation URLs), and the next chunk is built from them.
- This document is updated as the design changes. Your requests from this conversation live in this section.

### Chunk plan

**D0. This document**, presented for approval.

**D1. REAPI smoke test, plus GitHub Actions workflow.**

- A small Rust crate `cargo-rbe` with a `smoke` subcommand. It does the following, printing each step so failures are diagnosable:
  1. `GetCapabilities`: print supported API versions, digest functions, compressors, and whether execution is enabled.
  2. Upload a trivial Command and an input root to the CAS.
  3. `Execute` a small remote action (e.g. a shell command printing `uname -a` and an echo). This is the "do one small thing remotely" test of the API itself.
  4. Fetch stdout, stderr and exit code from the ActionResult.
  5. Re-run the identical action and report whether it was served from the action cache.
  6. Print the invocation ID and, where possible, the invocation URL.
- A GitHub Actions workflow that runs it against BuildBuddy with the API key from a repository secret (e.g. `BUILDBUDDY_API_KEY`), passed to the client as the `x-buildbuddy-api-key` header against `remote.buildbuddy.io` over TLS, as in reninja's README. The workflow must never echo the key, and forks without the secret must skip the job cleanly.

**D2. Cache-only wrapper.** `RUSTC_WRAPPER` that computes canonical Action digests, consults `GetActionResult`, executes locally on a miss and publishes the result. It includes the two-checkout digest-stability acceptance test and the JSONL action log.

**D3. Remote execution of one real rustc unit** (a registry dependency such as `cfg-if`), with eager downloads.

Later chunks follow the rollout in section 13.

### Invocation ID and URL: what to expect (to verify, not assumed)

- Remote execution itself does not create an "invocation page". As I understand BuildBuddy, the invocation view is built from Build Event Stream (BES) data, which is a separate backend call from REAPI. reninja's flags suggest the same split: it takes a BES backend and a results URL alongside the remote cache and executor flags.
- REAPI requests can carry request metadata (a tool invocation ID, action ID, tool name) in a gRPC header. D1 will generate a UUID invocation ID, attach it to every REAPI call, and print the URL pattern `https://app.buildbuddy.io/invocation/<id>`.
- Whether that URL resolves to a populated page with REAPI metadata alone, or whether a minimal BES stream (started/finished events) must also be sent, is unknown. D1 tests the metadata-only variant first. If the page stays empty, the next chunk adds a minimal BES publisher.
- BuildBuddy's platform and container-image property names, and whether execution needs a `container-image=docker://...` property, will be checked against real behavior in D1 and fixed from your results.

### What I need from you after D1

- The workflow run log (or the printed output of `cargo-rbe smoke`).
- Whether the invocation URL showed anything.
- The `GetCapabilities` output (API versions, compression, digest functions).
- Any error text from the remote step, especially container or platform errors.

## 16. Sources

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
