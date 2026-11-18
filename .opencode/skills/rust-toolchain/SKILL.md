---
name: rust-toolchain
description: "Deploy a pinned, sha256-verified Rust toolchain to an area passed by flag or env var (RBE_RUST_PREFIX), and run unmodified scripts against it via PATH; trigger keywords: deploy rust toolchain, restore rust-home, rustc too old, missing cargo, RBE_RUST_PREFIX, run two-checkout-test.sh with toolchain, toolchain for drop test scripts."
---

# Rust toolchain deployment (cargo-rbe)

`rust-toolchain.sh` in this directory deploys a pinned Rust toolchain to a prefix and exposes it via PATH. Purpose: Claude-provided test scripts from drops (e.g. `scripts/two-checkout-test.sh`) call bare `cargo`/`rustc` and must not be modified — give them the toolchain through PATH instead, so every new drop's scripts work unmodified on repeated imports.

## Usage
- Deploy: `.opencode/skills/rust-toolchain/rust-toolchain.sh deploy` (idempotent; default prefix `/tmp/opencode/rust-home`, version 1.91.1)
- Override area/version: `--prefix DIR`, `--version VER`, or env vars `RBE_RUST_PREFIX` / `RBE_RUST_VERSION`
- Run an unmodified drop script against it (from repo root): `.opencode/skills/rust-toolchain/rust-toolchain.sh run bash scripts/two-checkout-test.sh --remote-cache grpc://127.0.0.1:9092`
- Or for your own shell: `eval "$( .opencode/skills/rust-toolchain/rust-toolchain.sh env )"`

## Layout under the prefix
Installed via the tarball's own install.sh into `rust-<ver>-<target>/usr/local/`, with `bin -> rust-<ver>-<target>/usr/local/bin`. Idempotent: skips the download when `bin/rustc --version` already reports the pinned version. (The 1.9x component-style dist tarballs have no usable bare bin/ — install.sh is required; see d1-local-smoke.)

## Safety / pre-approval
- Writes only under the prefix; downloads are sha256-verified against static.rust-lang.org's `.sha256` sidecar before extraction; refuses to overwrite a non-symlink `bin` path.
- The cargo-rbe-importer agent may be pre-approved for `deploy`/`env` only. `run` stays gated by design: it execs arbitrary commands, so pre-approving it would bypass the denylist.

## Notes
- `/tmp` prefixes are ephemeral — redeploy is one command after a wipe; point `RBE_RUST_PREFIX` at a durable area (e.g. `~/.local/share/cargo-rbe/rust`) for repeated imports.
- `CARGO_HOME` stays default (`~/.cargo`), reusing the existing registry cache so builds are fast.
