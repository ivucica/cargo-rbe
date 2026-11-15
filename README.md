# cargo-rbe (D1: REAPI smoke test)

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

Exit code: 0 all fine, 1 a step failed, 2 configuration or connection error.
`--json-out` writes a report with no header values (safe to paste back).

## Configuration

Flags (Bazel-style names): `--config`, `--config_file`, `--remote_executor`, `--remote_cache`,
`--remote_instance_name`, `--remote_header`, `--remote_timeout`,
`--remote_default_exec_properties`, `--tls_certificate`, `--tls_client_certificate`,
`--tls_client_key`, `--results_url`, `--bes_backend` (accepted, not used yet).

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
| `Execute` on any other server; platform properties; a populated BES invocation page | **not yet** |
| TLS (`grpcs://`), custom headers on a real service | **not yet** |
