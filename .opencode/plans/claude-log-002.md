Write it into a markdown doc, and use present_file on it as soon as it is written.  Discussing your design, the alternative that was discarded (--build-plan), section on "what the prior art settles", section "what was verified", architecture, documentation, decomposition into actions, well-defined inputs and cas eficiency, hermeticity, pipelining and parallelism (both options listed), outputs and materialization (including stripping -C incremental, replay diagnostics etc), failure model, and definitely rollout (your implementation plan) and open risks. All these sections are important. You can stay very verbatim with the phrasing there.

After you present it to me for approval, we will begin writing the initial test version that invokes reapi in a way that can be tested against buildbuddy on github actions, using an API key stored as a secret. Having cache-only version delivered is a first zeroth step, but maybe you can include doing a small thing remotely, just to test if the API works, if you can get invocation ID / invocation URL, etc?

Your deliverables should be incremental, and presented regularly (preferably before hitting token limit, in atomic chunks), with a tarball created and presented regularly with present_file tool. Ideally we would have something that I can test with actual remote execution api implementations quickly, and then after I provide you with the results, we can continue from there.

Aside from the design doc as listed, include my requests into the design-and-implementation doc you write.

# Response

The design doc is written and presented as `cargo-rbe-design.md`. It has all the sections you listed, and your requests are in section 15.

Two things to know when you review it:

- **Documentation:** I read "documentation" in your list as the user-facing configuration plus the docs that ship with the code. That is section 6.
- **Invocation URL:** BuildBuddy's invocation page is probably built from Build Event Stream (BES) data, not from the REAPI calls themselves. I haven't confirmed this. D1 first attaches a generated invocation ID to every REAPI call and prints the URL it would produce. If that page comes up empty, the next chunk adds a minimal BES publisher.

The first chunk after your approval (D1) is a small `cargo-rbe smoke` command plus a GitHub Actions workflow:

- It calls `GetCapabilities`, uploads a trivial command, runs it remotely, and fetches stdout.
- It then repeats the identical action to check whether the action cache serves it.
- It reads the API key from a repository secret (for example `BUILDBUDDY_API_KEY`) and skips cleanly when the secret is absent.

After D1, the next chunks are the cache-only `RUSTC_WRAPPER` (D2) and remote execution of one real dependency such as `cfg-if` (D3). Each chunk ships with a tarball.

Please check these before I start D1:

- Does the design match what you intended, including the decision to discard `--build-plan`?
- Is `BUILDBUDDY_API_KEY` the secret name you want?
- Should the smoke test run on every push, or only on manual dispatch and pull requests from the main repo?
