//! Configuration: flags, environment, TOML files (with named configs and imports).
//!
//! Precedence (lowest to highest): user config, project config, environment, CLI.
//! Within a file: `[common]`, then each selected `[config.<name>]`, then the file's imports.
//! Nothing here is specific to any one REAPI service.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Default, Clone)]
pub struct Settings {
    pub remote_executor: Option<String>,
    pub remote_cache: Option<String>,
    pub remote_instance_name: Option<String>,
    /// Raw header specs "name=value" (value may be @env:NAME or @file:path). Appended across layers.
    pub remote_header: Vec<String>,
    pub remote_timeout: Option<String>,
    /// Platform properties; later layers override the same key.
    pub exec_properties: BTreeMap<String, String>,
    pub results_url: Option<String>,
    pub bes_backend: Option<String>,
    pub tls_certificate: Option<PathBuf>,
    pub tls_client_certificate: Option<PathBuf>,
    pub tls_client_key: Option<PathBuf>,
}

impl Settings {
    pub fn overlay(&mut self, o: Settings) {
        macro_rules! take { ($($f:ident),*) => { $( if o.$f.is_some() { self.$f = o.$f; } )* } }
        take!(
            remote_executor, remote_cache, remote_instance_name, remote_timeout, results_url,
            bes_backend, tls_certificate, tls_client_certificate, tls_client_key
        );
        self.remote_header.extend(o.remote_header);
        self.exec_properties.extend(o.exec_properties);
    }
}

#[derive(Debug, Default, Deserialize)]
struct Section {
    remote_executor: Option<String>,
    remote_cache: Option<String>,
    remote_instance_name: Option<String>,
    #[serde(default)]
    remote_header: Vec<String>,
    remote_timeout: Option<String>,
    #[serde(default)]
    exec_properties: BTreeMap<String, String>,
    results_url: Option<String>,
    bes_backend: Option<String>,
    tls_certificate: Option<PathBuf>,
    tls_client_certificate: Option<PathBuf>,
    tls_client_key: Option<PathBuf>,
    /// Keys used by later chunks (download policy, path mode, ...). Accepted and ignored by D1.
    #[serde(flatten)]
    extra: HashMap<String, toml::Value>,
}

impl Section {
    fn into_settings(self, origin: &str, warnings: &mut Vec<String>) -> Settings {
        for k in self.extra.keys() {
            warnings.push(format!("{origin}: key '{k}' is not used by this version (ignored)"));
        }
        Settings {
            remote_executor: self.remote_executor,
            remote_cache: self.remote_cache,
            remote_instance_name: self.remote_instance_name,
            remote_header: self.remote_header,
            remote_timeout: self.remote_timeout,
            exec_properties: self.exec_properties,
            results_url: self.results_url,
            bes_backend: self.bes_backend,
            tls_certificate: self.tls_certificate,
            tls_client_certificate: self.tls_client_certificate,
            tls_client_key: self.tls_client_key,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct FileCfg {
    #[serde(default)]
    import: Vec<String>,
    common: Option<Section>,
    #[serde(default)]
    config: BTreeMap<String, Section>,
}

pub struct Loaded {
    pub settings: Settings,
    pub files_read: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

fn load_file(
    path: &Path,
    names: &[String],
    seen_names: &mut HashSet<String>,
    visited: &mut HashSet<PathBuf>,
    out: &mut Loaded,
) -> Result<()> {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canon) {
        bail!("import cycle at {}", path.display());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: FileCfg = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    out.files_read.push(path.to_path_buf());
    let origin = path.display().to_string();
    let mut s = Settings::default();
    if let Some(c) = cfg.common {
        s.overlay(c.into_settings(&format!("{origin} [common]"), &mut out.warnings));
    }
    let mut cfgs = cfg.config;
    for n in names {
        if let Some(c) = cfgs.remove(n) {
            seen_names.insert(n.clone());
            s.overlay(c.into_settings(&format!("{origin} [config.{n}]"), &mut out.warnings));
        }
    }
    out.settings.overlay(s);
    let dir = path.parent().unwrap_or(Path::new("."));
    for imp in cfg.import {
        let (optional, rel) = match imp.strip_prefix('?') {
            Some(r) => (true, r),
            None => (false, imp.as_str()),
        };
        let p = dir.join(rel);
        if !p.exists() {
            if optional {
                continue;
            }
            bail!("{origin}: import '{rel}' not found");
        }
        load_file(&p, names, seen_names, visited, out)?;
    }
    Ok(())
}

fn find_project_file(explicit: Option<&Path>) -> Result<Option<PathBuf>> {
    if let Some(p) = explicit {
        if !p.exists() {
            bail!("--config_file {} does not exist", p.display());
        }
        return Ok(Some(p.to_path_buf()));
    }
    let mut dir = std::env::current_dir()?;
    loop {
        let c = dir.join(".cargo-rbe.toml");
        if c.exists() {
            return Ok(Some(c));
        }
        if !dir.pop() {
            return Ok(None);
        }
    }
}

fn user_file() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    let p = base.join("cargo-rbe").join("config.toml");
    p.exists().then_some(p)
}

fn env_settings() -> Settings {
    let g = |k: &str| std::env::var(format!("CARGO_RBE_{k}")).ok().filter(|v| !v.is_empty());
    Settings {
        remote_executor: g("REMOTE_EXECUTOR"),
        remote_cache: g("REMOTE_CACHE"),
        remote_instance_name: g("REMOTE_INSTANCE_NAME"),
        remote_timeout: g("REMOTE_TIMEOUT"),
        results_url: g("RESULTS_URL"),
        bes_backend: g("BES_BACKEND"),
        ..Default::default()
    }
}

pub fn load(config_names: &[String], config_file: Option<&Path>, cli: Settings) -> Result<Loaded> {
    let mut names: Vec<String> = config_names
        .iter()
        .flat_map(|s| s.split(',').map(|x| x.trim().to_string()).collect::<Vec<_>>())
        .filter(|s| !s.is_empty())
        .collect();
    if names.is_empty() {
        if let Ok(e) = std::env::var("CARGO_RBE_CONFIG") {
            names = e.split(',').map(|x| x.trim().to_string()).filter(|s| !s.is_empty()).collect();
        }
    }
    let mut out = Loaded { settings: Settings::default(), files_read: vec![], warnings: vec![] };
    let mut seen = HashSet::new();
    let mut visited = HashSet::new();
    if let Some(u) = user_file() {
        load_file(&u, &names, &mut seen, &mut visited, &mut out)?;
    }
    if let Some(p) = find_project_file(config_file)? {
        load_file(&p, &names, &mut seen, &mut visited, &mut out)?;
    }
    for n in &names {
        if !seen.contains(n) {
            bail!("config '{n}' is not defined in any loaded config file (files read: {:?})", out.files_read);
        }
    }
    out.settings.overlay(env_settings());
    out.settings.overlay(cli);
    Ok(out)
}

/// Resolve "name=value" where value may be `@env:NAME` or `@file:path`. Never logs the value.
pub fn resolve_header(spec: &str) -> Result<(String, String)> {
    let (name, val) = spec
        .split_once('=')
        .ok_or_else(|| anyhow!("header spec must look like name=value (got a spec without '=')"))?;
    let name = name.trim().to_ascii_lowercase();
    if name.is_empty() || name.ends_with("-bin") {
        bail!("invalid header name '{name}'");
    }
    let value = if let Some(var) = val.strip_prefix("@env:") {
        std::env::var(var).map_err(|_| anyhow!("header '{name}': environment variable {var} is not set"))?
    } else if let Some(path) = val.strip_prefix("@file:") {
        std::fs::read_to_string(path)
            .with_context(|| format!("header '{name}': reading {path}"))?
            .trim_end_matches(['\n', '\r'])
            .to_string()
    } else {
        val.to_string()
    };
    Ok((name, value))
}

pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let (num, mult) = if let Some(n) = s.strip_suffix("ms") {
        (n, 0.001)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1.0)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60.0)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600.0)
    } else {
        (s, 1.0)
    };
    let v: f64 = num.trim().parse().map_err(|_| anyhow!("bad duration '{s}' (use e.g. 30s, 5m, 3600)"))?;
    Ok(Duration::from_secs_f64(v * mult))
}

/// Expand header specs into (name, value) pairs. A spec that *starts* with `@env:` or `@file:`
/// refers to a source holding one or more `name=value` lines (so even the header name can live in
/// a CI secret). Otherwise a spec is `name=value`, where the value may itself be `@env:`/`@file:`.
pub fn expand_header_specs(specs: &[String]) -> Result<Vec<(String, String)>> {
    let mut out = vec![];
    for spec in specs {
        let whole = if let Some(var) = spec.strip_prefix("@env:") {
            Some(std::env::var(var).map_err(|_| anyhow!("header source: environment variable {var} is not set"))?)
        } else if let Some(path) = spec.strip_prefix("@file:") {
            Some(std::fs::read_to_string(path).with_context(|| format!("header source: reading {path}"))?)
        } else {
            None
        };
        match whole {
            Some(text) => {
                for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
                    out.push(resolve_header(line)?);
                }
            }
            None => out.push(resolve_header(spec)?),
        }
    }
    Ok(out)
}
