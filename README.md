# cargo-rbe (D3: build scripts, the rustc wrapper, and the REAPI smoke test)

Minimum supported Rust: **1.85** (Debian trixie). `rust-version` and `resolver = "3"` keep `cargo update` from
selecting crate versions that need a newer compiler; CI builds on 1.85.

Experimental. Part of the design in `cargo-rbe-design.md` (Cargo as planner, REAPI as the
execution and cache layer). D1 contains only the **smoke test**: it exercises the REAPI calls the
design depends on against *any* server and reports exactly what that server does. It does not
touch Cargo yet (D2 adds the cache-only `RUSTC_WRAPPER`).

Nothing is tied to one service: endpoints, headers, TLS, instance name, platform properties and
the results URL are all configuration.

## Build

```
cargo build --release          # no protoc needed; generated code is checked in (src/proto/)
```

## Run

```
cargo-rbe smoke \
  --remote_executor=grpcs://rbe.example.com \
  --remote_cache=grpcs://rbe.example.com \
  --remote_header=x-example-auth=@env:RBE_AUTH \
  --remote_default_exec_properties=OSFamily=linux \
  --results_url='https://ui.example.com/invocation/{invocation_id}' \
  --json-out report.json
```

Cache-only server (no executor): pass only `--remote_cache`; execution steps are skipped.
Plaintext local server: `grpc://host:port`. TLS: `grpcs://` (system and bundled roots), plus
`--tls_certificate` for a private CA and `--tls_client_certificate/--tls_client_key` for mTLS.

Steps, each reported as `ok`, `skip` or `FAIL` (independent where possible):

| Step | What it checks |
|---|---|
| `capabilities` | `GetCapabilities`: API versions, digest functions, compressors, execution/AC-update flags |
| `cas-batch` | `FindMissingBlobs`, `BatchUpdateBlobs`, `BatchReadBlobs`, deduplication on re-upload |
| `cas-bytestream` | `ByteStream.Write`/`Read` of a ~1 MiB blob, verified by hash |
| `action-cache-roundtrip` | `GetActionResult` miss, `UpdateActionResult`, `GetActionResult` hit |
| `execute` | uploads a tiny Action (`/bin/sh -c 'uname -a; ...'`), `Execute`, fetches stdout and `out.txt` from the CAS |
| `execute-again` | same Action again; reports whether `cached_result` was true |

| `bes` | only if `--bes_backend` is set: publishes the invocation as a Build Event Stream (see below) |

Exit code: 0 all fine, 1 a step failed, 2 configuration or connection error.
`--json-out` writes a report with no header values (safe to paste back).

## Cache-only rustc wrapper (D2)

```
cargo build --release                                   # builds cargo-rbe and cargo-rbe-wrap
cargo-rbe --config=remotecache cargo build              # your normal cargo command, wrapped
cargo-rbe --remote_cache=grpc://host:9092 cargo --mode=read-only test
```

`cargo-rbe cargo <args>` runs `cargo <args>` with `RUSTC_WRAPPER=cargo-rbe-wrap`, passes the resolved
settings and one invocation ID to the wrapper, and prints a summary afterwards. Cargo stays the
planner and scheduler; nothing else about your build changes. The flags and config from the smoke
test apply (`--remote_cache`, `--remote_header`, `--config`, `--bes_backend`, ...).

For each rustc invocation the wrapper builds a canonical REAPI Action (command line with paths
mapped to input-root-relative form, an allowlisted environment, a Merkle input root made of the
package directory, the dependency closure and `OUT_DIR`), then:

- **hit**: restores `.rlib`/`.rmeta`/`.d`, rewrites paths inside `.d`, replays rustc's diagnostics
  (so warnings appear on a hit too), exits 0;
- **miss**: runs the real rustc locally, then uploads the outputs and the `ActionResult`
  (mode `cache` only);
- never fails a build because of the cache: any remote error means "run rustc locally".

Modes (`--mode`): `cache` (default), `read-only` (lookup, never publish), `digest-only` (compute and
log Action digests, no network at all), `off`.

**What is cached in this version:** library compilations (`lib`/`rlib`, emitting `dep-info`,
`metadata`, `link`) of registry crates and workspace crates, including crates with build scripts
(the build script's `OUT_DIR` content is part of the key). **Not cached, and logged with the reason:**
binaries, tests, proc macros, build scripts (their compile), anything with `-l`/`-L native`,
and crates whose dependency closure is unknown (built before the wrapper saw them).

**Things worth knowing**
- Every compile of a registry crate, cached or not, gets `--remap-path-prefix` for the registry
  directory (`/cargo-registry`) and for the working directory (`/workspace`), unless `--remap none`.
  Without it the artifacts embed `CARGO_HOME` and the working directory: the `.rmeta` differed per
  location, and a locally built proc macro `.so` did too, which made every dependent miss. Paths of
  registry crates in panics and debuginfo therefore show `/cargo-registry/...` or `/workspace`. Workspace
  crates are not remapped.
- A crate that reads `CARGO_MANIFEST_DIR` through `env!` is never published (its output may embed
  the path), and so never hits. Any other environment variable a crate reads must be in the key,
  otherwise the result is not published. Inputs rustc reports (dep-info) that fall outside what was
  hashed also block publishing. All of it is logged.
- The package directory is hashed as a whole (minus `target/`, `.git`), which is sound but coarse:
  for a root package that contains other workspace members, editing a member invalidates the root.
  A precise two-stage lookup is planned, not implemented.
- Hashing uses a persistent (path, size, mtime, inode) cache under the cache directory
  (`$CARGO_RBE_CACHE_DIR`, default `~/.cache/cargo-rbe`), as does the local index of dependency closures.
- `CARGO_RBE_WRAP_SETTINGS` (the resolved config, including header *specs*) is placed in Cargo's
  environment, so build scripts and proc macros can read it. Use `@env:`/`@file:` header references
  rather than literal values, as everywhere else.
- `OUT_DIR` must lie inside the target directory (where Cargo puts it); otherwise the unit is not cached.
- Unix only. The rustc version is part of the key (`rustc -vV`, computed once per `cargo-rbe cargo` run).

**Log.** One JSON line per compile invocation (`--log`, default
`<cache dir>/actions-<invocation>.jsonl`): crate, outcome (`hit`, `miss_published`, `miss_not_published`,
`local`, `local_only`, ...), the reason when ineligible or not published, the Action digest, and the key
components (toolchain, command, input root, package, deps, closure) so two digests can be compared.
With `--bes_backend` the log is attached to the invocation and the summary appears as console output.

## Build scripts (D3)

```
cargo-rbe --config=remote cargo --build-scripts=cache  build     # run locally once, publish, reuse everywhere
cargo-rbe --config=remote cargo --build-scripts=remote build     # execute on the executor, reuse everywhere
```

Once libraries are cached, build-script execution dominates builds with native dependencies (fd's jemalloc
build script was about 94 of 108 seconds). With `--build-scripts` the wrapper also handles them. Cargo offers no
hook around running a build script, so after rustc compiles `build_script_build` the wrapper moves the real
executable aside (`*.rbe-real`) and puts a **shim** at its path: a hard link to `cargo-rbe-wrap` (touched so its
mtime is current, otherwise Cargo thinks the unit is stale; a copy if linking is not possible) plus
`rbe-shim.json`. When Cargo runs the shim:

1. it builds an Action: the real executable, the package directory (minus `target/`, `.git`), an empty `out/`,
   and a command that runs it with `OUT_DIR=out` as an output directory;
2. a cache hit restores `OUT_DIR` and replays the script's stdout/stderr, so Cargo sees exactly what a local run
   would have printed;
3. on a miss, `cache` runs the script locally and publishes `OUT_DIR` and the output, `remote` uploads the inputs
   and executes the Action on the executor (the server also caches the result);
4. on any problem (server unreachable, missing tool on the worker, non-zero exit, unusable result) the real
   executable runs locally: a build never fails because of this.

**Environment.** The Action sees only: Cargo's variables (`CARGO_PKG_*`, `CARGO_FEATURE_*`, `CARGO_CFG_*`,
`CARGO_ENCODED_RUSTFLAGS`, `TARGET`, `HOST`, `PROFILE`, `OPT_LEVEL`, `DEBUG`, `DEP_*`, ...), well-known toolchain variables
(`CC`, `CXX`, `AR`, `CFLAGS`, `PKG_CONFIG_*`, `SOURCE_DATE_EPOCH`, ...; extend with `--env-passthrough A,B`),
`RUSTC=rustc`, a fixed `NUM_JOBS`, and a `PATH` of the usual system directories preceded by `--build-script-path`
(default: the `bin` directory of rustc's sysroot, the real compiler, not a rustup proxy). A value that points into this
machine's build directories (for example a `DEP_*` variable naming another crate's `OUT_DIR`) makes the script
ineligible, logged with the reason. After a run, a script that declares `rerun-if-env-changed=NAME` for a variable that
is set here but was not forwarded is not trusted: the result is discarded and the script runs locally.

**Paths.** A worker's directory layout is unknown, so the command prints its root as a first line and paths in the
script's output below that root are mapped back to this machine's `OUT_DIR` and package directory. Generated *files* are
not rewritten, so a script that bakes absolute paths into its outputs bakes the worker's.

**Requirements for `remote`.** The worker image needs `/bin/sh`, the C toolchain the scripts use, and rustc in the
`PATH` you pass (same version: the toolchain identity is in the key). Select the image with
`--remote_default_exec_properties` (they are part of the Action). Local-run (`cache`) results are keyed with the C
compiler's identity (`$CC --version`); remote results use a separate key namespace.

**Limits, stated plainly.** Not detected: scripts that read environment variables without declaring them, files
outside the package directory, or the network; non-deterministic scripts (timestamps, git hashes) are cached with their
first result. Use `--build-script-skip pkg1,pkg2` to keep such packages local. `OUT_DIR` over 512 MB is not published.
A workspace crate's build script is compiled with `--remap-path-prefix` for the working directory when `--build-scripts`
is on (so the executable is identical across checkouts); compile errors in that `build.rs` then show `/workspace/...`.

Log: records with `"kind":"build_script"` and outcomes `bs_hit`, `bs_remote`, `bs_published`, `bs_local`,
`bs_fallback` (with the reason), `bs_local_only` (ineligible, with the reason), `bs_local_failed`.
`scripts/build-script-test.sh --mode cache|remote ...` is the acceptance test (see below).

**On a real project.** `scripts/real-project-test.sh GIT_URL REF [cargo-rbe flags]` clones the project twice
into different directories (own `CARGO_HOME`, own empty target dir), builds it through the wrapper in
each, and prints what was cached, what hit, what could not be cached and why, which units missed in the
second build and which dependency file differed, and roughly how long the build scripts took. Logs go
to `$RESULTS_DIR` (default `./real-project-out`) and contain no header values:

```
scripts/real-project-test.sh https://github.com/BurntSushi/ripgrep.git 15.2.0 \
  --remote_cache=grpcs://rbe.example.com --remote_header=@env:RBE_HEADERS
```

In GitHub Actions: Actions, smoke, Run workflow, and fill `project_repo` (and optionally `project_ref`,
`project_cargo_args`); it uses the repository variables and secret of the `remote` job. Note that this
uploads the project's library artifacts (about 100 to 150 MB for a small project in the debug profile)
to the cache. Projects tried so far: ripgrep 15.2.0 (no proc macros) and fd 10.5.2 (clap derive, jemalloc);
see the design doc for results. Untested suggestions for larger ones: bat, hyperfine, rust-analyzer.

**Acceptance test.** `scripts/two-checkout-test.sh [--remote-cache URL]` builds
`examples/sample-ws` (a registry dependency, a build script, a library, a binary) from two different
directories with different `CARGO_HOME`s: (1) every cacheable unit has the same Action digest in both;
(2) with a cache, units published from one checkout are restored in the other, warnings are replayed,
Cargo considers restored units fresh, and the binaries agree; (3) editing a source file misses and
publishes, the same edit in the other checkout hits. The script unsets `CARGO_TARGET_DIR` (it needs one
target directory per checkout); `EXTERNAL_TARGET=1` puts each checkout's target directory outside the
checkout instead. In CI it runs against a local cache-only server
and against the local BuildBuddy; set the variable `RBE_RUN_WRAPPER_TEST=true` to run it against your
configured cache too.

**Build-script acceptance test.** `scripts/build-script-test.sh --mode cache --remote-cache URL` or
`--mode remote --remote-executor URL` builds the sample (two build scripts, one compiling C with the `cc` crate)
and checks: scripts go through the shim (published, or executed on the executor with the stages shown); a second
checkout restores all of them and the C library links and runs; a rebuild runs nothing; editing the C file reruns
the script and the same edit elsewhere hits; an unreachable server falls back to local runs with a correct binary.

## Build Event Stream (invocation pages)

Some servers build their invocation page from build events, not from REAPI calls. The invocation
ID we send in request metadata links executions to it, but without events the server has no
invocation record and the page says "Invocation not found". With `--bes_backend=<url>` the smoke
test publishes a minimal, well-formed stream (Bazel's event types):

- lifecycle: BuildEnqueued, InvocationAttemptStarted, InvocationAttemptFinished, BuildFinished
  (best effort; reported in `notes` if a server rejects them),
- tool stream: Started, UnstructuredCommandLine, StructuredCommandLine, WorkspaceStatus,
  BuildMetadata, Progress (the smoke output, as console text), BuildFinished, and BuildToolLogs
  (the JSON report attached as `cargo-rbe-report.json`), then ComponentStreamFinished.

Every event is acknowledged by the server or the step fails (`published N events, M acknowledged`).
Header values are never published: the recorded command line replaces literal `--remote_header` and
`--bes_header` values with `<redacted>` (references like `@env:NAME` are kept).

- `--bes_header=name=value`: extra headers for the BES connection only. BES also receives every
  `--remote_header`.
- `--build_metadata=KEY=VALUE` (repeatable): published in the BuildMetadata event, for example
  `VISIBILITY=PUBLIC` on services that default unauthenticated invocations to private.
- Under GitHub Actions, `ROLE=CI`, `REPO_URL`, `COMMIT_SHA` and `BRANCH_NAME` are added to the
  workspace status automatically.
- The structured command line is split into executable / command / options, and an `OptionsParsed`
  event carries `tool_tag=cargo-rbe` (the only tool identity the event stream has; servers show it as
  "Tool tag"). The options also drive badges like "remote execution on".
- `--results_url` is still only a template; the URL printed is where the server says it shows
  invocations, not something this tool can verify.

## Configuration

Flags (Bazel-style names): `--config`, `--config_file`, `--remote_executor`, `--remote_cache`,
`--remote_instance_name`, `--remote_header`, `--remote_timeout`,
`--remote_default_exec_properties`, `--tls_certificate`, `--tls_client_certificate`,
`--tls_client_key`, `--results_url`, `--bes_backend`, `--bes_header`, `--build_metadata`.

Precedence, lowest to highest: `~/.config/cargo-rbe/config.toml`, nearest `.cargo-rbe.toml`,
`CARGO_RBE_*` environment (executor, cache, instance name, timeout, results URL, BES backend,
`CARGO_RBE_CONFIG`), CLI. Within a file: `[common]`, then each `--config=<name>` section
`[config.<name>]`, then the file's `import = [...]` (prefix `?` = ignore if missing).
See `examples/cargo-rbe.toml`. Unknown keys are accepted with a warning (later chunks use them).

Headers: any name; repeatable; sent as gRPC metadata on every call; never printed or logged.
- `name=value`
- `name=@env:VAR` or `name=@file:path` (value read at run time)
- `@env:VAR` or `@file:path` alone: the source holds one or more `name=value` lines, so even
  the header name can live in a CI secret.

The tool always generates an invocation ID and sends it in the standard request-metadata header.
A results URL is printed only if `--results_url` is configured; `{invocation_id}` is substituted.

## Try it against a local cache-only server

```
curl -L -o bazel-remote https://github.com/buchgr/bazel-remote/releases/download/v2.6.4/bazel-remote-2.6.4-linux-amd64
chmod +x bazel-remote && mkdir -p /tmp/brdata
./bazel-remote --dir /tmp/brdata --max_size 2 --grpc_address 127.0.0.1:9092 &
cargo-rbe smoke --remote_cache=grpc://127.0.0.1:9092
```

## Local BuildBuddy stack (cache + execution + BES), no containers

```
sudo apt-get install -y redis-server curl        # redis binary only
scripts/local-buildbuddy.sh start                # downloads v2.325.0 once, waits until the executor registers
cargo-rbe smoke --remote_executor=grpc://127.0.0.1:1985 \
  --results_url='http://localhost:8080/invocation/{invocation_id}'
scripts/local-buildbuddy.sh stop
```

Findings (BuildBuddy v2.325.0, Linux amd64):
- The plain OSS server binary starts and serves cache/ByteStream/ActionCache/BES, but does **not**
  implement the Execution service even with `--remote_execution.enable_remote_exec=true`.
- Execution worked with the release's `*-enterprise-*` server and executor, plus Redis for the
  scheduler (`--remote_execution.redis_target`). Check BuildBuddy's license terms for those
  binaries before using them in your own CI. The executor ran with the bare runner (no isolation).
- On a 1-CPU machine the executor stalled at "Downloading inputs"; setting
  `--cache.client.filecache_link_parallelism` and `--cache.client.input_download_concurrency`
  fixed it (the script sets both; which one matters was not isolated).
- Sending an invocation ID in request metadata is enough for the server to attach the execution
  to it, but no invocation record or page exists until build events (BES) are published.

## CI

`.github/workflows/smoke.yml`: `local-cache-only` and `local-buildbuddy` jobs (no secrets; the latter runs the full stack above and has **not yet run on GitHub Actions**) and a `remote` job that runs
only when repository variables/secrets are configured and the PR is not from a fork. Variable
and secret names are documented at the top of the file.

## Regenerating protobuf code

Protos are pinned in `protos/` (`REMOTE_APIS_COMMIT`, `GOOGLEAPIS_COMMIT`). Regenerate with
`cargo run --manifest-path tools/protogen/Cargo.toml` from the repo root (needs `protoc`).
Generated files in `src/proto/` are checked in so normal builds need neither `protoc` nor `buf`.
The protos are Apache-2.0 licensed (bazelbuild/remote-apis, googleapis).

## Status

| Verified | Where |
|---|---|
| Builds on Rust 1.91; capabilities, CAS batch, ByteStream, ActionCache round-trip, config layering, header indirection, skip-on-no-Execution | locally, against bazel-remote 2.6.4 (cache-only) |
| Full smoke (6/6 steps): `Execute` with stages QUEUED, CACHE_CHECK, EXECUTING, COMPLETED, output fetch from CAS, second run `cached_result=true` | locally, BuildBuddy v2.325.0 enterprise server + executor + Redis (Linux) |
| Full smoke 6/6 with `Execute` and `cached_result=true`, run by the maintainer on GitHub Actions against BuildBuddy cloud and against the local stack, and locally on another Linux host | maintainer reports |
| BES: 16 of 16 events acknowledged; invocation record created (command, success, structured command line, metadata, console log chunks, attached report) | locally, BuildBuddy v2.325.0 |
| BES on BuildBuddy cloud: 16 of 16 events acknowledged, invocation page populated (maintainer's GitHub run and local browser) | maintainer reports |
| D2 acceptance test: digest stability, publish in A / restore in B, warning replay, Cargo freshness, edit -> miss -> hit elsewhere | locally, against bazel-remote 2.6.4; launcher with BES against BuildBuddy v2.325.0 (2 cacheable units published, 2 restored) |
| D3 acceptance test, both modes: cache against bazel-remote and local BuildBuddy; remote on the local BuildBuddy executor (bare runner, same host) | locally |
| fd v10.5.2 with `--build-scripts=remote`: all 11 build scripts (incl. jemalloc's C build) executed on the executor in checkout A, all 11 restored in B; second build 23 s instead of 108 s | locally |
| D3 on a real remote worker pool (different image and host than the client), BuildBuddy cloud, the GitHub Actions run of the new jobs | **not yet** |
| TLS (`grpcs://`), custom headers on a real service | **not yet** |
