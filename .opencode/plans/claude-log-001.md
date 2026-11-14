Design an approach for Cargo to distribute build process using Remote Build Execution API to make efficient use of CAS, break down a project into multiple concurrent tasks with well defined inputs and outputs while still using Cargo to trigger a build. 

Take a look at the available APIs: [bazelbuild/remote-apis: An API for caching and execution of actions on a remote system.](https://github.com/bazelbuild/remote-apis)

[Docs · bazel/remote-apis](https://buf.build/bazel/remote-apis/docs/main%3Abuild.bazel.remote.execution.v2#build.bazel.remote.execution.v2.DirectoryNode)

Inspiration are some examples of non-Bazel projects that use RBE: [BuildGrid / recc · GitLab](https://gitlab.com/BuildGrid/recc) -- https://github.com/buildbuddy-io/reninja (and given CMake can generate Ninja files, then Reninja is usable for CMake projects; an important caveat is that the recommendation is to use the same container that the RBE environment will use as well, see the reply comment: ['device or resource busy' for 'unlinkat' during even a most basic invocation of GCC · Issue #22 · buildbuddy-io/reninja](https://github.com/buildbuddy-io/reninja/issues/22#issuecomment-4014988207) )


# Response

I'll start with the sources you linked, then check what Cargo and rustc currently expose that a design like this can build on.

Cargo stays the planner and scheduler, and the build plugs in at `RUSTC_WRAPPER`. Every rustc invocation Cargo schedules becomes one REAPI Action, and build scripts get a small shim that does the same. A long-lived local daemon owns the gRPC connections and digest caches. `cargo build` stays the entry point. `--unit-graph` is only a prefetch hint, never the source of truth.

I chose this over exporting a plan and executing it elsewhere for two reasons. Cargo removed `--build-plan` entirely in 1.93. The Cargo team points to plumbing commands, `--unit-graph` and structured logging as its replacements. A unit graph carries only the unit DAG and per-unit metadata, so an external tool has to reimplement rustc command-line construction itself. The wrapper route lets Cargo keep resolving features, env vars and flags, which is also the stance cargo-reapi takes: Cargo remains the planner and the wrapper observes the exact compiler commands it schedules.

## What the prior art settles

- **reninja** sidesteps absolute-path problems by changing the remote container's working directory so CMake's absolute paths stay valid, and it advises running cmake inside the same container used for remote execution. It also solves undeclared inputs with a two-stage cache lookup: explicit inputs map to a discovered list of implicit deps, and those deps' hashes map to the output. Rust has the same problem, because modules and `include_str!` files aren't declared up front.
- **recc** is the right pattern (a wrapper client), but the project is marked obsolete and merged into buildbox, so I'd borrow the idea rather than the code.
- **cargo-reapi** is the closest Rust analogue and a useful warning. Its qualified public result is still a shared local cache, remote link actions are ineligible until the native input set can be represented, and its REAPI adapter hasn't been validated against a live service.
- **rules_rust and Buck2** hit the pipelining problem. Bazel can't unblock dependents mid-action, so metadata and full compilation need two compiler invocations, and `--emit=metadata` and `--emit=metadata,link` yield different `.rmeta` files. Newer rules_rust work uses `-Zno-codegen` hollow rlibs, as Buck2 does, which needs `RUSTC_BOOTSTRAP=1` on stable.

## What I verified

I built a small workspace (path deps, a proc macro, a build script, one registry dep) from two different checkout paths with a logging wrapper. This was Cargo and rustc 1.75, the only toolchain I could install, so please re-run it on yours.

- After rewriting only the checkout root, the invocations were identical. The only differences were the jobserver fds inside `CARGO_MAKEFLAGS` (which must be excluded from the env) and scheduling order. `-C metadata` and `extra-filename` matched across checkouts, so canonical Action digests are feasible.
- Cargo passes absolute paths in `--out-dir`, `-L dependency=`, `--extern`, `-C incremental=`, `CARGO_MANIFEST_DIR` and `OUT_DIR`. Workspace members get relative source paths, while registry crates get absolute ones.
- Library deps arrive as `.rmeta` and proc macros as `.so`.
- dep-info lists the files rustc actually read, including the generated file in `OUT_DIR`. It also records `# env-dep:` lines, including an absolute `CARGO_MANIFEST_DIR` when `env!` is used. That is both the hook for a two-stage lookup and the hazard for path canonicalization.

## Architecture

```
cargo build -j128                      (user-facing, unchanged)
   │  RUSTC_WRAPPER=cargo-rbe-wrap
   ▼
wrapper (thin) ──unix socket──▶ cargo-rbe daemon ──gRPC──▶ CAS · ActionCache · Execution
   │ ineligible?                  ├ digest index: unit outputs → Digest
   ▼                              ├ stat cache: (path,ino,mtime,size) → Digest
 real rustc (local)               └ in-flight dedup, upload batching
build.rs binary = shim ─────────▶ same daemon (remote-runs the real build script)

prefetch: cargo build --unit-graph + cargo metadata → daemon pre-uploads toolchain + registry trees
```

The wrapper is just a process spawn per unit, so all state lives in the daemon. Cargo's `-j` still bounds concurrency. For remote builds set it far above core count, since wrapper processes mostly wait. An early Cargo issue on this topic argued it would eventually need to live in Cargo proper because it affects how many jobs can run. That is the long-term upstream path (see rollout).

User-facing configuration would be a sketch like this:

```toml
# .cargo/config.toml
[build]
rustc-wrapper = "cargo-rbe-wrap"
jobs = 128

# .cargo-rbe.toml
[remote]
executor = "grpcs://rbe.example.com"
platform = { OSFamily = "linux", Arch = "x86_64", container-image = "docker://…@sha256:…" }
[strategy]
default   = "remote"       # remote | cache-only | local
workspace = "local"        # keep incremental for crates you're editing
download  = "toplevel"     # minimal | toplevel | all
[paths]
mode = "canonical"         # or "identity" per package
```

## Decomposition into actions

| Cargo unit | REAPI action | Key inputs | Outputs |
|---|---|---|---|
| build.rs compile (host) | rustc, bin | build.rs, dep rlibs, native toolchain | executable |
| build.rs run | shim → exec of real binary | exe, package tree, filtered env | `OUT_DIR` dir, stdout (directives), stderr |
| proc-macro compile (host) | rustc, proc-macro | sources, dep metadata closure | `.so` |
| lib compile | rustc, split in two (below) | sources, `OUT_DIR`, dep metadata closure | `.rmeta`, `.rlib`, `.d` |
| bin/test/cdylib link | rustc, link | sources, dep rlib closure, native toolchain | executable |

The build-script shim is my proposal. I found no prior art for it in the sources I checked. When rustc compiles `build_script_build`, the wrapper runs it remotely. It then writes a tiny shim executable at the path Cargo expects, carrying the real binary's digest. When Cargo runs it, the shim packages Cargo's env (`OUT_DIR`, `CARGO_FEATURE_*`, `DEP_*`, …), executes remotely, relays stdout and stderr, and materializes `OUT_DIR`. It also reverse-maps virtual paths inside `cargo:` directives back to local ones.

## Well-defined inputs: layered Merkle roots

```
/                           input root
├─ tc/                      rustc + sysroot            (one digest per rustc version)
├─ src/registry/<crate>-<ver>/   immutable; digest memoized by Cargo.lock checksum
├─ src/ws/<pkg>/            workspace package; stat-cached
├─ deps/<crate>-<hash>/     one DirectoryNode per upstream unit (rmeta or rlib)
└─ gen/<pkg>/out/           OUT_DIR from the build-script ActionResult
```

The linker, libc and `cc` come from the pinned container image, set as a platform property. That follows reninja's advice to configure inside a container and treat its toolchain as the declared environment. Platform properties belong on the Action itself as of API v2.2. Using one subdirectory per upstream unit makes a unit's `deps/` just a list of stable child digests. Rustc still finds everything through repeated `-L dependency=` flags.

**Two-stage input scan.** Sending the whole package directory is sound but gives false misses, because editing a README invalidates the compile.
1. The first run uses the full package tree.
2. The wrapper reads dep-info and stores a small manifest (the files read plus `env-dep` vars) under a stage-1 key, namely the hash of command, entry file and dep digests.
3. Later runs build a precise input root from the manifest.
4. If a precise run fails on a missing file, retry with the full tree.

Trimming is only unsafe for proc macros that read untracked files and degrade silently. Those get a per-package `extra inputs = [...]` override or stay on full-tree mode.

## CAS efficiency: the per-action flow

1. Compute the Action digest from the digest index alone, with no uploads.
2. Call `GetActionResult`. On a hit you're done, with zero bytes uploaded.
3. On a miss, run `FindMissingBlobs` over the closure. Prefetch has usually already uploaded the toolchain and registry trees. Upload only what's missing, using `BatchUpdateBlobs` for small blobs and ByteStream for large ones. Negotiate the digest function and compression through `GetCapabilities`.
4. Execute, then record output digests in the index.
5. Materialize outputs according to the download policy.

Other levers:
- Memoize Directory protos per unit.
- Feed libraries `.rmeta` (or hollow rlibs) instead of full rlibs wherever linking isn't needed.
- Keep output paths sorted and deduplicated, because the API requires that for consistent Action hashing.
- Prefer directory outputs that can feed downstream actions without the client re-uploading Directory protos. Newer API revisions have an output-format option for this, but check server support via capabilities. I'm recalling that from the proto, not verifying it.
- The Remote Asset API maps URIs to CAS digests and lets a server fetch dynamically, which could let servers pull crate sources directly. Support varies by server.

Server choice matters. bazel-remote is listed without remote execution, so it is fine for the cache-only phase but not for later phases.

## Hermeticity: paths, env, platform

- **Paths.** The default is canonical mode: map the workspace, registry, target dir and toolchain onto fixed virtual prefixes, add `--remap-path-prefix`, and rewrite dep-info back to local paths on return, since Cargo's fingerprints read it. That gives cross-user cache hits. Identity mode mounts the same absolute path remotely (reninja's approach). It is needed for crates that use `env!("CARGO_MANIFEST_DIR")` for runtime lookups, which my experiment confirmed gets baked into the output.
- **Env.** Use an allowlist (`CARGO_*`, `OUT_DIR`, target vars), sorted, with `HOME` and `TMPDIR` pinned and the jobserver vars dropped. Cross-check against the `env-dep` lines. Never ignore a variable just to raise the hit rate. cargo-reapi's dogfooding showed that widening the ignore list improves hits while opening a route to stale outputs.
- **Platform parity.** In v1 the host triple must equal the worker's. Proc macros and build scripts are host artifacts, and Cargo takes the host from the local rustc. The clean way is to run Cargo in the same container image the workers use, as the reninja maintainer recommends.
- **Non-hermetic build scripts** (embedding timestamps or git state) should be marked `do_not_cache` or kept local by policy.

## Pipelining and parallelism

REAPI actions return outputs only at completion, so Cargo's single-process pipelining can't carry over directly. The wrapper splits each library into a metadata action and a full-codegen action. When the metadata action completes, the wrapper prints rustc's `{"artifact":…,"emit":"metadata"}` JSON notification to stderr, so Cargo unblocks dependents. It exits only after the codegen action finishes. Two options for the metadata step:

- **Option A:** `--emit=metadata`, which is cheap but duplicates frontend work. Pick one `.rmeta` flavor and verify empirically that the SVH matches what the final rlib carries.
- **Option B:** a hollow rlib via `-Zno-codegen`, which matches Buck2 but requires nightly or `RUSTC_BOOTSTRAP`.

I'd start with A and make B opt-in.

## Outputs and materialization

- **Replay diagnostics.** Store rustc's JSON stderr in the ActionResult and replay it on a hit, so a fresh checkout still shows warnings.
- **Incremental compilation.** Strip `-C incremental` from remote actions, because its state is path-bound and stateful. rules_rust's incremental support needed Bazel-managed tree artifacts so the path is identical on local and remote workers. Instead use the `workspace = "local"` hybrid: dependencies and build scripts are remote and cached, while the crates you edit compile locally with incremental.
- **Builds without the bytes.** Keep only digests in the index and write stub files at the paths Cargo expects. Materialize on demand for local fallback and top-level binaries, as Bazel's download modes do. This is risky because rustdoc, rust-analyzer and local fallback all read real files, so it comes late.
- **Cargo layout.** Derive everything from rustc's arguments (`--out-dir`, `--emit`, `--extern`, `-L`), not from Cargo's directory layout. `build-dir` was stabilized in 1.91, and its internal layout is explicitly an implementation detail that may change.

## Failure model

The wrapper is fail-closed. It rejects unknown rustc flags, paths it can't map, env it can't classify and platform mismatches. In those cases it runs the real rustc locally and logs the reason as JSONL. The log doubles as your tuning signal for what to make eligible next.

## Rollout

0. **Cache-only.** Execute locally and publish ActionResults. This validates key stability with no execution risk. The acceptance test is that two different checkout paths produce identical Action digests for the same unit.
1. **Remote execution of registry dependencies** with eager downloads, running Cargo in the worker image so local build scripts see the same environment.
2. **Remote build scripts via the shim, and proc macros.**
3. **Metadata/codegen split and the two-stage input scan.**
4. **Workspace crates, link actions, tests, clippy and check, then stub-based minimal downloads.**
5. **Upstream.** Propose a Cargo executor hook so Cargo can size parallelism itself and skip the wrapper hop.

## Open risks

- Link actions are the hardest part. Native inputs (system libs, SDKs, response files) are why cargo-reapi hasn't made them remote-eligible.
- Untracked file reads by proc macros.
- The SVH consistency between the metadata and codegen actions.
- Cross-OS workers (macOS laptop to Linux workers), which this design doesn't solve.
- Rustc flag drift across toolchains, so the arg parser must be conservative.

I can also make this a doc you can edit here, share, or download as Word or PDF, if you'd like.
