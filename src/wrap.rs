//! RUSTC_WRAPPER for D2: a cache-only REAPI client for rustc invocations.
//!
//! For every `rustc` invocation Cargo schedules, the wrapper builds a canonical REAPI Action
//! (location-independent command line, allowlisted environment, Merkle input root), asks the
//! ActionCache for a result, and on a hit restores the outputs and replays rustc's diagnostics.
//! On a miss it runs the real rustc locally and publishes the result. It never fails a build
//! because of the cache: anything unexpected falls back to running rustc locally.
//!
//! Scope (see the design doc, section 13): only library compilations (`lib`/`rlib`, emitting
//! `dep-info`, `metadata`, `link`) are cached. Binaries, proc macros, build scripts and anything
//! needing native linking are passed through (and logged with the reason).
//!
//! Modes (`CARGO_RBE_MODE`): `off`, `digest-only` (compute and log Action digests, no network),
//! `read-only` (lookup, never publish), `cache` (default).
#![allow(deprecated)] // deprecated REAPI fields are set on purpose for older servers

use crate::config::{self, Settings};
use crate::proto::build::bazel::remote::execution::v2 as re;
use crate::rpc::{self, digest_of, fmt_digest, Conn};
use crate::statcache::{sha256_hex, StatCache};
use anyhow::{anyhow, Result};
use prost::Message;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type R<T> = Result<T, String>;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Off,
    DigestOnly,
    ReadOnly,
    Cache,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "off" => Some(Mode::Off),
            "digest-only" => Some(Mode::DigestOnly),
            "read-only" => Some(Mode::ReadOnly),
            "cache" => Some(Mode::Cache),
            _ => None,
        }
    }
    fn from_env() -> Mode {
        std::env::var("CARGO_RBE_MODE").ok().and_then(|s| Mode::parse(&s)).unwrap_or(Mode::Cache)
    }
}

// ---------------------------------------------------------------------------------------------
// paths

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn absolutize(cwd: &Path, s: &str) -> PathBuf {
    let p = Path::new(s);
    normalize(&if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) })
}

pub fn cache_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("CARGO_RBE_CACHE_DIR") {
        return PathBuf::from(d);
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| std::env::temp_dir());
    base.join("cargo-rbe")
}

/// Replace real path prefixes by stable tokens (and back) in text artifacts (stderr, dep-info).
fn replace_root(s: &str, from: &str, to: &str) -> String {
    if from.len() <= 1 || !s.contains(from) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(from) {
        let before_ok = i == 0 || !rest[..i].chars().next_back().map(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.').unwrap_or(false);
        let after = &rest[i + from.len()..];
        let after_ok = after.chars().next().map(|c| !(c.is_alphanumeric() || c == '_' || c == '-' || c == '.')).unwrap_or(true);
        out.push_str(&rest[..i]);
        out.push_str(if before_ok && after_ok { to } else { from });
        rest = after;
    }
    out.push_str(rest);
    out
}

fn tokenize_text(s: &str, tokens: &[(String, String)]) -> String {
    tokens.iter().fold(s.to_string(), |acc, (real, tok)| replace_root(&acc, real, tok))
}

fn untokenize_text(s: &str, tokens: &[(String, String)]) -> String {
    tokens.iter().fold(s.to_string(), |acc, (real, tok)| acc.replace(tok.as_str(), real))
}

// ---------------------------------------------------------------------------------------------
// argument parsing

#[derive(Clone, Debug)]
struct Tok {
    flag: String, // "" for positional arguments
    value: Option<String>,
}

const LONG_VALUE_FLAGS: &[&str] = &[
    "--crate-name", "--crate-type", "--emit", "--out-dir", "--extern", "--cfg", "--check-cfg", "--edition", "--error-format", "--json",
    "--target", "--cap-lints", "--sysroot", "--remap-path-prefix", "--diagnostic-width", "--color", "--print", "--allow", "--warn",
    "--deny", "--forbid", "--force-warn", "--codegen", "--env-set", "--explain",
];
const SHORT_VALUE_FLAGS: &[&str] = &["-C", "-L", "-l", "-A", "-W", "-D", "-F", "-Z", "-o"];

fn tokenize_args(args: &[String]) -> Vec<Tok> {
    let mut out = vec![];
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a.starts_with("--") {
            if let Some((f, v)) = a.split_once('=') {
                out.push(Tok { flag: f.into(), value: Some(v.into()) });
            } else if LONG_VALUE_FLAGS.contains(&a.as_str()) && i + 1 < args.len() {
                out.push(Tok { flag: a.clone(), value: Some(args[i + 1].clone()) });
                i += 1;
            } else {
                out.push(Tok { flag: a.clone(), value: None });
            }
        } else if a.len() > 1 && a.starts_with('-') {
            let f = a.get(..2).unwrap_or(a);
            if SHORT_VALUE_FLAGS.contains(&f) {
                if a.len() > 2 {
                    out.push(Tok { flag: f.into(), value: Some(a[2..].into()) });
                } else if i + 1 < args.len() {
                    out.push(Tok { flag: f.into(), value: Some(args[i + 1].clone()) });
                    i += 1;
                } else {
                    out.push(Tok { flag: a.clone(), value: None });
                }
            } else {
                out.push(Tok { flag: a.clone(), value: None });
            }
        } else {
            out.push(Tok { flag: String::new(), value: Some(a.clone()) });
        }
        i += 1;
    }
    out
}

fn is_compile(args: &[String]) -> bool {
    args.iter().any(|a| a == "--crate-name") && args.iter().any(|a| a.ends_with(".rs")) && !args.iter().any(|a| a.starts_with("--print"))
}

fn quick_field(args: &[String], flag: &str) -> Option<String> {
    tokenize_args(args).into_iter().find(|t| t.flag == flag).and_then(|t| t.value)
}

fn extra_filename(args: &[String]) -> Option<String> {
    tokenize_args(args).into_iter().filter(|t| t.flag == "-C").filter_map(|t| t.value).find_map(|v| v.strip_prefix("extra-filename=").map(|s| s.to_string()))
}

// ---------------------------------------------------------------------------------------------
// plan: canonical action

struct OutSpec {
    name: String,
    real: PathBuf,
    is_depinfo: bool,
    is_artifact: bool, // rlib / rmeta: recorded in the closure index
}

struct Plan {
    action_digest: re::Digest,
    blobs: Vec<(re::Digest, Vec<u8>)>, // Command and Action protos (always published with a result)
    outputs: Vec<OutSpec>,
    tokens: Vec<(String, String)>,
    manifest_dir: PathBuf,
    out_dir: PathBuf,
    gen_dir: Option<PathBuf>,
    excludes: Vec<PathBuf>,
    closure: BTreeMap<String, re::Digest>,
    allowed_env: BTreeSet<String>,
    components: Value,
    cwd: PathBuf,
    cache_dir: PathBuf,
    /// Arguments appended to the real rustc invocation (path remapping). Their canonical form is
    /// already part of the Action.
    real_extra_args: Vec<String>,
    remap_target: Option<(PathBuf, String)>,
}

fn registry_root_of(manifest_dir: &Path) -> Option<PathBuf> {
    manifest_dir
        .ancestors()
        .find(|a| {
            a.parent().and_then(|p| p.file_name()).map(|n| n == "src").unwrap_or(false)
                && a.parent().and_then(|p| p.parent()).and_then(|p| p.file_name()).map(|n| n == "registry").unwrap_or(false)
        })
        .map(|p| p.to_path_buf())
}

/// Path remapping applied to every compile of a registry crate (cached or not), so that artifacts
/// do not embed CARGO_HOME or the working directory. Returns (real flags, canonical flags).
/// The registry flag comes last: when CARGO_HOME lives inside the working directory the more
/// specific remapping must win.
fn registry_remap_flags(manifest_dir: &Path, cwd: &Path) -> (Vec<String>, Vec<String>) {
    if std::env::var("CARGO_RBE_REMAP").unwrap_or_else(|_| "registry".into()) != "registry" {
        return (vec![], vec![]);
    }
    match registry_root_of(manifest_dir) {
        Some(reg) => (
            vec![format!("--remap-path-prefix={}=/workspace", cwd.display()), format!("--remap-path-prefix={}=/cargo-registry", reg.display())],
            vec!["--remap-path-prefix=$CWD=/workspace".into(), "--remap-path-prefix=$REGISTRY=/cargo-registry".into()],
        ),
        None => (vec![], vec![]),
    }
}

fn is_cargo_cachedir(p: &Path) -> bool {
    p.join("CACHEDIR.TAG").exists()
}

fn dir_digest(sc: &mut StatCache, dir: &Path, excludes: &[PathBuf], count: &mut usize) -> R<re::Digest> {
    let mut entries: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .map_err(|e| format!("reading {}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| (e.file_name().to_string_lossy().to_string(), e.path()))
        .collect();
    entries.sort();
    let mut d = re::Directory::default();
    for (name, path) in entries {
        *count += 1;
        if *count > 100_000 {
            return Err("input tree too large (over 100000 entries)".into());
        }
        let md = std::fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let ft = md.file_type();
        if ft.is_symlink() {
            let target = std::fs::read_link(&path).map_err(|e| e.to_string())?;
            d.symlinks.push(re::SymlinkNode { name, target: target.to_string_lossy().to_string(), ..Default::default() });
        } else if ft.is_dir() {
            if matches!(name.as_str(), ".git" | ".hg" | ".svn") || excludes.iter().any(|e| *e == path) || is_cargo_cachedir(&path) {
                continue;
            }
            let sub = dir_digest(sc, &path, excludes, count)?;
            d.directories.push(re::DirectoryNode { name, digest: Some(sub) });
        } else if ft.is_file() {
            let dg = sc.file_digest(&path).map_err(|e| format!("hashing {}: {e}", path.display()))?;
            d.files.push(re::FileNode { name, digest: Some(dg), is_executable: md.mode() & 0o111 != 0, ..Default::default() });
        }
    }
    Ok(digest_of(&d.encode_to_vec()))
}

fn closure_path(cache: &Path, hash: &str) -> PathBuf {
    cache.join("closure").join(format!("{hash}.json"))
}

fn read_closure(cache: &Path, hash: &str) -> Option<Vec<(String, re::Digest)>> {
    let v: Value = serde_json::from_slice(&std::fs::read(closure_path(cache, hash)).ok()?).ok()?;
    v.as_array()?
        .iter()
        .map(|e| Some((e["name"].as_str()?.to_string(), re::Digest { hash: e["hash"].as_str()?.to_string(), size_bytes: e["size"].as_i64()? })))
        .collect()
}

fn write_closure(cache: &Path, hash: &str, closure: &BTreeMap<String, re::Digest>) {
    let v: Vec<Value> = closure.iter().map(|(n, d)| json!({"name": n, "hash": d.hash, "size": d.size_bytes})).collect();
    let p = closure_path(cache, hash);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = p.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, serde_json::to_vec(&v).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

/// Run `rustc -vV` (not through the wrapper). The launcher exports the result once per build.
fn rustc_identity(rustc: &Path) -> R<String> {
    if let Ok(v) = std::env::var("CARGO_RBE_RUSTC_ID") {
        if !v.is_empty() {
            return Ok(v);
        }
    }
    let out = Command::new(rustc).arg("-vV").output().map_err(|e| format!("rustc -vV: {e}"))?;
    if !out.status.success() {
        return Err("rustc -vV failed".into());
    }
    Ok(sha256_hex(&out.stdout))
}

pub fn rustc_identity_for_launcher() -> Option<String> {
    let rustc = std::env::var_os("RUSTC").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("rustc"));
    let out = Command::new(rustc).arg("-vV").output().ok()?;
    out.status.success().then(|| sha256_hex(&out.stdout))
}

fn make_plan(rustc: &Path, args: &[String], cwd: &Path, cache: &Path, settings: &Settings) -> R<Plan> {
    let toks = tokenize_args(args);

    // ---- scan
    let (mut crate_name, mut crate_types, mut emit) = (None, vec![], vec![]);
    let (mut out_dir, mut extra, mut incremental, mut source) = (None, None, false, None);
    let mut has_target = false;
    for t in &toks {
        match (t.flag.as_str(), t.value.as_deref()) {
            ("--crate-name", Some(v)) => crate_name = Some(v.to_string()),
            ("--crate-type", Some(v)) => crate_types.push(v.to_string()),
            ("--emit", Some(v)) => emit.extend(v.split(',').map(|s| s.to_string())),
            ("--out-dir", Some(v)) => out_dir = Some(absolutize(cwd, v)),
            ("--target", _) => has_target = true,
            ("-C", Some(v)) => {
                if let Some(x) = v.strip_prefix("extra-filename=") {
                    extra = Some(x.to_string());
                }
                if v.starts_with("incremental=") {
                    incremental = true;
                }
            }
            ("", Some(v)) if v.ends_with(".rs") => {
                if source.replace(v.to_string()).is_some() {
                    return Err("more than one source file".into());
                }
            }
            ("-l", _) => return Err("native link flag -l (needs native libraries as inputs; later chunk)".into()),
            ("--test", _) => return Err("test harness (links an executable)".into()),
            (f, _) if f.starts_with("@") => return Err("response file argument".into()),
            _ => {}
        }
    }
    let crate_name = crate_name.ok_or("no --crate-name")?;
    let out_dir = out_dir.ok_or("no --out-dir")?;
    let extra = extra.ok_or("no -C extra-filename (cannot predict output names)")?;
    let source = source.ok_or("no source file")?;
    if crate_types.is_empty() || !crate_types.iter().all(|t| t == "lib" || t == "rlib") {
        return Err(format!("crate type {:?}: only lib/rlib are cached in this version (linking needs native inputs)", crate_types));
    }
    if emit.is_empty() || !emit.iter().all(|e| matches!(e.as_str(), "dep-info" | "metadata" | "link")) {
        return Err(format!("emit {:?}: only dep-info, metadata, link are supported", emit));
    }
    if out_dir.file_name().map(|n| n != "deps").unwrap_or(true) {
        return Err("--out-dir is not a Cargo 'deps' directory (unknown layout)".into());
    }
    let manifest_dir = normalize(Path::new(&std::env::var("CARGO_MANIFEST_DIR").map_err(|_| "CARGO_MANIFEST_DIR not set (not run by Cargo?)".to_string())?));

    // ---- roots
    let mut target_root = out_dir.clone();
    for _ in 0..(if has_target { 3 } else { 2 }) {
        target_root.pop();
    }
    let registry_root = registry_root_of(&manifest_dir);
    let cargo_home = registry_root.as_ref().and_then(|r| r.parent()?.parent()?.parent().map(|p| p.to_path_buf()));
    let mut tokens: Vec<(String, String)> = vec![];
    let mut add = |p: Option<&PathBuf>, tok: &str| {
        if let Some(p) = p {
            let s = p.to_string_lossy().to_string();
            if s.len() > 1 && !tokens.iter().any(|(r, _)| *r == s) {
                tokens.push((s, tok.to_string()));
            }
        }
    };
    add(Some(&out_dir), "@DEPS@");
    add(Some(&target_root), "@TARGET@");
    add(Some(&manifest_dir), "@MANIFEST@");
    add(registry_root.as_ref(), "@REG@");
    add(cargo_home.as_ref(), "@CARGOHOME@");
    add(Some(&cwd.to_path_buf()), "@CWD@");
    tokens.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

    let source_abs = absolutize(cwd, &source);
    let rel_source = source_abs.strip_prefix(&manifest_dir).map_err(|_| "source file is outside the package directory".to_string())?.to_string_lossy().to_string();

    // ---- canonical arguments, extern inputs
    let mut sc_deps = StatCache::open(cache, &format!("deps:{}", out_dir.display()));
    let mut canon: Vec<String> = vec![];
    let mut direct: Vec<(String, re::Digest)> = vec![];
    for t in &toks {
        match (t.flag.as_str(), t.value.as_deref()) {
            ("--out-dir", _) => canon.push("--out-dir=out".into()),
            ("--extern", Some(v)) => match v.split_once('=') {
                Some((l, p)) => {
                    let abs = absolutize(cwd, p);
                    let fname = abs.file_name().ok_or("bad --extern path")?.to_string_lossy().to_string();
                    let d = sc_deps.file_digest(&abs).map_err(|e| format!("hashing extern {}: {e}", abs.display()))?;
                    direct.push((fname.clone(), d));
                    canon.push(format!("--extern={l}=deps/{fname}"));
                }
                None => canon.push(format!("--extern={v}")),
            },
            ("-L", Some(v)) => {
                let (kind, path) = v.split_once('=').unwrap_or(("all", v));
                if kind == "dependency" && absolutize(cwd, path) == out_dir {
                    canon.push("-L".into());
                    canon.push("dependency=deps".into());
                } else {
                    return Err(format!("-L {v}: only -L dependency=<out-dir> is supported"));
                }
            }
            ("-C", Some(v)) if v.starts_with("incremental=") => {}
            ("", Some(v)) if v.ends_with(".rs") => canon.push(format!("pkg/{rel_source}")),
            ("", Some(v)) => canon.push(v.to_string()),
            (f, Some(v)) if f.starts_with("--") => canon.push(format!("{f}={v}")),
            (f, Some(v)) => {
                canon.push(f.to_string());
                canon.push(v.to_string());
            }
            (f, None) => canon.push(f.to_string()),
        }
    }
    sc_deps.save();
    // Location-independent outputs: registry sources are remapped to a fixed virtual prefix, so the
    // same crate compiled under different CARGO_HOME paths yields identical artifacts (otherwise the
    // embedded source path changes the .rmeta, and every dependent's key with it).
    let (real_extra_args, canon_remap) = registry_remap_flags(&manifest_dir, cwd);
    let remap_target = registry_root.clone().filter(|_| !real_extra_args.is_empty()).map(|r| (r, "/cargo-registry".to_string()));
    canon.extend(canon_remap);
    for a in &canon {
        let value = a.split_once('=').map(|(_, v)| v).unwrap_or(a.as_str());
        if value.starts_with('/') || tokens.iter().any(|(r, _)| a.contains(r.as_str())) {
            return Err(format!("unmapped absolute path in argument: {a}"));
        }
    }

    // ---- dependency closure (what rustc may load from the search path)
    let mut closure: BTreeMap<String, re::Digest> = BTreeMap::new();
    let mut add_closure = |n: &str, d: &re::Digest| -> R<()> {
        match closure.get(n) {
            Some(old) if old.hash != d.hash => Err(format!("two different files named {n} in the dependency closure")),
            _ => {
                closure.insert(n.to_string(), d.clone());
                Ok(())
            }
        }
    };
    for (fname, d) in &direct {
        add_closure(fname, d)?;
        let ext = Path::new(fname).extension().and_then(|e| e.to_str()).unwrap_or("");
        if matches!(ext, "so" | "dylib" | "dll") {
            continue; // proc macros are loaded as a unit; their own dependencies are not needed
        }
        match read_closure(cache, &d.hash) {
            Some(list) => {
                for (n, dd) in list {
                    add_closure(&n, &dd)?;
                }
            }
            None => return Err(format!("dependency closure unknown for {fname} (it was not built through the wrapper in this cache directory)")),
        }
    }

    // ---- input tree: pkg/, deps/, gen/out/
    let mut excludes: Vec<PathBuf> = vec![];
    if let Ok(rel) = target_root.strip_prefix(&manifest_dir) {
        if let Some(first) = rel.components().next() {
            excludes.push(manifest_dir.join(first.as_os_str()));
        }
    }
    let mut count = 0usize;
    let mut sc_pkg = StatCache::open(cache, &format!("pkg:{}", manifest_dir.display()));
    let pkg_digest = dir_digest(&mut sc_pkg, &manifest_dir, &excludes, &mut count)?;
    let gen_dir = std::env::var("OUT_DIR").ok().map(|p| absolutize(cwd, &p));
    if let Some(g) = &gen_dir {
        // Cargo creates OUT_DIR under the target directory. Anything else is a stray variable (or an
        // unknown layout) and must not be hashed as if it were generated code.
        if !g.starts_with(&target_root) {
            return Err(format!("OUT_DIR {} is outside the target directory", g.display()));
        }
    }
    let gen_digest = match &gen_dir {
        Some(g) if g.is_dir() => {
            let mut sc_gen = StatCache::open(cache, &format!("gen:{}", g.display()));
            let d = dir_digest(&mut sc_gen, g, &[], &mut count)?;
            sc_gen.save();
            Some(d)
        }
        Some(_) => Some(digest_of(&re::Directory::default().encode_to_vec())),
        None => None,
    };
    sc_pkg.save();
    let deps_dir = re::Directory {
        files: closure.iter().map(|(n, d)| re::FileNode { name: n.clone(), digest: Some(d.clone()), is_executable: false, ..Default::default() }).collect(),
        ..Default::default()
    };
    let deps_digest = digest_of(&deps_dir.encode_to_vec());
    let mut root = re::Directory::default();
    root.directories.push(re::DirectoryNode { name: "deps".into(), digest: Some(deps_digest.clone()) });
    if let Some(g) = &gen_digest {
        let gen_parent = re::Directory { directories: vec![re::DirectoryNode { name: "out".into(), digest: Some(g.clone()) }], ..Default::default() };
        root.directories.push(re::DirectoryNode { name: "gen".into(), digest: Some(digest_of(&gen_parent.encode_to_vec())) });
    }
    root.directories.push(re::DirectoryNode { name: "pkg".into(), digest: Some(pkg_digest.clone()) });
    let root_digest = digest_of(&root.encode_to_vec());

    // ---- environment (allowlist)
    let rustc_id = rustc_identity(rustc)?;
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let mut allowed: BTreeSet<String> = BTreeSet::new();
    for (k, v) in std::env::vars() {
        let keep = k.starts_with("CARGO_PKG_") || matches!(k.as_str(), "CARGO_CRATE_NAME" | "CARGO_BIN_NAME" | "CARGO_PRIMARY_PACKAGE" | "RUSTC_BOOTSTRAP");
        if keep {
            allowed.insert(k.clone());
            env.insert(k, v);
        }
    }
    allowed.insert("OUT_DIR".into());
    env.insert("CARGO_MANIFEST_DIR".into(), "pkg".into());
    if gen_dir.is_some() {
        env.insert("OUT_DIR".into(), "gen/out".into());
    }
    env.insert("CARGO_RBE_INCREMENTAL".into(), if incremental { "1" } else { "0" }.into());
    env.insert("CARGO_RBE_TOOLCHAIN".into(), rustc_id.clone());

    // ---- outputs
    let stem = crate_name.replace('-', "_");
    let mut outputs = vec![];
    for e in &emit {
        let (name, depinfo, artifact) = match e.as_str() {
            "link" => (format!("lib{stem}{extra}.rlib"), false, true),
            "metadata" => (format!("lib{stem}{extra}.rmeta"), false, true),
            _ => (format!("{stem}{extra}.d"), true, false),
        };
        outputs.push(OutSpec { real: out_dir.join(&name), name, is_depinfo: depinfo, is_artifact: artifact });
    }
    outputs.sort_by(|a, b| a.name.cmp(&b.name));
    outputs.dedup_by(|a, b| a.name == b.name);

    // ---- command and action
    let platform = re::Platform { properties: settings.exec_properties.iter().map(|(k, v)| re::platform::Property { name: k.clone(), value: v.clone() }).collect() };
    let mut arguments = vec!["rustc".to_string()];
    arguments.extend(canon);
    let command = re::Command {
        arguments,
        environment_variables: env.into_iter().map(|(name, value)| re::command::EnvironmentVariable { name, value }).collect(),
        output_paths: outputs.iter().map(|o| format!("out/{}", o.name)).collect(),
        platform: Some(platform.clone()),
        working_directory: String::new(),
        ..Default::default()
    };
    let cmd_bytes = command.encode_to_vec();
    let action = re::Action {
        command_digest: Some(digest_of(&cmd_bytes)),
        input_root_digest: Some(root_digest.clone()),
        timeout: None,
        do_not_cache: false,
        salt: vec![],
        platform: Some(platform),
    };
    let action_bytes = action.encode_to_vec();
    let action_digest = digest_of(&action_bytes);
    let components = json!({
        "toolchain": rustc_id,
        "command": fmt_digest(action.command_digest.as_ref().unwrap()),
        "input_root": fmt_digest(&root_digest),
        "pkg": fmt_digest(&pkg_digest),
        "gen": gen_digest.as_ref().map(fmt_digest),
        "deps": fmt_digest(&deps_digest),
        "closure": closure.iter().map(|(n, d)| format!("{n}={}", &d.hash[..12])).collect::<Vec<_>>(),
        "args": command.arguments,
    });
    let blobs = vec![(action.command_digest.clone().unwrap(), cmd_bytes), (action_digest.clone(), action_bytes)];
    Ok(Plan { action_digest, blobs, outputs, tokens, manifest_dir, out_dir, gen_dir, excludes, closure, allowed_env: allowed, components, cwd: cwd.to_path_buf(), cache_dir: cache.to_path_buf(), real_extra_args, remap_target })
}

// ---------------------------------------------------------------------------------------------
// dep-info verification

fn split_make_tokens(line: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.peek() {
                Some(' ') | Some('\\') | Some('#') | Some(':') => cur.push(chars.next().unwrap()),
                _ => cur.push('\\'),
            },
            ' ' | '\t' => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Every input rustc reports must be inside what we hashed, and every environment variable it
/// reads must be part of the key. Otherwise the result must not be published.
fn check_depinfo(text: &str, plan: &Plan) -> R<()> {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# env-dep:") {
            let name = rest.split('=').next().unwrap_or(rest);
            if name == "CARGO_MANIFEST_DIR" {
                return Err("the crate reads CARGO_MANIFEST_DIR (path-dependent output; needs identity paths, later chunk)".into());
            }
            if !plan.allowed_env.contains(name) {
                return Err(format!("the crate reads env var {name}, which is not part of the cache key"));
            }
            continue;
        }
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        for tok in split_make_tokens(line) {
            let tok = tok.trim_end_matches(':');
            if tok.is_empty() {
                continue;
            }
            let mut p = absolutize(&plan.cwd, tok);
            if let Some((real, virt)) = &plan.remap_target {
                if let Ok(rest) = p.strip_prefix(virt) {
                    p = real.join(rest);
                }
            }
            let in_pkg = p.starts_with(&plan.manifest_dir) && !plan.excludes.iter().any(|e| p.starts_with(e));
            let in_gen = plan.gen_dir.as_ref().map(|g| p.starts_with(g)).unwrap_or(false);
            if !(p.starts_with(&plan.out_dir) || in_pkg || in_gen) {
                return Err(format!("input outside the hashed set: {}", p.display()));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// local execution

fn passthrough(rustc: &Path, args: &[OsString]) -> i32 {
    let err = Command::new(rustc).args(args).exec();
    eprintln!("cargo-rbe-wrap: failed to run {}: {err}", rustc.display());
    127
}

/// Run the real rustc, forwarding stderr immediately (Cargo reads it as a stream) while keeping a copy.
fn run_local(rustc: &Path, args: &[String]) -> (i32, Vec<u8>) {
    let mut child = match Command::new(rustc).args(args).stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cargo-rbe-wrap: failed to run {}: {e}", rustc.display());
            return (127, vec![]);
        }
    };
    let mut captured = vec![];
    if let Some(mut err) = child.stderr.take() {
        let mut buf = [0u8; 16 * 1024];
        let stderr = std::io::stderr();
        loop {
            match err.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut lock = stderr.lock();
                    let _ = lock.write_all(&buf[..n]);
                    let _ = lock.flush();
                    captured.extend_from_slice(&buf[..n]);
                }
            }
        }
    }
    let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(1);
    (code, captured)
}

// ---------------------------------------------------------------------------------------------
// remote

async fn connect(settings: &Settings, invocation_id: &str) -> Result<(Conn, String)> {
    let url = settings.remote_cache.clone().or_else(|| settings.remote_executor.clone()).ok_or_else(|| anyhow!("no remote_cache configured"))?;
    let conn = tokio::time::timeout(Duration::from_secs(8), rpc::connect_with(&url, settings, invocation_id, "rustc")).await.map_err(|_| anyhow!("connect timed out"))??;
    Ok((conn, settings.remote_instance_name.clone().unwrap_or_default()))
}

async fn lookup(conn: &Conn, instance: &str, plan: &Plan) -> Result<Option<re::ActionResult>> {
    let r = conn
        .ac()
        .get_action_result(re::GetActionResultRequest {
            instance_name: instance.into(),
            action_digest: Some(plan.action_digest.clone()),
            inline_stdout: true,
            inline_stderr: true,
            inline_output_files: vec![],
            digest_function: 0,
        })
        .await;
    match r {
        Ok(r) => Ok(Some(r.into_inner())),
        Err(e) if e.code() == tonic::Code::NotFound => Ok(None),
        Err(e) => Err(anyhow!("GetActionResult: {:?}: {}", e.code(), e.message())),
    }
}

struct Restored {
    files: usize,
    bytes: u64,
    stderr: Vec<u8>,
}

async fn restore(conn: &Conn, instance: &str, plan: &Plan, ar: &re::ActionResult) -> Result<Restored> {
    let mut want: BTreeMap<String, &re::OutputFile> = BTreeMap::new();
    for o in &ar.output_files {
        want.insert(o.path.clone(), o);
    }
    for spec in &plan.outputs {
        if !want.contains_key(&format!("out/{}", spec.name)) {
            return Err(anyhow!("cached result lacks out/{}", spec.name));
        }
    }
    let (mut bytes, mut files) = (0u64, 0usize);
    for spec in &plan.outputs {
        let o = want[&format!("out/{}", spec.name)];
        let d = o.digest.clone().ok_or_else(|| anyhow!("output without digest"))?;
        let mut data = rpc::fetch(conn, instance, &d).await?;
        bytes += data.len() as u64;
        if spec.is_depinfo {
            data = untokenize_text(&String::from_utf8_lossy(&data), &plan.tokens).into_bytes();
        }
        let tmp = spec.real.with_extension(format!("rbe-tmp-{}", std::process::id()));
        std::fs::write(&tmp, &data)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))?;
        std::fs::rename(&tmp, &spec.real)?;
        files += 1;
    }
    let raw = if !ar.stderr_raw.is_empty() {
        ar.stderr_raw.clone()
    } else if let Some(d) = ar.stderr_digest.as_ref().filter(|d| d.size_bytes > 0) {
        rpc::fetch(conn, instance, d).await?
    } else {
        vec![]
    };
    let stderr = untokenize_text(&String::from_utf8_lossy(&raw), &plan.tokens).into_bytes();
    Ok(Restored { files, bytes, stderr })
}

async fn publish(conn: &Conn, instance: &str, plan: &Plan, stderr: &[u8]) -> Result<(String, u64)> {
    let mut blobs = plan.blobs.clone();
    let mut result = re::ActionResult { exit_code: 0, ..Default::default() };
    for spec in &plan.outputs {
        let mut data = std::fs::read(&spec.real).map_err(|e| anyhow!("reading {}: {e}", spec.real.display()))?;
        if spec.is_depinfo {
            let text = String::from_utf8_lossy(&data).to_string();
            if let Err(why) = check_depinfo(&text, plan) {
                return Ok((format!("skipped: {why}"), 0));
            }
            data = tokenize_text(&text, &plan.tokens).into_bytes();
        }
        let d = digest_of(&data);
        result.output_files.push(re::OutputFile { path: format!("out/{}", spec.name), digest: Some(d.clone()), is_executable: false, ..Default::default() });
        blobs.push((d, data));
    }
    let err_bytes = tokenize_text(&String::from_utf8_lossy(stderr), &plan.tokens).into_bytes();
    if !err_bytes.is_empty() {
        let d = digest_of(&err_bytes);
        result.stderr_digest = Some(d.clone());
        blobs.push((d, err_bytes));
    }
    result.execution_metadata = Some(re::ExecutedActionMetadata { worker: "cargo-rbe-wrap (local)".into(), ..Default::default() });
    let (_, up) = rpc::upload_blobs(conn, instance, &blobs).await?;
    conn.ac()
        .update_action_result(re::UpdateActionResultRequest {
            instance_name: instance.into(),
            action_digest: Some(plan.action_digest.clone()),
            action_result: Some(result),
            results_cache_policy: None,
            digest_function: 0,
        })
        .await
        .map_err(|e| anyhow!("UpdateActionResult: {:?}: {}", e.code(), e.message()))?;
    Ok(("ok".into(), up))
}

// ---------------------------------------------------------------------------------------------
// log, settings, entry point

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

fn write_log(cache: &Path, rec: &Value) {
    let path = std::env::var_os("CARGO_RBE_LOG").map(PathBuf::from).unwrap_or_else(|| cache.join("actions.jsonl"));
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let mut line = serde_json::to_vec(rec).unwrap_or_default();
        line.push(b'\n');
        let _ = f.write_all(&line);
    }
}

fn load_settings() -> Settings {
    if let Ok(j) = std::env::var("CARGO_RBE_WRAP_SETTINGS") {
        if let Ok(s) = serde_json::from_str::<Settings>(&j) {
            return s;
        }
    }
    let file = std::env::var_os("CARGO_RBE_CONFIG_FILE").map(PathBuf::from);
    let names: Vec<String> = std::env::var("CARGO_RBE_CONFIG").ok().map(|s| vec![s]).unwrap_or_default();
    config::load(&names, file.as_deref(), Settings::default()).map(|l| l.settings).unwrap_or_default()
}

fn net_down(cache: &Path) -> bool {
    std::fs::read_to_string(cache.join("remote-down")).ok().and_then(|s| s.trim().parse::<u128>().ok()).map(|t| now_ms().saturating_sub(t) < 30_000).unwrap_or(false)
}

fn mark_down(cache: &Path) {
    let _ = std::fs::create_dir_all(cache);
    let _ = std::fs::write(cache.join("remote-down"), now_ms().to_string());
}

fn record_closure_for_outputs(plan: &Plan) {
    for spec in plan.outputs.iter().filter(|o| o.is_artifact) {
        if let Ok(bytes) = std::fs::read(&spec.real) {
            write_closure(&plan.cache_dir, &sha256_hex(&bytes), &plan.closure);
        }
    }
}

pub fn run() -> i32 {
    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
    if argv.is_empty() {
        eprintln!("cargo-rbe-wrap: used as RUSTC_WRAPPER; run `cargo-rbe cargo build` (or set RUSTC_WRAPPER=cargo-rbe-wrap)");
        return 2;
    }
    let rustc = PathBuf::from(&argv[0]);
    let rest_os = &argv[1..];
    let rest: Vec<String> = match rest_os.iter().map(|a| a.to_str().map(|s| s.to_string())).collect::<Option<Vec<_>>>() {
        Some(v) => v,
        None => return passthrough(&rustc, rest_os),
    };
    let mode = Mode::from_env();
    if mode == Mode::Off || !is_compile(&rest) {
        return passthrough(&rustc, rest_os);
    }
    let t0 = Instant::now();
    let cwd = std::env::current_dir().unwrap_or_default();
    let cache = cache_dir();
    let settings = load_settings();
    let crate_name = quick_field(&rest, "--crate-name").unwrap_or_default();
    let extra = extra_filename(&rest).unwrap_or_default();
    let crate_type = quick_field(&rest, "--crate-type").unwrap_or_default();
    let mut rec = json!({"ts_ms": now_ms() as u64, "pid": std::process::id(), "mode": format!("{mode:?}"), "crate": crate_name, "unit": extra, "crate_type": crate_type});
    let finish = |mut rec: Value, outcome: &str, t0: Instant| {
        rec["outcome"] = json!(outcome);
        rec["millis"] = json!(t0.elapsed().as_millis() as u64);
        write_log(&cache, &rec);
    };

    let plan = match make_plan(&rustc, &rest, &cwd, &cache, &settings) {
        Ok(p) => p,
        Err(reason) => {
            rec["eligible"] = json!(false);
            rec["reason"] = json!(reason);
            finish(rec, "local_only", t0);
            // Not cached, but still remapped (registry crates only) so that what it produces, e.g. a
            // proc macro, is identical across checkouts and does not poison its dependents' keys.
            let mut args_os: Vec<OsString> = rest_os.to_vec();
            if let Ok(m) = std::env::var("CARGO_MANIFEST_DIR") {
                for a in registry_remap_flags(&normalize(Path::new(&m)), &cwd).0 {
                    args_os.push(a.into());
                }
            }
            return passthrough(&rustc, &args_os);
        }
    };
    rec["eligible"] = json!(true);
    rec["action"] = json!(fmt_digest(&plan.action_digest));
    rec["action_full"] = json!(plan.action_digest.hash);
    rec["key"] = plan.components.clone();
    let mut notes: Vec<String> = vec![];

    let remote_url = settings.remote_cache.clone().or_else(|| settings.remote_executor.clone());
    let want_net = matches!(mode, Mode::ReadOnly | Mode::Cache) && remote_url.is_some() && !net_down(&cache);
    if matches!(mode, Mode::ReadOnly | Mode::Cache) && remote_url.is_none() {
        notes.push("no remote_cache configured".into());
    }
    let rt = if want_net { tokio::runtime::Builder::new_current_thread().enable_all().build().ok() } else { None };
    let invocation = std::env::var("CARGO_RBE_INVOCATION_ID").unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());
    let mut remote: Option<(Conn, String)> = None;
    if let Some(rt) = &rt {
        match rt.block_on(connect(&settings, &invocation)) {
            Ok(c) => remote = Some(c),
            Err(e) => {
                mark_down(&cache);
                notes.push(format!("remote_error (connect): {e:#}"));
            }
        }
    }
    let mut lookup_ok = false;
    if let (Some(rt), Some((conn, inst))) = (&rt, &remote) {
        match rt.block_on(async { tokio::time::timeout(Duration::from_secs(30), lookup(conn, inst, &plan)).await }) {
            Ok(Ok(Some(ar))) => match rt.block_on(restore(conn, inst, &plan, &ar)) {
                Ok(r) => {
                    record_closure_for_outputs(&plan);
                    let mut err = std::io::stderr();
                    let _ = err.write_all(&r.stderr);
                    let _ = err.flush();
                    rec["files_restored"] = json!(r.files);
                    rec["bytes_down"] = json!(r.bytes);
                    rec["notes"] = json!(notes);
                    finish(rec, "hit", t0);
                    return ar.exit_code;
                }
                Err(e) => notes.push(format!("restore_failed: {e:#}")),
            },
            Ok(Ok(None)) => lookup_ok = true,
            Ok(Err(e)) => notes.push(format!("remote_error (lookup): {e:#}")),
            Err(_) => notes.push("remote_error (lookup): timed out".into()),
        }
    }

    // miss (or no remote): run rustc locally
    let mut real_args = rest.clone();
    real_args.extend(plan.real_extra_args.iter().cloned());
    let (code, stderr) = run_local(&rustc, &real_args);
    if code != 0 {
        rec["notes"] = json!(notes);
        finish(rec, "local_failed", t0);
        return code;
    }
    record_closure_for_outputs(&plan);
    let mut outcome = if want_net { "miss" } else { "local" };
    if mode == Mode::Cache && lookup_ok {
        if let (Some(rt), Some((conn, inst))) = (&rt, &remote) {
            match rt.block_on(async { tokio::time::timeout(Duration::from_secs(120), publish(conn, inst, &plan, &stderr)).await }) {
                Ok(Ok((status, up))) => {
                    rec["bytes_up"] = json!(up);
                    if status == "ok" {
                        outcome = "miss_published";
                    } else {
                        outcome = "miss_not_published";
                    }
                    rec["publish"] = json!(status);
                }
                Ok(Err(e)) => {
                    outcome = "miss_publish_failed";
                    rec["publish"] = json!(format!("error: {e:#}"));
                }
                Err(_) => {
                    outcome = "miss_publish_failed";
                    rec["publish"] = json!("error: timed out");
                }
            }
        }
    }
    rec["notes"] = json!(notes);
    finish(rec, outcome, t0);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_replacement_respects_path_boundaries() {
        let toks = vec![("/home/a/ws".to_string(), "@CWD@".to_string())];
        assert_eq!(tokenize_text("/home/a/ws/src/lib.rs", &toks), "@CWD@/src/lib.rs");
        assert_eq!(tokenize_text("/home/a/ws2/src/lib.rs", &toks), "/home/a/ws2/src/lib.rs");
        assert_eq!(tokenize_text("x/home/a/ws/y", &toks), "x/home/a/ws/y");
        assert_eq!(untokenize_text("@CWD@/src/lib.rs", &toks), "/home/a/ws/src/lib.rs");
    }

    #[test]
    fn tokenizes_cargo_style_arguments() {
        let a: Vec<String> = ["--crate-name", "foo", "-C", "metadata=abc", "--extern=x=/t/d/libx.rmeta", "-L", "dependency=/t/d", "src/lib.rs", "-Cextra-filename=-abc"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let t = tokenize_args(&a);
        assert_eq!(t[0].flag, "--crate-name");
        assert_eq!(t[1].flag, "-C");
        assert_eq!(t[1].value.as_deref(), Some("metadata=abc"));
        assert_eq!(t[2].flag, "--extern");
        assert_eq!(t[3].flag, "-L");
        assert_eq!(t[4].flag, "");
        assert_eq!(t[5].flag, "-C");
        assert_eq!(extra_filename(&a).as_deref(), Some("-abc"));
    }

    #[test]
    fn make_style_dep_lines_split_on_unescaped_spaces() {
        assert_eq!(split_make_tokens(r"/o/lib.rmeta: src/lib.rs my\ file.rs"), vec!["/o/lib.rmeta:", "src/lib.rs", "my file.rs"]);
    }

    #[test]
    fn mode_parsing() {
        assert_eq!(Mode::parse("digest-only"), Some(Mode::DigestOnly));
        assert_eq!(Mode::parse("nope"), None);
    }

    #[test]
    fn lexical_normalization() {
        assert_eq!(normalize(Path::new("/a/b/../c/./d")), PathBuf::from("/a/c/d"));
    }
}
