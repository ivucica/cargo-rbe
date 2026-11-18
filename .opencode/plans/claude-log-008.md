"If you can, also try the wrapper on one real project against your cloud cache (`RBE_RUN_WRAPPER_TEST=true` runs the sample there too). A real project’s `actions-*.jsonl` log would tell me which crates are ineligible or unpublished and why, and that decides what D3 should tackle first."

 Explain very quickly how would I run it on "one real project"? Which real project should I use it with? Can you propose some real test steps?

This time for speed I'm attaching reports only from github actions, unless you explicitly need something from non-GHA environment.

# Response

I'll check the GitHub Actions results first, then give you the real-project steps.

Now build checkout A through the wrapper (cache mode, debug profile) in the background, so a slow build can't hit the command timeout.

A finished already. Let me see the full summary, then build checkout B (different path, different `CARGO_HOME`, empty target directory).

fd gives a more representative picture: 71 invocations, 56 cached, 15 local. Now checkout B, to confirm what hits when proc macros and a native dependency are in the mix.

B hit 54 of the 56 units A published. I want to know which 2 missed and why, since proc macros are the likely suspect.

Now the acceptance test twice against the *same* server without restarting it, which is exactly the situation that failed in CI.

The test flaw is fixed: the second run reports `1 published, 1 already cached` and passes. Now fd again from scratch (fresh server, clones, caches and target directories) with the broader remapping.

Now I'll run the script exactly as you would, with ripgrep against a fresh local cache server, to confirm it works end to end.

# Continuation

Continue

# Response

Now the proper check: ripgrep again with the fixed script, plus a quick test that a stray `OUT_DIR` is now refused rather than hashed.

I've packaged D2.1 as `cargo-rbe-D2.1.tar.gz` and updated `cargo-rbe-design.md`. It replaces D2 (the D2 tarball is removed from the outputs). I ran the wrapper on two real projects myself so the steps below are verified.

**Your GitHub Actions results**
- Smoke passes on BuildBuddy cloud and on the local stack, 7 of 7.
- The wrapper acceptance test passes on the local cache-only server and local BuildBuddy.
- The `external-target` run failed with "nothing was published in A". That was a flaw in my test: both variants shared one server, so the second run found the first run's entries already cached. The test now accepts "already cached", and I re-ran both variants against one long-lived server to confirm.

**How to run it on a real project**
- **Locally:** `scripts/real-project-test.sh GIT_URL REF --remote_cache=grpcs://… --remote_header=@env:RBE_HEADERS`. It clones the project twice into different directories, builds it through the wrapper in each (the first build publishes, the second restores), and prints a report.
- **In GitHub Actions:** Actions → smoke → Run workflow, then fill in `project_repo` (and optionally `project_ref`, `project_cargo_args`). It uses the same variables and secret as your `remote` job. The `real-project` artifact holds the logs.
- **Cache size:** it uploads the project's library artifacts, about 100–150 MB for a small project in the debug profile, to the cache you point it at.

**Which project**
- **ripgrep `15.2.0` (start here):** small, pure Rust, no proc macros. It restored 33 of 33 cacheable units, with a build in the second directory taking about a third of the first's time.
- **fd `v10.5.2` (second):** it brings in a proc macro (clap derive) and jemalloc (a native dependency). It restored 56 of 56, and its results drove the findings below.
- **Larger projects:** bat, hyperfine or rust-analyzer are untested suggestions.

**Findings from the real runs** (the design doc has the numbers):
- **Proc macros poison dependents.** In fd, `clap` and `clap_complete` missed in the second build. The only difference was the locally built `libclap_derive….so`, which came out with different bytes in each checkout. I now apply path remapping to every compile of a registry crate, cached or not. The `.so` is then byte-identical across checkouts, and fd goes from 54 to 56 of 56. In a serde-derive project this cascade could have hit most of the dependency graph.
- **Build scripts dominate once libraries are cached.** In fd, the jemalloc build script (compiling C sources) took about 94 s of the roughly 108 s second build. The 56 restored units took about 7 s in total. This is estimated from file timestamps on a one-CPU box, so treat it as indicative only.
- **Bug of mine:** my first version of the script took its results directory from an env var named `OUT_DIR`, the same name Cargo uses for build-script output. The wrapper hashed my results directory as a crate's generated code, and the second build missed 24 of 33 units. I renamed it to `RESULTS_DIR` and fixed the same collision in the workflow. The wrapper now also refuses an `OUT_DIR` outside the target directory and just doesn't cache that unit.

**D3 proposal, changed by this data:**
- Start with running build scripts as REAPI actions through a shim at the path Cargo executes. That removes the dominant cost wherever native dependencies are built from source.
- Then proc-macro and link units, then crates with native link flags.
- Remote execution of a single plain rustc unit would show the mechanism, but it isn't the biggest cost anymore.

**What I need from you:**
- Run the real-project workflow on ripgrep against your cloud cache and send the `real-project` artifact, mainly `summary.txt`.
- Run it once on a larger project you actually care about, so I can see its ineligible reasons.
- Tell me which server you'll use for D3's remote build-script execution. It needs an executor image that has the same toolchain.


