Please update the doc: rather than fixing BUILDBUDDY_API_KEY we should allow any header to be set the way Bazel provides it, and we want to pass it as a CLI flag (or a config file, the .toml you shown perhaps); similarly, do not lock us into app.buildbuddy.io/invocation/ because this depends on the actual service used (we may run a local copy of buildbuddy, buildbarn, or another custom implementation of CAS and of executor).

Example from .bazelrc:

build:remote --remote_executor=grpcs://remote.buildbuddy.io
build:remote --host_platform=@buildbuddy_toolchain//:platform
build:remote --platforms=@buildbuddy_toolchain//:platform
build:remote --extra_execution_platforms=@buildbuddy_toolchain//:platform
build:remote --crosstool_top=@buildbuddy_toolchain//:toolchain
build:remote --extra_toolchains=@buildbuddy_toolchain//:cc_toolchain
build:remote --javabase=@buildbuddy_toolchain//:javabase_jdk8
build:remote --host_javabase=@buildbuddy_toolchain//:javabase_jdk8
build:remote --java_toolchain=@buildbuddy_toolchain//:toolchain_jdk8
build:remote --host_java_toolchain=@buildbuddy_toolchain//:toolchain_jdk8
build:remote --define=EXECUTOR=remote
build:remote --define=libsdl12_linux_deps_bin=true

build:remote --bes_results_url=https://app.buildbuddy.io/invocation/
build:remote --bes_backend=grpcs://remote.buildbuddy.io
build:remote --remote_cache=grpcs://remote.buildbuddy.io
build:remote --remote_timeout=3600
build:remote --remote_executor=grpcs://remote.buildbuddy.io

# build:remote --build_metadata=REPO_URL=https://github.com/ivucica/glict
# build:remote --build_metadata=VISIBILITY=PUBLIC  # default for unauthenticated, could be put into buildbuddysecrets if authenticated
build:remote --workspace_status_command=$(pwd)/workspace_status.sh

# readonly:
#build:remote --noremote_upload_local_results # Uploads logs & artifacts without writing to cache
# toplevel:
#build:remote --remote_download_toplevel # Helps remove network bottleneck if caching is enabled

build:remote --jobs=50

# Use buildbuddy for cache only, by passing --config=remotecache.
# This should be fine for this open source project.
build:remotecache --bes_results_url=https://app.buildbuddy.io/invocation/
build:remotecache --bes_backend=grpcs://remote.buildbuddy.io
build:remotecache --remote_cache=grpcs://remote.buildbuddy.io
build:remotecache --remote_timeout=3600
build:remotecache --remote_executor=grpcs://remote.buildbuddy.io
# Cannot use in 6.5.0:
# build:remotecache --experimental_remote_downloader=grpcs://remote.buildbuddy.io
# Not in 6.5.0:
# build:remotecache --unix_digest_hash_attribute_name=user.git_hash
# build:remotecache --digest_function=SHA1

# build:remotecache --build_metadata=REPO_URL=https://github.com/ivucica/glict
# build:remotecache --build_metadata=VISIBILITY=PUBLIC  # default for unauthenticated, could be put into buildbuddysecrets if authenticated
build:remotecache --workspace_status_command=$(pwd)/workspace_status.sh


# readonly:
#build:remotecache --noremote_upload_local_results # Uploads logs & artifacts without writing to cache
# toplevel:
#build:remotecache --remote_download_toplevel # Helps remove network bottleneck if caching is enabled

try-import %workspace%/.bazelrc.buildbuddysecrets

Redacted version of .bazelrc.buildbuddysecrets which was simply imported into the main .bazelrc:

build:remote --remote_header=x-buildbuddy-api-key=SOME_KEY_HERE
build:remotecache --remote_header=x-buildbuddy-api-key=SOME_KEY_HERE

which presumably works with grpc too, somehow.

It might also be good to discuss specific uses of specific RPCs/APIs, and the language this must be written in. (Is it easier to write our wrapper in Rust, Go or Python, or some other language? Do we want to use buf for proto management? Just because Cargo is for Rust does not mean we should not consider other languages)

NOTE: This does NOT MEAN we want to support ALL OF THIS or in this format, but we DO WANT to be able to adjust anything to make it not specific to  buildbuddy. Buildbuddy is just one RBE implementation, and its cloud version is just another cloud version.

NOTE: This is not a directive to completely rework your design, just to make it more aware that we are unlikely to actually run this against buildbuddy.

Confirm by present_file'ing the file again.

# Response

I've updated `cargo-rbe-design.md` and presented it again. The design is otherwise unchanged; it is now server-agnostic.

- **Headers and config (section 6):**
  - Headers are generic `--remote_header=name=value` pairs, repeatable, sent as gRPC metadata on every call. There is no fixed key name.
  - A value can be `@env:NAME` or `@file:path`, so CI secrets never sit in a config file.
  - Config is the `.cargo-rbe.toml` with `[common]` and `[config.<name>]` sections, selected by `--config=<name>`. An optional gitignored `import` file plays the role of your `try-import` secrets file.
  - Precedence is CLI, then environment, then project config, then user config.
  - Bazel flag names are reused where they fit. I deliberately left out the Bazel toolchain and platform flags (`--host_platform`, `--crosstool_top`, `--javabase` and so on). Here the equivalent is plain execution properties on the Action.
- **Results URL (section 15):** The invocation ID is always generated and sent in the request metadata. A URL is printed only if you configure a `results_url` template such as `https://ui.example/invocation/{invocation_id}`. A BES (Build Event Stream) endpoint stays optional. I note that some services may have no invocation page at all.
- **CI (section 15):** The endpoint comes from a repository variable and headers from secrets, under names you choose. The remote job is skipped when they're absent. I also propose an optional second job that runs the smoke test against a local server container, so pull requests need no cloud account. Which server to use for that is your call, since I haven't verified any of those setups.
- **RPCs (new section 16):** A table of each RPC, what it's used for and which chunk first needs it. It also covers request metadata, Action shape, digest negotiation, retries, and not caching failed results.
- **Language and protos (new section 17):**
  - I recommend Rust, because the wrapper is spawned once per rustc call, users already have the toolchain, and it's the only route to eventually upstreaming into Cargo. The daemon boundary keeps Go as a later swap. Python is for throwaway experiments only.
  - For protos, I propose pinning `remote-apis` and checking the generated Rust code into the repo, so contributors and CI don't need `protoc` or `buf`. `buf` would be optional tooling for lint, breaking-change checks and regeneration.

Please confirm Rust, or tell me if you'd rather use Go. Once the doc is approved, D1 (the smoke test plus the GitHub Actions workflow) is next.

