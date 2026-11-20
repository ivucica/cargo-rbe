//! D3: caching and remote execution of build scripts.
//!
//! Cargo runs a build script as `target/.../build-script-build` and offers no hook around that. So
//! after rustc has compiled the script (see `wrap::run`), the wrapper moves the real executable
//! aside (`*.rbe-real`) and puts a *shim* at its path: a hard link to `cargo-rbe-wrap` plus a
//! sidecar file `rbe-shim.json`. When Cargo runs the shim it behaves like any other build script
//! from Cargo's point of view (same stdout directives, same `OUT_DIR` contents), but underneath:
//!
//! 1. builds a REAPI Action: the real executable, the package directory, an allowlisted
//!    environment (Cargo's own variables plus a few well-known toolchain variables) and a command
//!    that runs it with `OUT_DIR` as an output directory;
//! 2. asks the ActionCache; a hit restores `OUT_DIR` and replays the script's output;
//! 3. on a miss, mode `cache` runs the script locally and publishes the result, mode `remote`
//!    uploads the inputs and executes the Action on a remote worker;
//! 4. on any problem runs the real executable locally: the build must never fail because of this.
//!
//! Absolute paths: a worker's directory layout is unknown, so the command prints its own root as a
//! first line (`cargo-rbe-map:{...}`), and paths in the script's output under that root are mapped
//! back to this machine's `OUT_DIR` and package directory.

#![allow(deprecated)] // deprecated REAPI fields are set on purpose for older servers

use crate::config::{parse_duration, Settings};
use crate::proto::build::bazel::remote::execution::v2 as re;
use crate::rpc::{self, digest_of, fmt_digest, Conn, Source};
use crate::statcache::StatCache;
use crate::tree;
use crate::wrap::{absolutize, cache_dir, extra_filename, load_settings, normalize, now_ms, quick_field, write_log};
use anyhow::{anyhow, Result};
use prost::Message;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum BsMode {
    Off,
    Cache,
    Remote,
}

impl BsMode {
    pub fn parse(s: &str) -> Option<BsMode> {
        match s {
            "off" => Some(BsMode::Off),
            "cache" => Some(BsMode::Cache),
            "remote" => Some(BsMode::Remote),
            _ => None,
        }
    }
    pub fn from_env() -> BsMode {
        std::env::var("CARGO_RBE_BUILD_SCRIPTS").ok().and_then(|s| BsMode::parse(&s)).unwrap_or(BsMode::Off)
    }
}

// ---------------------------------------------------------------------------------------------
// shim installation (called by the rustc wrapper) and detection (called at startup)

pub fn is_build_script_compile(args: &[String]) -> bool {
    quick_field(args, "--crate-name").map(|n| n.starts_with("build_script_")).unwrap_or(false) && quick_field(args, "--crate-type").as_deref() == Some("bin")
}

/// Replace the freshly compiled build-script executable by the shim. Returns the executable path.
pub fn install_shim(rest: &[String], cwd: &Path) -> Result<PathBuf, String> {
    let crate_name = quick_field(rest, "--crate-name").ok_or("no --crate-name")?;
    let extra = extra_filename(rest).unwrap_or_default();
    let out_dir = absolutize(cwd, &quick_field(rest, "--out-dir").ok_or("no --out-dir")?);
    let exe = out_dir.join(format!("{crate_name}{extra}"));
    if !exe.is_file() {
        return Err(format!("build script executable not found at {}", exe.display()));
    }
    let real_name = format!("{crate_name}{extra}.rbe-real");
    let real = out_dir.join(&real_name);
    let me = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    std::fs::rename(&exe, &real).map_err(|e| format!("moving the real executable aside: {e}"))?;
    // Cargo treats a unit as stale when an output is older than a source file, and a hard link carries
    // the wrapper binary's (old) modification time. So the shim's mtime must be "now": touch the hard
    // link (which also touches the wrapper binary, harmless), or if that is not permitted, copy.
    let touched_link = std::fs::hard_link(&me, &exe).is_ok()
        && std::fs::OpenOptions::new().write(true).open(&exe).and_then(|f| f.set_modified(std::time::SystemTime::now())).is_ok();
    let linked = if touched_link {
        Ok(())
    } else {
        let _ = std::fs::remove_file(&exe);
        std::fs::copy(&me, &exe).map(|_| ())
    };
    if let Err(e) = linked {
        let _ = std::fs::rename(&real, &exe);
        return Err(format!("installing the shim: {e}"));
    }
    std::fs::write(out_dir.join("rbe-shim.json"), json!({"real": real_name}).to_string()).map_err(|e| format!("writing sidecar: {e}"))?;
    Ok(exe)
}

/// If this process was started by Cargo as a build script that carries a shim sidecar, return it.
pub fn shim_invocation() -> Option<PathBuf> {
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if argv.len() != 1 {
        return None;
    }
    let p = Path::new(&argv[0]);
    let name = p.file_name()?.to_str()?;
    if !(name == "build-script-build" || name.starts_with("build_script_")) {
        return None;
    }
    let dir = if p.is_absolute() { p.parent()?.to_path_buf() } else { std::env::current_exe().ok()?.parent()?.to_path_buf() };
    let side = dir.join("rbe-shim.json");
    side.exists().then_some(side)
}

// ---------------------------------------------------------------------------------------------
// environment

const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

fn is_passthrough(name: &str, extra: &BTreeSet<String>) -> bool {
    const EXACT: &[&str] = &[
        "CC", "CXX", "AR", "RANLIB", "CFLAGS", "CXXFLAGS", "CPPFLAGS", "LDFLAGS", "ASFLAGS", "CROSS_COMPILE", "PKG_CONFIG", "PKG_CONFIG_PATH",
        "PKG_CONFIG_LIBDIR", "PKG_CONFIG_SYSROOT_DIR", "PKG_CONFIG_ALLOW_CROSS", "SOURCE_DATE_EPOCH", "LIBCLANG_PATH", "SDKROOT",
        "MACOSX_DEPLOYMENT_TARGET", "CRATE_CC_NO_DEFAULTS",
    ];
    const PREFIX: &[&str] = &["CC_", "CXX_", "CFLAGS_", "CXXFLAGS_", "AR_", "RANLIB_", "LDFLAGS_", "CPPFLAGS_", "TARGET_", "HOST_", "BINDGEN_", "OPENSSL_"];
    EXACT.contains(&name) || PREFIX.iter().any(|p| name.starts_with(p)) || extra.contains(name)
}

/// The environment the Action runs with. Everything not listed here is dropped, so the key depends
/// only on what is sent. A value that points into this machine's build directories cannot be
/// reproduced elsewhere and makes the script ineligible.
pub fn bs_env(vars: &[(String, String)], manifest_dir: &Path, out_dir: &Path, rustc_id: &str) -> Result<BTreeMap<String, String>, String> {
    let extra: BTreeSet<String> = std::env::var("CARGO_RBE_ENV_PASSTHROUGH").unwrap_or_default().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let profile_dir = out_dir.ancestors().find(|a| a.file_name().map(|n| n == "build").unwrap_or(false)).and_then(|b| b.parent());
    let mut markers: Vec<String> = vec![manifest_dir.to_string_lossy().to_string()];
    if let Some(t) = profile_dir.and_then(|p| p.parent()) {
        markers.push(t.to_string_lossy().to_string());
    }
    let mut env = BTreeMap::new();
    for (k, v) in vars {
        let k = k.as_str();
        let include = k.starts_with("CARGO_PKG_")
            || k.starts_with("CARGO_FEATURE_")
            || k.starts_with("CARGO_CFG_")
            || k.starts_with("DEP_")
            || matches!(k, "CARGO_MANIFEST_LINKS" | "CARGO_ENCODED_RUSTFLAGS" | "CARGO_PRIMARY_PACKAGE" | "CARGO_CRATE_NAME" | "TARGET" | "HOST" | "PROFILE" | "OPT_LEVEL" | "DEBUG" | "RUSTC_LINKER")
            || is_passthrough(k, &extra);
        if !include {
            continue;
        }
        if markers.iter().any(|m| m.len() > 1 && v.contains(m.as_str())) {
            return Err(format!("environment variable {k} points into a local build directory"));
        }
        env.insert(k.to_string(), v.clone());
    }
    if vars.iter().any(|(k, _)| k == "RUSTC") {
        env.insert("RUSTC".into(), "rustc".into());
    }
    if vars.iter().any(|(k, _)| k == "RUSTDOC") {
        env.insert("RUSTDOC".into(), "rustdoc".into());
    }
    let path = match std::env::var("CARGO_RBE_BUILD_SCRIPT_PATH") {
        Ok(p) if !p.is_empty() => format!("{p}:{DEFAULT_PATH}"),
        _ => DEFAULT_PATH.to_string(),
    };
    env.insert("PATH".into(), path);
    env.insert("NUM_JOBS".into(), "4".into());
    env.insert("CARGO_RBE_TOOLCHAIN".into(), rustc_id.to_string());
    Ok(env)
}

/// A script that declares `rerun-if-env-changed=NAME` depends on NAME. If NAME is set here but was
/// not part of the Action, the result may differ from what a local run would produce.
pub fn env_mismatch(stdout: &str, sent: &BTreeSet<String>, is_set: &dyn Fn(&str) -> bool) -> Option<String> {
    const HANDLED: &[&str] = &["OUT_DIR", "CARGO_MANIFEST_DIR", "NUM_JOBS", "CARGO_MAKEFLAGS", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "RUSTC", "RUSTDOC", "PATH"];
    for line in stdout.lines() {
        let name = line.strip_prefix("cargo:rerun-if-env-changed=").or_else(|| line.strip_prefix("cargo::rerun-if-env-changed="));
        if let Some(n) = name.map(|n| n.trim()) {
            if !sent.contains(n) && !HANDLED.contains(&n) && is_set(n) {
                return Some(n.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// output path mapping

pub fn split_marker(stdout: &[u8]) -> (Option<(String, String)>, Vec<u8>) {
    const TAG: &[u8] = b"cargo-rbe-map:";
    if !stdout.starts_with(TAG) {
        return (None, stdout.to_vec());
    }
    let nl = stdout.iter().position(|b| *b == b'\n').unwrap_or(stdout.len());
    let parsed = serde_json::from_slice::<Value>(&stdout[TAG.len()..nl]).ok().and_then(|v| Some((v["out"].as_str()?.to_string(), v["pkg"].as_str()?.to_string())));
    (parsed, stdout[(nl + 1).min(stdout.len())..].to_vec())
}

fn map_paths(text: &[u8], remote: &Option<(String, String)>, local_out: &Path, local_pkg: &Path) -> Vec<u8> {
    match remote {
        Some((ro, rp)) => String::from_utf8_lossy(text).replace(ro.as_str(), &local_out.to_string_lossy()).replace(rp.as_str(), &local_pkg.to_string_lossy()).into_bytes(),
        None => text.to_vec(),
    }
}

// ---------------------------------------------------------------------------------------------
// plan

struct Plan {
    action: re::Digest,
    command_blob: (re::Digest, Vec<u8>),
    action_blob: (re::Digest, Vec<u8>),
    inputs: Vec<(re::Digest, Source)>,
    sent_env: BTreeSet<String>,
    manifest_dir: PathBuf,
    out_dir: PathBuf,
    cache: PathBuf,
    key: Value,
}

// The working directory stays the input root, because REAPI output paths are relative to it; the
// script changes into the package directory itself, as Cargo would run a build script there.
const SCRIPT: &str = r#"R=$(pwd -P); printf 'cargo-rbe-map:{"out":"%s/out","pkg":"%s/pkg"}\n' "$R" "$R"; export OUT_DIR="$R/out" CARGO_MANIFEST_DIR="$R/pkg"; cd "$R/pkg" && exec "$R/bin/build-script-build""#;

/// Identity of the C toolchain a locally run script will see (`$CC --version`). Local-run results are
/// published under this identity so another machine with a different compiler does not reuse them;
/// remotely executed results use the constant "remote" (the worker's toolchain is selected by the
/// platform properties, which are part of the Action).
fn cc_identity() -> String {
    let cc = std::env::var("CC").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "cc".into());
    match Command::new(&cc).arg("--version").output() {
        Ok(o) if o.status.success() => crate::statcache::sha256_hex(&o.stdout)[..16].to_string(),
        _ => "unknown".into(),
    }
}

fn make_plan(real: &Path, settings: &Settings, cache: &Path, mode: BsMode) -> Result<Plan, String> {
    let manifest_dir = normalize(Path::new(&std::env::var("CARGO_MANIFEST_DIR").map_err(|_| "CARGO_MANIFEST_DIR not set".to_string())?));
    let out_dir = normalize(Path::new(&std::env::var("OUT_DIR").map_err(|_| "OUT_DIR not set".to_string())?));
    let rustc_id = std::env::var("CARGO_RBE_RUSTC_ID").ok().filter(|s| !s.is_empty()).or_else(crate::wrap::rustc_identity_for_launcher).ok_or("cannot determine the rustc identity")?;
    let vars: Vec<(String, String)> = std::env::vars().collect();
    let mut env = bs_env(&vars, &manifest_dir, &out_dir, &rustc_id)?;
    env.insert("CARGO_RBE_CC_ID".into(), if mode == BsMode::Cache { cc_identity() } else { "remote".into() });

    let mut sc = StatCache::open(cache, &format!("pkg:{}", manifest_dir.display()));
    let mut count = 0usize;
    let pkg = tree::collect(&mut sc, &manifest_dir, &[], &mut count)?;
    let exe_digest = sc.file_digest(real).map_err(|e| format!("hashing the build script: {e}"))?;
    sc.save();

    let bin_dir = re::Directory { files: vec![re::FileNode { name: "build-script-build".into(), digest: Some(exe_digest.clone()), is_executable: true, ..Default::default() }], ..Default::default() };
    let out_empty = re::Directory::default();
    let (bin_bytes, out_bytes) = (bin_dir.encode_to_vec(), out_empty.encode_to_vec());
    let (bin_d, out_d) = (digest_of(&bin_bytes), digest_of(&out_bytes));
    let root = re::Directory {
        directories: vec![
            re::DirectoryNode { name: "bin".into(), digest: Some(bin_d.clone()) },
            re::DirectoryNode { name: "out".into(), digest: Some(out_d.clone()) },
            re::DirectoryNode { name: "pkg".into(), digest: Some(pkg.root_digest.clone()) },
        ],
        ..Default::default()
    };
    let root_bytes = root.encode_to_vec();
    let root_d = digest_of(&root_bytes);

    let platform = re::Platform { properties: settings.exec_properties.iter().map(|(k, v)| re::platform::Property { name: k.clone(), value: v.clone() }).collect() };
    let command = re::Command {
        arguments: vec!["/bin/sh".into(), "-c".into(), SCRIPT.into()],
        environment_variables: env.iter().map(|(name, value)| re::command::EnvironmentVariable { name: name.clone(), value: value.clone() }).collect(),
        output_paths: vec!["out".into()],
        platform: Some(platform.clone()),
        working_directory: String::new(),
        ..Default::default()
    };
    let cmd_bytes = command.encode_to_vec();
    let cmd_d = digest_of(&cmd_bytes);
    let action = re::Action {
        command_digest: Some(cmd_d.clone()),
        input_root_digest: Some(root_d.clone()),
        timeout: Some(prost_types::Duration { seconds: 3600, nanos: 0 }),
        do_not_cache: false,
        salt: vec![],
        platform: Some(platform),
    };
    let action_bytes = action.encode_to_vec();
    let action_d = digest_of(&action_bytes);

    let mut inputs: Vec<(re::Digest, Source)> = vec![(bin_d, Source::Bytes(bin_bytes)), (out_d, Source::Bytes(out_bytes)), (root_d.clone(), Source::Bytes(root_bytes))];
    inputs.push((exe_digest.clone(), Source::File(real.to_path_buf())));
    inputs.push((pkg.root_digest.clone(), Source::Bytes(pkg.root.encode_to_vec())));
    for d in &pkg.dirs {
        let b = d.encode_to_vec();
        inputs.push((digest_of(&b), Source::Bytes(b)));
    }
    for (d, p) in &pkg.files {
        inputs.push((d.clone(), Source::File(p.clone())));
    }
    inputs.push((cmd_d.clone(), Source::Bytes(cmd_bytes.clone())));
    inputs.push((action_d.clone(), Source::Bytes(action_bytes.clone())));

    let key = json!({
        "toolchain": rustc_id,
        "executable": fmt_digest(&exe_digest),
        "package": fmt_digest(&pkg.root_digest),
        "package_files": pkg.files.len(),
        "input_root": fmt_digest(&root_d),
        "command": fmt_digest(&cmd_d),
        "env": env.keys().collect::<Vec<_>>(),
    });
    Ok(Plan {
        action: action_d.clone(),
        command_blob: (cmd_d, cmd_bytes),
        action_blob: (action_d, action_bytes),
        inputs,
        sent_env: env.keys().cloned().collect(),
        manifest_dir,
        out_dir,
        cache: cache.to_path_buf(),
        key,
    })
}

// ---------------------------------------------------------------------------------------------
// remote steps

async fn lookup(conn: &Conn, inst: &str, action: &re::Digest) -> Result<Option<re::ActionResult>> {
    match conn
        .ac()
        .get_action_result(re::GetActionResultRequest { instance_name: inst.into(), action_digest: Some(action.clone()), inline_stdout: true, inline_stderr: true, inline_output_files: vec![], digest_function: 0 })
        .await
    {
        Ok(r) => Ok(Some(r.into_inner())),
        Err(e) if e.code() == tonic::Code::NotFound => Ok(None),
        Err(e) => Err(anyhow!("GetActionResult: {:?}: {}", e.code(), e.message())),
    }
}

async fn blob_or_raw(conn: &Conn, inst: &str, raw: &[u8], d: &Option<re::Digest>) -> Result<Vec<u8>> {
    if !raw.is_empty() {
        return Ok(raw.to_vec());
    }
    match d {
        Some(d) if d.size_bytes > 0 => rpc::fetch(conn, inst, d).await,
        _ => Ok(vec![]),
    }
}

/// Restore `OUT_DIR` and replay the script's output from an ActionResult (cache hit or remote run).
async fn apply_result(conn: &Conn, inst: &str, ar: &re::ActionResult, plan: &Plan) -> Result<(usize, u64)> {
    if ar.exit_code != 0 {
        return Err(anyhow!("result has exit code {}", ar.exit_code));
    }
    let stdout_raw = blob_or_raw(conn, inst, &ar.stdout_raw, &ar.stdout_digest).await?;
    let stderr_raw = blob_or_raw(conn, inst, &ar.stderr_raw, &ar.stderr_digest).await?;
    let (remote, body) = split_marker(&stdout_raw);
    let stdout = map_paths(&body, &remote, &plan.out_dir, &plan.manifest_dir);
    let stderr = map_paths(&stderr_raw, &remote, &plan.out_dir, &plan.manifest_dir);
    if let Some(n) = env_mismatch(&String::from_utf8_lossy(&stdout), &plan.sent_env, &|n| std::env::var_os(n).is_some()) {
        return Err(anyhow!("env_mismatch: the script depends on {n}, which is set here but was not part of the action"));
    }
    std::fs::create_dir_all(&plan.out_dir)?;
    let restored = match ar.output_directories.iter().find(|o| o.path == "out") {
        Some(od) => {
            let td = od.tree_digest.clone().ok_or_else(|| anyhow!("output directory without tree digest"))?;
            tree::write_tree(conn, inst, &td, &plan.out_dir).await?
        }
        None => {
            return Err(anyhow!(
                "result has no output directory 'out' (output_directories: {:?}, output_files: {:?}, output_symlinks: {})",
                ar.output_directories.iter().map(|o| o.path.clone()).collect::<Vec<_>>(),
                ar.output_files.iter().map(|o| o.path.clone()).collect::<Vec<_>>(),
                ar.output_symlinks.len() + ar.output_file_symlinks.len() + ar.output_directory_symlinks.len()
            ))
        }
    };
    let _ = std::io::stdout().write_all(&stdout);
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().write_all(&stderr);
    Ok(restored)
}

fn run_capture(real: &Path) -> (i32, Vec<u8>, Vec<u8>) {
    match Command::new(real).output() {
        Ok(o) => (o.status.code().unwrap_or(1), o.stdout, o.stderr),
        Err(e) => (127, vec![], format!("cargo-rbe: failed to run {}: {e}\n", real.display()).into_bytes()),
    }
}

async fn publish_local(conn: &Conn, inst: &str, plan: &Plan, stdout: &[u8], stderr: &[u8]) -> Result<String> {
    let mut sc = StatCache::open(&plan.cache, &format!("gen:{}", plan.out_dir.display()));
    let mut count = 0usize;
    let c = tree::collect(&mut sc, &plan.out_dir, &[], &mut count).map_err(|e| anyhow!(e))?;
    sc.save();
    let total: i64 = c.files.iter().map(|(d, _)| d.size_bytes).sum();
    if total > 512 * 1024 * 1024 {
        return Ok(format!("skipped: OUT_DIR is {} MB (over the 512 MB limit)", total / 1_000_000));
    }
    let (td, tree_bytes) = tree::tree_blob(&c);
    let mut stored = format!("cargo-rbe-map:{}\n", json!({"out": plan.out_dir.to_string_lossy(), "pkg": plan.manifest_dir.to_string_lossy()})).into_bytes();
    stored.extend_from_slice(stdout);
    let mut items: Vec<(re::Digest, Source)> = c.files.iter().map(|(d, p)| (d.clone(), Source::File(p.clone()))).collect();
    items.push((td.clone(), Source::Bytes(tree_bytes)));
    items.push((plan.command_blob.0.clone(), Source::Bytes(plan.command_blob.1.clone())));
    items.push((plan.action_blob.0.clone(), Source::Bytes(plan.action_blob.1.clone())));
    let mut result = re::ActionResult { exit_code: 0, ..Default::default() };
    result.output_directories.push(re::OutputDirectory { path: "out".into(), tree_digest: Some(td), ..Default::default() });
    let sd = digest_of(&stored);
    items.push((sd.clone(), Source::Bytes(stored)));
    result.stdout_digest = Some(sd);
    if !stderr.is_empty() {
        let ed = digest_of(stderr);
        items.push((ed.clone(), Source::Bytes(stderr.to_vec())));
        result.stderr_digest = Some(ed);
    }
    result.execution_metadata = Some(re::ExecutedActionMetadata { worker: "cargo-rbe-wrap (local)".into(), ..Default::default() });
    let (_, up) = rpc::upload_sources(conn, inst, &items).await?;
    conn.ac()
        .update_action_result(re::UpdateActionResultRequest { instance_name: inst.into(), action_digest: Some(plan.action.clone()), action_result: Some(result), results_cache_policy: None, digest_function: 0 })
        .await
        .map_err(|e| anyhow!("UpdateActionResult: {:?}: {}", e.code(), e.message()))?;
    Ok(format!("ok ({up} bytes)"))
}

enum Flow {
    Done(i32, &'static str),
    Fallback(String),
}

async fn flow(mode: BsMode, plan: &Plan, real: &Path, settings: &Settings, invocation: &str, rec: &mut Value) -> Flow {
    let cache_url = match settings.remote_cache.clone().or_else(|| settings.remote_executor.clone()) {
        Some(u) => u,
        None => return Flow::Fallback("no remote_cache or remote_executor configured".into()),
    };
    let inst = settings.remote_instance_name.clone().unwrap_or_default();
    let connect = |url: String| async move {
        match tokio::time::timeout(Duration::from_secs(8), rpc::connect_with(&url, settings, invocation, "build-script")).await {
            Ok(Ok(c)) => Ok(c),
            Ok(Err(e)) => Err(format!("remote_error (connect {url}): {e:#}")),
            Err(_) => Err(format!("remote_error (connect {url}): timed out")),
        }
    };
    let cache = match connect(cache_url.clone()).await {
        Ok(c) => c,
        Err(e) => return Flow::Fallback(e),
    };
    match lookup(&cache, &inst, &plan.action).await {
        Ok(Some(ar)) => {
            return match apply_result(&cache, &inst, &ar, plan).await {
                Ok((files, bytes)) => {
                    rec["restored"] = json!({"files": files, "bytes": bytes});
                    Flow::Done(0, "bs_hit")
                }
                Err(e) => Flow::Fallback(format!("cached result not usable: {e:#}")),
            }
        }
        Ok(None) => {}
        Err(e) => return Flow::Fallback(format!("{e:#}")),
    }
    match mode {
        BsMode::Cache => {
            let (code, out, err) = run_capture(real);
            let _ = std::io::stdout().write_all(&out);
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().write_all(&err);
            if code != 0 {
                return Flow::Done(code, "bs_local_failed");
            }
            if let Some(n) = env_mismatch(&String::from_utf8_lossy(&out), &plan.sent_env, &|n| std::env::var_os(n).is_some()) {
                rec["publish"] = json!(format!("skipped: the script depends on {n}, which is set here but not part of the action"));
                return Flow::Done(0, "bs_local");
            }
            match publish_local(&cache, &inst, plan, &out, &err).await {
                Ok(s) => {
                    rec["publish"] = json!(s.clone());
                    Flow::Done(0, if s.starts_with("ok") { "bs_published" } else { "bs_local" })
                }
                Err(e) => {
                    rec["publish"] = json!(format!("error: {e:#}"));
                    Flow::Done(0, "bs_local")
                }
            }
        }
        BsMode::Remote => {
            let exec_url = settings.remote_executor.clone().unwrap_or_else(|| cache_url.clone());
            let exec = if exec_url == cache_url {
                cache.clone()
            } else {
                match connect(exec_url).await {
                    Ok(c) => c,
                    Err(e) => return Flow::Fallback(e),
                }
            };
            let t = Instant::now();
            let (n, bytes) = match rpc::upload_sources(&exec, &inst, &plan.inputs).await {
                Ok(x) => x,
                Err(e) => return Flow::Fallback(format!("uploading inputs: {e:#}")),
            };
            rec["inputs_uploaded"] = json!({"blobs": n, "bytes": bytes});
            let timeout = settings.remote_timeout.as_deref().and_then(|s| parse_duration(s).ok()).unwrap_or(Duration::from_secs(3600));
            let out = match tokio::time::timeout(timeout, rpc::execute(&exec, &inst, &plan.action)).await {
                Ok(Ok(o)) => o,
                Ok(Err(e)) => return Flow::Fallback(format!("remote execution: {e:#}")),
                Err(_) => return Flow::Fallback("remote execution timed out".into()),
            };
            rec["stages"] = json!(out.stages);
            rec["exec_millis"] = json!(t.elapsed().as_millis() as u64);
            if let Some(st) = out.response.status.as_ref().filter(|s| s.code != 0) {
                return Flow::Fallback(format!("remote execution status {}: {} {}", st.code, st.message, out.response.message));
            }
            let res = match out.response.result.clone() {
                Some(r) => r,
                None => return Flow::Fallback("remote execution returned no result".into()),
            };
            if let Some(m) = res.execution_metadata.as_ref().filter(|m| !m.worker.is_empty()) {
                rec["worker"] = json!(m.worker);
            }
            if res.exit_code != 0 {
                let tail = blob_or_raw(&exec, &inst, &res.stderr_raw, &res.stderr_digest).await.unwrap_or_default();
                let tail = String::from_utf8_lossy(&tail);
                let tail: String = tail.chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
                return Flow::Fallback(format!("remote exit code {}: {}", res.exit_code, tail.trim()));
            }
            match apply_result(&exec, &inst, &res, plan).await {
                Ok((files, bytes)) => {
                    rec["restored"] = json!({"files": files, "bytes": bytes});
                    Flow::Done(0, if out.response.cached_result { "bs_hit" } else { "bs_remote" })
                }
                Err(e) => Flow::Fallback(format!("remote result not usable: {e:#}")),
            }
        }
        BsMode::Off => Flow::Fallback("off".into()),
    }
}

fn exec_real(real: &Path) -> i32 {
    let err = Command::new(real).exec();
    eprintln!("cargo-rbe: failed to run {}: {err}", real.display());
    127
}

/// Entry point when Cargo runs the shim.
pub fn run_shim(sidecar: &Path) -> i32 {
    let t0 = Instant::now();
    let dir = sidecar.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let real = match std::fs::read(sidecar).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).and_then(|v| v["real"].as_str().map(|s| dir.join(s))) {
        Some(r) if r.is_file() => r,
        _ => {
            eprintln!("cargo-rbe: shim sidecar {} is unusable; the build script cannot run", sidecar.display());
            return 127;
        }
    };
    let mode = BsMode::from_env();
    if mode == BsMode::Off {
        return exec_real(&real);
    }
    let cache = cache_dir();
    let settings = load_settings();
    let pkg = std::env::var("CARGO_PKG_NAME").unwrap_or_default();
    let mut rec = json!({"ts_ms": now_ms() as u64, "kind": "build_script", "pid": std::process::id(), "crate": pkg, "mode": format!("{mode:?}")});
    let finish = |mut rec: Value, outcome: &str, reason: Option<String>| {
        rec["outcome"] = json!(outcome);
        if let Some(r) = reason {
            rec["reason"] = json!(r);
        }
        rec["millis"] = json!(t0.elapsed().as_millis() as u64);
        write_log(&cache, &rec);
    };
    let skip: BTreeSet<String> = std::env::var("CARGO_RBE_BUILD_SCRIPT_SKIP").unwrap_or_default().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if skip.contains(&pkg) {
        finish(rec, "bs_local_only", Some("skipped by --build-script-skip".into()));
        return exec_real(&real);
    }
    let plan = match make_plan(&real, &settings, &cache, mode) {
        Ok(p) => p,
        Err(reason) => {
            finish(rec, "bs_local_only", Some(reason));
            return exec_real(&real);
        }
    };
    rec["action"] = json!(fmt_digest(&plan.action));
    rec["action_full"] = json!(plan.action.hash);
    rec["key"] = plan.key.clone();
    let invocation = std::env::var("CARGO_RBE_INVOCATION_ID").unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            finish(rec, "bs_fallback", Some(format!("runtime: {e}")));
            return exec_real(&real);
        }
    };
    match rt.block_on(flow(mode, &plan, &real, &settings, &invocation, &mut rec)) {
        Flow::Done(code, outcome) => {
            finish(rec, outcome, None);
            code
        }
        Flow::Fallback(reason) => {
            finish(rec, "bs_fallback", Some(reason));
            exec_real(&real)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[(&str, &str)]) -> Vec<(String, String)> {
        a.iter().map(|(k, x)| (k.to_string(), x.to_string())).collect()
    }

    #[test]
    fn env_keeps_cargo_and_toolchain_variables_and_drops_the_rest() {
        let vars = v(&[("CARGO_PKG_NAME", "x"), ("CARGO_FEATURE_FOO", "1"), ("TARGET", "t"), ("HOME", "/home/u"), ("CC", "gcc"), ("CARGO_MAKEFLAGS", "--jobserver"), ("SECRET_TOKEN", "s"), ("CARGO_RBE_WRAP_SETTINGS", "{}"), ("RUSTC", "/opt/rustc")]);
        let env = bs_env(&vars, Path::new("/ws/x"), Path::new("/ws/target/debug/build/x-1/out"), "id").unwrap();
        assert!(env.contains_key("CARGO_PKG_NAME") && env.contains_key("CARGO_FEATURE_FOO") && env.contains_key("TARGET") && env.contains_key("CC"));
        for dropped in ["HOME", "CARGO_MAKEFLAGS", "SECRET_TOKEN", "CARGO_RBE_WRAP_SETTINGS"] {
            assert!(!env.contains_key(dropped), "{dropped} must not be sent");
        }
        assert_eq!(env["RUSTC"], "rustc");
        assert_eq!(env["CARGO_RBE_TOOLCHAIN"], "id");
    }

    #[test]
    fn env_pointing_into_the_build_directory_is_refused() {
        let vars = v(&[("DEP_Z_INCLUDE", "/ws/target/debug/build/libz-sys-9/out/include")]);
        assert!(bs_env(&vars, Path::new("/ws/x"), Path::new("/ws/target/debug/build/x-1/out"), "id").is_err());
        let ok = v(&[("DEP_Z_ROOT", "/usr")]);
        assert!(bs_env(&ok, Path::new("/ws/x"), Path::new("/ws/target/debug/build/x-1/out"), "id").is_ok());
    }

    #[test]
    fn rerun_if_env_changed_for_an_unsent_set_variable_is_a_mismatch() {
        let sent: BTreeSet<String> = ["CC".to_string()].into();
        let out = "cargo:rerun-if-env-changed=CC\ncargo::rerun-if-env-changed=FOO\ncargo:rerun-if-env-changed=OUT_DIR\n";
        assert_eq!(env_mismatch(out, &sent, &|n| n == "FOO"), Some("FOO".to_string()));
        assert_eq!(env_mismatch(out, &sent, &|_| false), None);
    }

    #[test]
    fn marker_is_split_and_paths_are_mapped() {
        let out = b"cargo-rbe-map:{\"out\":\"/r/out\",\"pkg\":\"/r/pkg\"}\ncargo:rustc-link-search=native=/r/out/lib\ncargo:rerun-if-changed=/r/pkg/build.rs\n";
        let (m, body) = split_marker(out);
        assert_eq!(m, Some(("/r/out".to_string(), "/r/pkg".to_string())));
        let mapped = String::from_utf8(map_paths(&body, &m, Path::new("/t/out"), Path::new("/t/pkg"))).unwrap();
        assert_eq!(mapped, "cargo:rustc-link-search=native=/t/out/lib\ncargo:rerun-if-changed=/t/pkg/build.rs\n");
        let (none, same) = split_marker(b"plain\n");
        assert!(none.is_none() && same == b"plain\n");
    }

    #[test]
    fn build_script_compiles_are_recognised() {
        let a: Vec<String> = ["--crate-name", "build_script_build", "--crate-type", "bin", "build.rs"].iter().map(|s| s.to_string()).collect();
        assert!(is_build_script_compile(&a));
        let b: Vec<String> = ["--crate-name", "foo", "--crate-type", "bin", "main.rs"].iter().map(|s| s.to_string()).collect();
        assert!(!is_build_script_compile(&b));
    }
}
