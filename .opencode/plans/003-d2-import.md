---
epic: cargo-rbe import
title: D2 import (cache-only RUSTC_WRAPPER)
status: done
created: 2026-11-17
completed: 2026-11-17
priority: P1
---

# 003 — Import of cargo-rbe-D2

## Clear directives (user, 2026-11-17)

> "Restart done and includeBy[6] error fixed. Let's drive the importer on the D2
> package we have now."

Interpretation: formal import of the delivered D2 drop (cargo-rbe-D2.tar.gz +
design v6 + log 007). It supersedes the experimental draft committed as 7776144
"Import experimental D2 RUSTC_WRAPPER draft (will be superseded)". That commit
STAYS in history — no rewrite; rebase/reset/amend are forbidden. New dated
commits land on top of it. Standing rules from 001/002 continue:

- No absolute paths (/wolfpool, /home/) in repo files or commit messages.
- Never modify claude logs, design originals, or tarballs (sacred).
- Message gate per commit; explicit date verification after each dated commit.
- Standing directive (user, 2026-11-17, for ALL future drop imports): commit
  messages must be short human-readable bullet lists (sublists only where
  needed) — no dense paragraphs. The C_b message of this import was approved
  in that list style.

## Source inventory (.opencode/plans/, on-disk mtime +0000)

| File | mtime (+0000) | sha256 / notes |
|---|---|---|
| cargo-rbe-D2.tar.gz | 2026-11-17T10:23:04 | see below — canonical D2 drop |
| cargo-rbe-D2-older.tar.gz | 2026-11-17T10:25:02 | byte-identical duplicate, unused |
| cargo-rbe-design.v6.md | 2026-11-17T10:23:14 | v5→v6 delta below |
| claude-log-007.md | 2026-11-17T10:25:27 | untracked, sacred log for this drop |

- cargo-rbe-D2.tar.gz sha256:
  `4b29a8d3abef2b49fc33edaa88dfb200c4ae2ae4f30f882eea07728125debca9`.
  Top-level cargo-rbe/ dir; extract with --strip-components=1.
- cargo-rbe-D2-older.tar.gz is byte-identical to the above (same sha256,
  verified). NOT used, NOT deleted, NOT modified. Log 007 says an
  earlier-presented tarball was premature and to ignore it — on disk both
  copies are already the final version, so nothing is lost.
- cargo-rbe-design.v6.md delta vs v5 (43 lines): D2 chunk marked done; concrete
  D3 proposal added to the chunk plan; new "Field results for D1.2 and fixes
  (reported 2026-11-16)" subsection; new "D2 status (delivered 2026-11-16):
  cache-only rustc wrapper" subsection.
- claude-log-007.md: on-disk mtime is a copy time (delivered with the tarball),
  not an authoring time — ships inside C_b per pattern.

Tarball delta vs HEAD (7776144), verified by extraction to /tmp/opencode/d2s1/
and diff against `git archive HEAD`:

- MODIFIED: Cargo.toml, .github/workflows/smoke.yml, README.md, src/bes.rs,
  src/config.rs, src/main.rs, src/rpc.rs, src/smoke.rs, src/statcache.rs,
  src/wrap.rs.
- MODIFIED (revert): Cargo.lock reverts to the pre-draft D1.2 state —
  byte-identical to cdfa1d1; drops signal-hook-registry that experimental
  draft 7776144 added.
- ADDED: src/lib.rs + src/wrap_main.rs (crate restructure into a library plus
  two binaries), examples/sample-ws/ (sample workspace: app + samplelib with
  build.rs, its own Cargo.lock), scripts/two-checkout-test.sh.
- UNCHANGED vs HEAD (extraction overwrites byte-identical content; no git
  change): protos/**, tools/protogen/src/main.rs, examples/cargo-rbe.toml,
  scripts/local-buildbuddy.sh, src/proto/*.rs.
- Tarball contains an EMPTY dir tools/protogen/src/proto — untrackable by git,
  a no-op (see decisions).
- .gitignore: the tarball carries its own 4-line version vs our extended one →
  CLOBBER HAZARD on unpack; restore via `git checkout -- .gitignore` afterwards.
  All four tarball rules are already present from the D1/D1.2 merges — verify,
  append only if missing.

## Per-commit spec (chronological by source mtime)

Note: this time the tarball PRECEDES the design doc by 10 s (see decision g).

### C_b: D2 unpack + log 007 — commit time 2026-11-17T10:23:04 +0000
(= tarball mtime)

Staging steps (from repo root):

1. `tar xzf .opencode/plans/cargo-rbe-D2.tar.gz --strip-components=1`
   (mtimes preserved by default).
2. `git checkout -- .gitignore` (clobber restore; verify our rules survived).
3. Stage exactly the MODIFIED + ADDED paths listed above, plus
   `.opencode/plans/claude-log-007.md` in place.
4. Never stage this plan file or d*-info artifacts.

Message draft — subject: `Import D2: cache-only RUSTC_WRAPPER, crate
restructure, wrapper CI job`; body (paragraphs as committed):

> Restructures the crate into a library plus two binaries: cargo-rbe
> (launcher; `cargo-rbe --config=… cargo build`) and cargo-rbe-wrap (the
> RUSTC_WRAPPER). The wrapper builds a canonical Action for each library
> compile, looks it up via GetActionResult, restores on hit; on miss runs rustc
> locally and publishes the result; remote errors always fall back to local.
> Modes: cache, read-only, digest-only, off. Caches registry and workspace
> library crates including ones with build scripts; binaries, proc macros,
> build-script compiles and native-link-flag units are not cached (reason
> logged per unit). Adds a persistent stat/digest cache, registry-path remapping
> for location-independent digests (--remap none to disable), and a local
> dependency-closure index.

> D1.2 fixes from the maintainer's field results: sends OptionsParsed with
> tool_tag=cargo-rbe (populates the invocation page Tool tag), splits the
> structured command line into executable/command/options/residual, delivers
> options as a parsed list; silences intentional deprecated-field warnings;
> doctests off for generated code.

> Adds examples/sample-ws (two cacheable units incl. a build-script crate) and
> scripts/two-checkout-test.sh (digest stability across checkout paths and
> CARGO_HOMEs, cross-checkout restore, edit-miss-then-hit; unsets
> CARGO_TARGET_DIR itself after the premature-tarball incident).

> .github/workflows/smoke.yml: new wrapper-cache-test job against a local
> bazel-remote cache-only server in both normal and EXTERNAL_TARGET=1 layouts
> with log artifacts; local-buildbuddy job also runs the acceptance test
> against the local BuildBuddy cache; build job now caches/uploads
> cargo-rbe-wrap. README documents usage, caveats and findings (remapping,
> dependency closure, CARGO_MANIFEST_DIR env! exclusion, coarse package-dir
> hashing, @env: settings in environment, no daemon yet).

> Cargo.lock reverts to the pre-draft state (drops signal-hook-registry added
> by experimental draft 7776144, which this drop supersedes). Includes verbatim
> Claude session log 007 (.opencode/plans/claude-log-007.md).

Assisted-by: claude/sonnet-5.5-medium (established pattern — confirm at gate).

Date verification after commit:
`git log -1 --format='author-date=[%ad] committer-date=[%cd]'` — both must
equal Tue Nov 17 10:23:04 2026 +0000 exactly.

### C_a: v6 design-doc overwrite — commit time 2026-11-17T10:23:14 +0000
(= v6 mtime)

Staging steps (from repo root):

1. `cp -a .opencode/plans/cargo-rbe-design.v6.md ./cargo-rbe-design.md`
   (mtime preserved); that one path only.
2. Expect an incidental mode change 100755→100644 (v6 source is non-executable)
   — same as the v5 overwrite; note it in the execution log.

Message draft — subject: `Record D2 delivery and D1.2 field results in design
doc (v6)`; body (paragraphs as committed):

> Marks the D2 chunk done with its delivered contents, adds a concrete D3
> proposal to the chunk plan (reuse the D2 Action, upload the input tree incl.
> dependency closure and OUT_DIR, run it on a worker whose image provides the
> same rustc, then compare remote outputs with a local build of the same unit),
> records the maintainer's D1.2 field results (local-cache-only 4 ok/1 skipped;
> local-buildbuddy 6 ok; remote BuildBuddy cloud 6 ok with bes: published 16
> events, 16 acknowledged; invocation page fully populated) and the three
> resulting fixes (OptionsParsed tool_tag=cargo-rbe, command-line split into
> executable/command/options/residual, parsed options list).

> Also records D2 verification results (identical Action digests across checkout
> paths and CARGO_HOMEs, cross-checkout restore with warning replay,
> edit-miss-then-hit, 16/16 events through the launcher; hit 6–8 ms vs compile
> 25–36 ms on tiny samples) and findings (registry-path remapping,
> dependency-closure index, CARGO_MANIFEST_DIR env! exclusion, coarse
> package-dir hashing, no daemon yet).

Assisted-by: claude/sonnet-5.5-medium (confirm at gate).

Date verification after commit:
`git log -1 --format='author-date=[%ad] committer-date=[%cd]'` — both must
equal Tue Nov 17 10:23:14 2026 +0000 exactly.

## Decisions

- (a) Experimental draft 7776144 stays in history; no rewrite (rebase/reset/
  amend forbidden). The formal drop supersedes its content: C_b's extraction
  diffs against the draft, and Cargo.lock reverts to the pre-draft state as
  part of that. RESOLVED by user directive.
- (b) Duplicate tarball cargo-rbe-D2-older.tar.gz (byte-identical, same sha256):
  use cargo-rbe-D2.tar.gz as canonical; neither file deleted nor modified
  (sacred). RESOLVED.
- (c) .gitignore clobber during unpack: restore via `git checkout -- .gitignore`,
  append only missing tarball rules — per 001/002 precedent, not a decision
  point.
- (d) Empty dir tools/protogen/src/proto in the tarball is untrackable by git →
  no-op; noted for completeness, nothing staged.
- (e) No empty/no-op commits: every path that differs from HEAD lands in C_b;
  byte-identical overwrites produce no git change and are not spec'd separately.
- (f) Log 007 ships with C_b (the delivery commit), per the established pattern
  (each log NNN ships inside the commit of the chunk it delivers); its on-disk
  mtime is a copy time. RESOLVED by pattern; user may object at the message gate.
- (g) Commit order follows source mtimes literally: tarball 10:23:04 precedes
  design v6 10:23:14 by 10 s — a copy artifact (per log 007 the doc update was
  written before the final tarball presentation, but both on-disk times are
  post-delivery). Unpack first, then doc overwrite; final tree state is
  identical either way.

## GitHub Actions info needed after import

- No new secrets: RBE_HEADERS (x-buildbuddy-api-key=...) is reused for
  everything in this drop.
- New optional repo variable: `RBE_RUN_WRAPPER_TEST` = 'true' makes the remote
  job also run the wrapper acceptance test against the cloud cache (smoke.yml
  gate `if: vars.RBE_RUN_WRAPPER_TEST == 'true'`). Unset/empty is safe — the
  step simply skips.
- Claude's forward-looking asks from log 007 (all EXTERNAL — user runs them
  after pushing):
  - Run the workflow, especially the new wrapper-cache-test job and
    local-buildbuddy; send back the report.json and wrapper-test.log artifacts.
  - Try the wrapper on one real project against the cloud cache; a real
    project's actions-*.jsonl log would show which crates are ineligible or
    unpublished and why — that decides what D3 tackles first.
  - For D3 (remote execution of one rustc unit): tell Claude which server to
    test toolchain-on-worker. Open question for the next drop; no action needed
    now.

## Execution protocol (same as 001/002)

Per commit:

1. Message gate via question tool (full draft shown verbatim).
2. Stage exactly the spec'd paths.
3. Review staged-only content (`git diff --cached`, `git show :<path>`): every
   message claim verified against the actual diff, zero absolute paths
   (/wolfpool, /home/), file endings intact — must PASS before committing.
4. Dated commit via
   `env GIT_AUTHOR_DATE="<iso> +0000" GIT_COMMITTER_DATE="<iso> +0000" git commit -m ...`
   (the env prefix intentionally breaks pre-approval; the approval prompt is BY
   DESIGN for history-critical commits).
5. Explicit date verification per spec above.
6. Update this plan's execution log, then stop and report.

## Execution log

### C_b D2 unpack + log 007 — DONE (2026-11-17)
- Commit 18baabf on master (parent 7776144). Author and committer dates both
  verified = Tue Nov 17 10:23:04 2026 +0000 (= tarball mtime), via
  git log -1 --format='author-date=[%ad] committer-date=[%cd]'.
- 22 files changed, 834 insertions(+), 245 deletions(-); includes
  .opencode/plans/claude-log-007.md staged in place (verbatim, hash-checked).
- The predicted .gitignore clobber happened during extraction and was restored
  via git checkout -- .gitignore before staging; all four tarball rules were
  already present from the D1/D1.2 merges, so nothing was appended.
- Explore-agent review of staged-only content: PASS (13/13 checks). One minor
  finding: the README does not itself say "no daemon yet" — that finding is
  documented in claude-log-007.md:71, which ships in this same commit; all
  other README sub-items verified at README.md:79-97. Reviewer also confirmed
  staged Cargo.lock byte-identical to cdfa1d1:Cargo.lock and the log blob
  verbatim.
### C_a v6 overwrite — DONE (2026-11-17)
- Commit dbf39b3 on master (parent 18baabf). Author and committer dates both
  verified = Tue Nov 17 10:23:14 2026 +0000 (= v6 mtime), via
  git log -1 --format='author-date=[%ad] committer-date=[%cd]'.
- 1 file changed, cargo-rbe-design.md +36/−2 (exactly the v5→v6 delta); staged blob
  byte-identical to the sacred v6 original (sha256 cd26803c…, re-verified at commit).
- Review basis: CARRIED-FROM-PASS3 — explore review of this exact staged content was
  complete in pass 3; its FAIL concerned one message sub-claim, not the content.
  The user chose option 2 (pass-3 report): the findings bullet drops only
  "CARGO_MANIFEST_DIR env! exclusion," (that finding is recorded in
  claude-log-007.md:68 and README.md:86-89, already committed via C_b — not in the
  design doc); all other message text verbatim as approved.
- This completes the D2 import.
