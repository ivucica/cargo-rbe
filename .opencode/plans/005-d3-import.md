---
epic: cargo-rbe drop imports
title: D3 import (build scripts) — plan 005
status: completed
completed: 2026-11-20
created: 2026-11-20
priority: P0
---

# D3 import plan

## User directives (verbatim)
- "New drop has arrived, log 009 and design v8 along with drop D3. Import it" (2026-11-20)

## Source inventory (.opencode/plans/)
| file | mtime (+0000) | sha256 | note |
|---|---|---|---|
| cargo-rbe-D3.tar.gz | 2026-11-20 11:36:58 | c5df499e5e76…ec9be3903 | CANONICAL drop, 122 entries under cargo-rbe/ |
| cargo-rbe-design.v8.md | 2026-11-20 11:38:18 | aa37d5a2bb90…af6a18d2 | gitignored original; C_a source |
| claude-log-009.md | (copy time, wrong by design) | f988fe0fecfe…c9c36b252817 | sacred; ships in C_b. FINAL RESPONSE MISSING (token exhaustion; user note L115: v8 has all updates we need) |

No delivery zip this drop. Chronology by source mtime: tarball 11:36:58 < design v8 11:38:18 → C_b before C_a (same pattern as D2/D2.1).

## Branch base and merge analysis
- Pristine base = tip of D2.1 dated import chain = **5e3e925** (branch d2.1-import), NOT master (master carries local work: rust-toolchain skill, manifest pins, agent file, plans, .gitignore rules).
- `git diff --name-only 5e3e925..master` → .gitignore, .opencode/agents/cargo-rbe-importer.md, .opencode/plans/{003,004}-*.md, .opencode/skills/d1-local-smoke/SKILL.md, .opencode/skills/rust-toolchain/*, BASIC_USAGE.sh, Cargo.lock, Cargo.toml.
- Overlap with drop content:
  - **Cargo.lock**: tarball's is BYTE-IDENTICAL to master's (our local uuid 1.26.1 pin == Claude's — same `cargo update --precise` command) → merge converges cleanly, no conflict.
  - **Cargo.toml**: EXPECTED CONFLICT at merge. Master has bare `rust-version = "1.85"` (local 9e4cabb); tarball adds the same line WITH comment plus a workspace `resolver = "3" # MSRV-aware...` line we lack. Resolution: take Claude's version verbatim (superset of our local change, upstream-sanctioned). Post-merge invariant: Cargo.toml == tarball byte-for-byte.
  - **.gitignore**: branch side stays at base version (restored post-extraction, not staged) → merge auto-takes master's newer rules. Clean.
  - No other overlap (drop does not touch .opencode/ or BASIC_USAGE.sh).

## Commits

### C_b — Import D3 (tarball content + log 009)
- Commit time: **Fri Nov 20 11:36:58 2026 +0000** (= tarball mtime)
- Staging (worktree already extracted; .gitignore restored):
  - Modified (16): Cargo.toml, Cargo.lock, .github/workflows/smoke.yml, README.md, examples/sample-ws/Cargo.toml, examples/sample-ws/Cargo.lock, examples/sample-ws/app/Cargo.toml, examples/sample-ws/app/src/main.rs, examples/sample-ws/samplelib/Cargo.toml, scripts/real-project-test.sh, scripts/two-checkout-test.sh, src/lib.rs, src/main.rs, src/rpc.rs, src/wrap.rs, tools/protogen/Cargo.toml
  - Added (8): src/buildscript.rs, src/tree.rs, scripts/build-script-test.sh, examples/sample-ws/nativelib/{Cargo.toml,build.rs,src/native.c,src/lib.rs}
  - Log in place: .opencode/plans/claude-log-009.md (staged sha256 must equal f988fe0f…c9c36b252817)
  - NOT staged: d2.1-info/, cargo-rbe-D2.1-files.zip, .gitignore
- Message draft: see gate record below

### C_a — design v8 → working copy
- Commit time: **Fri Nov 20 11:38:18 2026 +0000** (= v8 mtime)
- Staging: `cp -a .opencode/plans/cargo-rbe-design.v8.md cargo-rbe-design.md` (only that file; staged sha256 must equal aa37d5a2…af6a18d2; diff vs committed working copy = exactly the reviewed 49-line delta)
- Message draft: see gate record below

## Decisions
(a) Branch base 5e3e925 per D2.1 directive (local work stays on master). Expected single Cargo.toml conflict at merge → resolve by taking Claude's version verbatim (superset); post-merge invariant Cargo.toml == tarball byte-for-byte; Cargo.lock converges byte-identical (verified pre-extraction).
(b) .gitignore clobber hazard → restored ours post-extraction (`git checkout -- .gitignore`), verified absent from modified list.
(c) Log 009 final response missing (token exhaustion; user note L115) → design v8 is authoritative for D3 details and forward-looking asks; log staged verbatim (sacred, never modified).
(d) Design doc header says "D3 status (delivered 2026-11-18)" but actual delivery is Nov 20 — Claude's date quirk seen in prior drops too; commit time = source mtime per protocol, doc not modified.
(e) Forward-looking asks all EXTERNAL: run the new CI build-script step on GHA (Claude did not verify it there); real worker-pool / larger-project results decide next-chunk order ("Where to go after D3"); BuildBuddy cloud for build scripts needs `container-image` exec property if default image lacks C compiler/rustc. No local data bundle requested this drop.
(f) GH Actions: no new secrets/vars anticipated (workflow reuses RBE_HEADERS + existing vars); smoke.yml pins Rust 1.85 and adds a build-script-test step — verify at staged review.

## GitHub Actions info
- No new secrets or vars expected for D3 (verify against workflow diff at C_b review).
- CONFIRMED at C_b review and post-merge: no new secrets; one NEW OPTIONAL var RBE_RUN_BUILD_SCRIPT_TEST (gate for the remote build-script step; unset/empty = safe skip). CI pins Rust 1.85; build-script acceptance test runs in cache mode always, remote mode only when that var is set to true.

## Execution protocol
Per standing procedure: question-tool message gate per dated commit → stage spec'd paths only → staged-content review PASS (zero absolute paths /wolfpool|/home/, file endings intact, log sha256-identical) → env-prefixed dated commit (approval prompt BY DESIGN) → explicit date verify `git log -1 --format='author-date=[%ad] committer-date=[%cd]'` → update this plan → STOP and report after EACH. Merge back only after user inspection; expect the one Cargo.toml conflict, resolve per decision (a).

## Execution log
- 2026-11-20: pre-flight clean (master=5417589); inventory + sha256 verified; scratch diff vs 5e3e925 done (/tmp/opencode/d3s0-a, d3s1-a); manifest convergence checked (Cargo.lock identical to master; Cargo.toml differs by comment+resolver line → expected merge conflict).
- 2026-11-20: branch d3-import cut from 5e3e925; overlap check done (only Cargo.toml conflicts at merge); tarball extracted into worktree (--strip-components=1), .gitignore restored, git status matches spec exactly.
- 2026-11-20: C_b message approved as drafted (question gate); staged exactly the 24 spec'd files; staged-content review PASS (zero real absolute paths — only synthetic /home/u unit-test fixture, documented exception; all file endings intact; log sha256 f988fe0f… identical to original; every message claim verified against diff incl. README section and smoke.yml). Committed as **7444d0c** with env-prefixed dates; DATES-VERIFIED author=committer=Fri Nov 20 11:36:58 2026 +0000 (= tarball mtime). Note for GH Actions section: workflow adds one NEW OPTIONAL var RBE_RUN_BUILD_SCRIPT_TEST (gate for the remote build-script step; unset/empty = safe skip) — no new secrets.
- 2026-11-20 (evening): C_a message gate — v1 rejected by user (too much jargon/acronyms), rewritten as clean narrative with list items/sublists; v2 approved after removing second-person "your" references (commit author is nominally the user). Staged only cargo-rbe-design.md (cp -a from gitignored v8 original); review PASS: staged sha256 aa37d5a2…af6a18d2 identical to original, diff = exactly the reviewed 49-line delta, zero absolute paths. Committed as **766a4e9** with env-prefixed dates; DATES-VERIFIED author=committer=Fri Nov 20 11:38:18 2026 +0000 (= v8 mtime).
- 2026-11-20 (evening): merge per user directive ("merge it into main and fix the merge issue by falling back to what claude did"). NOTE: master had moved since pre-flight — user's own commit **d4b494b** (12:04, .gitignore `d1-info/` → `d*-info/`, which also resolves the pending d2.1-info decision) landed in between; merge base remained 5e3e925. Merge commit **830d5f5** "Merge D3 import (7444d0c, 766a4e9) into master" — exactly one conflict as predicted (Cargo.toml), resolved with `git checkout --theirs` = Claude's version verbatim per the directive. Post-merge verification ALL PASS: Cargo.toml == tarball byte-for-byte (sha256 1f1c78d0…); Cargo.lock unchanged from pre-merge master; .gitignore keeps user's d*-info/ rule plus all local rules; drop content present in HEAD (src/buildscript.rs, src/tree.rs, scripts/build-script-test.sh mode 100755, examples/sample-ws/nativelib/, claude-log-009.md); smoke.yml RUST_VERSION "1.85" + build-script acceptance step (line 162) + project_build_scripts dispatch input; design doc == v8 original (aa37d5a2…); local work intact (agent file, rust-toolchain skill tree, BASIC_USAGE.sh, plans).
- 2026-11-20 (evening): branch d3-import KEPT at 766a4e9 (no deletion requested; D2.1 precedent). Import complete.