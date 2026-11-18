---
name: rust-toolchain
description: "Provide a Rust toolchain for builds and unmodified drop test scripts: uses the system rustc as-is when it satisfies the project minimum (1.85), otherwise deploys a pinned, sha256-verified toolchain to an area passed by flag or env var (default pin 1.85.1; default location XDG cache basedir/.rust-toolchain-cache) and exposes it via PATH; trigger keywords: deploy rust toolchain, restore rust-home, rustc too old, missing cargo, RBE_RUST_PREFIX, .rust-toolchain-cache, run two-checkout-test.sh with toolchain, toolchain for drop test scripts."
---

# Rust toolchain deployment (cargo-rbe)

`rust-toolchain.sh` in this directory provides a Rust toolchain for builds and test scripts. If the system `rustc` already satisfies the required version (default: project minimum 1.85, per Cargo.toml `rust-version`), every subcommand uses it as-is and installs nothing. Otherwise it deploys a pinned, sha256-verified toolchain to a prefix and exposes it via PATH. Purpose: Claude-provided test scripts from drops (e.g. `scripts/two-checkout-test.sh`) call bare `cargo`/`rustc` and must not be modified — give them the toolchain through PATH instead, so every new drop's scripts work unmodified on repeated imports.

## Prefix resolution
1. `--prefix DIR` flag
2. `RBE_RUST_PREFIX` env var (use this to point at a permanent archive, e.g. on the NAS)
3. Default: XDG cache basedir + `/.rust-toolchain-cache` — resolved with python3 via pyxdg (`xdg.BaseDirectory.xdg_cache_home`, package python3-xdg) when installed, else `$XDG_CACHE_HOME` or `~/.cache`. Python is used only for this resolution; the rest of the script is bash.

## Usage
- Deploy: `.opencode/skills/rust-toolchain/rust-toolchain.sh deploy` — no-op when the system rustc satisfies >= 1.85.1 (prints a skip notice); otherwise installs the default pin **1.85.1** (sha256-verified, docs excluded). Force another version with `RBE_RUST_VERSION=1.91.1 ... deploy`.
- Run an unmodified drop script against it (from repo root): `.opencode/skills/rust-toolchain/rust-toolchain.sh run bash scripts/two-checkout-test.sh --remote-cache grpc://127.0.0.1:9092`
- Or for your own shell: `eval "$( .opencode/skills/rust-toolchain/rust-toolchain.sh env )"`

## Pinned downloads (hash table)
| file | size (bytes) | sha256 | source of truth |
|---|---|---|---|
| rust-1.85.1-x86_64-unknown-linux-gnu.tar.gz | 339,058,449 | b7202563a52b47f575b284a5a4794fafd688e39bfe8fd855b5e80129e671cb7f | official .sha256 sidecar on static.rust-lang.org (fetched 2026-11-18; download not yet exercised — the system toolchain suffices on this host) |
| rust-1.91.1-x86_64-unknown-linux-gnu.tar.gz | 371,298,415 | 1c955c040dd087e4751d15588ddec288b4208bea16f8ec5046c164877e55fff7 | official .sha256 sidecar on static.rust-lang.org — MATCH (verified 2026-11-15, re-verified by script deploy 2026-11-18) |

The single dist tarball contains both rustc and cargo — there is no separate cargo download. The script verifies the built-in pin for these versions without network; other versions fall back to the official `.sha256` sidecar (or `RBE_RUST_SHA256`). When bumping a pinned version, add a row here AND mirror it in the script's `pinned_sha256()`.

## Layout under the prefix
Installed via the tarball's own install.sh into `rust-<ver>-<target>/usr/local/`, with `bin -> rust-<ver>-<target>/usr/local/bin`. Idempotent: skips the download when `bin/rustc --version` already reports the pinned version. (The 1.9x component-style dist tarballs have no usable bare bin/ — install.sh is required; see d1-local-smoke.)

## Components and disk space
- Installs all dist-tarball components except `rust-docs` by default (~780 MB of HTML docs — useless for building). Override with `RBE_RUST_WITHOUT` (empty = everything, incl. docs); component names are listed in the tarball's `components` file.
- Before downloading, the script checks free space on both the prefix partition and the scratch partition against expected sizes (measured 2026-11-18: full install 1.6 GB, without docs ~850 MB; scratch needs ~2 GB) and fails early with numbers.
- Downloaded tarballs are cached in `$RBE_RUST_TARBALL_DIR` (default `<scratch>/rust-toolchain-tarballs`) and reused across runs — a second deploy never re-downloads. Every use is still sha256-verified; on hash mismatch the script refuses to proceed rather than deleting the entry (no automatic deletion). Point `RBE_RUST_TARBALL_DIR` at a NAS path for persistence or sharing between machines.

## Safety / pre-approval
- Final toolchain writes only under the prefix; download/staging scratch goes to `$RBE_RUST_SCRATCH`, else `$TMPDIR`, else `/tmp` — never under the prefix, so a nearly-full cache partition doesn't double its peak usage. Downloads are sha256-verified before extraction; refuses to overwrite a non-symlink `bin` path.
- `--replace` deletes an existing install dir. An agent MUST ask the user via the question tool before using it — no implied consent, never assume deletion is free (even under .cache, even if the dir looks like a partial/failed install). The permission layer gates it too: pre-approved patterns are `deploy*`/`env*`, and `--replace deploy` matches neither, so an approval prompt fires regardless.
- The cargo-rbe-importer agent may be pre-approved for `deploy`, `env`, and `run cargo *` only. Bare `run` stays gated by design: it execs arbitrary commands, so pre-approving it would bypass the denylist.

## Notes
- This host (Debian trixie) ships `rustc 1.85.1 (4eb161250 2025-03-15) (built from a source tarball)` — it satisfies the project minimum, so on this machine every subcommand uses the system toolchain and no install happens.
- Cache-basedir prefixes survive reboots; for a permanent shared archive set `RBE_RUST_PREFIX` to a NAS path (deploy is idempotent there too).
- `CARGO_HOME` stays default (`~/.cargo`), reusing the existing registry cache so builds are fast.
