use anyhow::{anyhow, Result};
use cargo_rbe::config::Settings;
use cargo_rbe::{bes, config, rpc, smoke};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "cargo-rbe", version, about = "REAPI client experiments for Cargo builds")]
struct Cli {
    /// Named config section(s) to apply (like Bazel's build:<name>). Repeatable or comma-separated.
    #[arg(long = "config", global = true)]
    config: Vec<String>,
    /// Path to the project config file (default: nearest .cargo-rbe.toml).
    #[arg(long = "config_file", global = true)]
    config_file: Option<PathBuf>,
    #[arg(long = "remote_executor", global = true)]
    remote_executor: Option<String>,
    #[arg(long = "remote_cache", global = true)]
    remote_cache: Option<String>,
    #[arg(long = "remote_instance_name", global = true)]
    remote_instance_name: Option<String>,
    /// name=value; value may be @env:NAME or @file:path. Repeatable.
    #[arg(long = "remote_header", global = true)]
    remote_header: Vec<String>,
    #[arg(long = "remote_timeout", global = true)]
    remote_timeout: Option<String>,
    /// key=value platform property placed on every Action. Repeatable.
    #[arg(long = "remote_default_exec_properties", global = true)]
    exec_properties: Vec<String>,
    #[arg(long = "tls_certificate", global = true)]
    tls_certificate: Option<PathBuf>,
    #[arg(long = "tls_client_certificate", global = true)]
    tls_client_certificate: Option<PathBuf>,
    #[arg(long = "tls_client_key", global = true)]
    tls_client_key: Option<PathBuf>,
    #[arg(long = "bes_backend", global = true)]
    bes_backend: Option<String>,
    /// name=value header sent only to the BES backend (BES also receives --remote_header). Repeatable.
    #[arg(long = "bes_header", global = true)]
    bes_header: Vec<String>,
    /// KEY=VALUE published in the BuildMetadata event (e.g. VISIBILITY=PUBLIC). Repeatable.
    #[arg(long = "build_metadata", global = true)]
    build_metadata: Vec<String>,
    /// URL template; {invocation_id} is substituted. Printed only if set.
    #[arg(long = "results_url", global = true)]
    results_url: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Exercise capabilities, CAS, ByteStream, ActionCache and (if available) remote execution.
    Smoke(SmokeArgs),
    /// Print the effective configuration (header values are never printed).
    PrintConfig,
    /// Run `cargo <args>` with the rustc wrapper set up: `cargo-rbe --config=remote cargo build`.
    Cargo(CargoArgs),
}

#[derive(Args)]
struct CargoArgs {
    /// off | digest-only | read-only | cache
    #[arg(long, default_value = "cache")]
    mode: String,
    /// Path remapping for location-independent artifacts: registry | none
    #[arg(long, default_value = "registry")]
    remap: String,
    /// Where to write the JSONL action log (default: a per-invocation file in the cache directory).
    #[arg(long)]
    log: Option<PathBuf>,
    /// Arguments passed to cargo, e.g. `build --release`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
    args: Vec<String>,
}

#[derive(Args)]
struct SmokeArgs {
    /// Write a machine-readable JSON report here (safe to share: contains no header values).
    #[arg(long)]
    json_out: Option<PathBuf>,
    /// Reuse a nonce to test cache hits across separate runs (default: random per run).
    #[arg(long)]
    nonce: Option<String>,
}

fn nz(o: &Option<String>) -> Option<String> {
    o.clone().filter(|s| !s.trim().is_empty())
}

fn cli_settings(c: &Cli) -> Result<Settings> {
    let mut props = std::collections::BTreeMap::new();
    for p in c.exec_properties.iter().filter(|p| !p.trim().is_empty()) {
        let (k, v) = p.split_once('=').ok_or_else(|| anyhow!("--remote_default_exec_properties expects key=value"))?;
        props.insert(k.to_string(), v.to_string());
    }
    let mut meta = std::collections::BTreeMap::new();
    for m in c.build_metadata.iter().filter(|m| !m.trim().is_empty()) {
        let (k, v) = m.split_once('=').ok_or_else(|| anyhow!("--build_metadata expects KEY=VALUE"))?;
        meta.insert(k.to_string(), v.to_string());
    }
    Ok(Settings {
        bes_header: c.bes_header.iter().filter(|h| !h.trim().is_empty()).cloned().collect(),
        build_metadata: meta,
        remote_executor: nz(&c.remote_executor),
        remote_cache: nz(&c.remote_cache),
        remote_instance_name: nz(&c.remote_instance_name),
        remote_header: c.remote_header.iter().filter(|h| !h.trim().is_empty()).cloned().collect(),
        remote_timeout: nz(&c.remote_timeout),
        exec_properties: props,
        results_url: nz(&c.results_url),
        bes_backend: nz(&c.bes_backend),
        tls_certificate: c.tls_certificate.clone(),
        tls_client_certificate: c.tls_client_certificate.clone(),
        tls_client_key: c.tls_client_key.clone(),
    })
}

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("error: {e:#}");
        std::process::exit(2);
    }
}

async fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let loaded = config::load(&cli.config, cli.config_file.as_deref(), cli_settings(&cli)?)?;
    let s = loaded.settings;
    for w in &loaded.warnings {
        eprintln!("warning: {w}");
    }
    match cli.cmd {
        Cmd::PrintConfig => {
            println!("config files read: {:?}", loaded.files_read);
            println!("remote_executor: {:?}", s.remote_executor);
            println!("remote_cache: {:?}", s.remote_cache);
            println!("remote_instance_name: {:?}", s.remote_instance_name);
            println!("remote_header names (values hidden): {:?}", rpc::header_names(&s));
            println!("remote_timeout: {:?}", s.remote_timeout);
            println!("exec_properties: {:?}", s.exec_properties);
            println!("results_url: {:?}", s.results_url);
            println!("bes_backend: {:?}", s.bes_backend);
            println!("bes_header names (values hidden): {:?}", rpc::header_names(&Settings { remote_header: s.bes_header.clone(), ..Default::default() }));
            println!("build_metadata: {:?}", s.build_metadata);
            Ok(())
        }
        Cmd::Cargo(a) => run_cargo(s, a).await,
        Cmd::Smoke(a) => {
            let invocation_id = uuid::Uuid::new_v4().to_string();
            let nonce = a.nonce.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let cache_url = s.remote_cache.clone().or_else(|| s.remote_executor.clone()).ok_or_else(|| anyhow!("set --remote_cache and/or --remote_executor (or a config section that does)"))?;
            let results_url = s.results_url.as_ref().map(|t| t.replace("{invocation_id}", &invocation_id));
            println!("invocation_id: {invocation_id}");
            match &results_url {
                Some(u) => println!("results_url: {u}"),
                None => println!("results_url: (none configured)"),
            }
            let cache = rpc::connect(&cache_url, &s, &invocation_id).await?;
            let exec = match &s.remote_executor {
                Some(u) if *u == cache_url => None, // same endpoint: reuse the cache connection
                Some(u) => Some(rpc::connect(u, &s, &invocation_id).await?),
                None => None,
            };
            let same = s.remote_executor.as_deref() == Some(cache_url.as_str());
            let exec_ref: Option<&rpc::Conn> = if same { Some(&cache) } else { exec.as_ref() };
            // Optional BES publishing (invocation page on servers that build it from build events).
            let mut bes_setup_error: Option<String> = None;
            let bes = match &s.bes_backend {
                None => None,
                Some(url) => {
                    let mut bs = s.clone();
                    bs.remote_header.extend(s.bes_header.clone());
                    match rpc::connect(url, &bs, &invocation_id).await {
                        Ok(conn) => Some(
                            bes::Bes::start(
                                conn,
                                bes::BesConfig {
                                    invocation_id: invocation_id.clone(),
                                    command_line: bes::redact_args(&std::env::args().collect::<Vec<_>>()),
                                    command: "smoke".into(),
                                    tool_tag: "cargo-rbe".into(),
                                    build_metadata: s.build_metadata.clone(),
                                    workspace_status: bes::ci_workspace_status(),
                                },
                            )
                            .await?,
                        ),
                        Err(e) => {
                            bes_setup_error = Some(format!("{e:#}"));
                            None
                        }
                    }
                }
            };
            let mut rep = smoke::run(&s, &cache, exec_ref, &invocation_id, nonce, results_url.clone(), bes).await?;
            if s.bes_backend.is_some() {
                let t = std::time::Instant::now();
                let ok_so_far = !rep.failed();
                let (status, detail) = match (rep.bes.take(), bes_setup_error) {
                    (Some(b), _) => {
                        let json = serde_json::to_vec_pretty(&rep)?;
                        let sum = b.finish(ok_so_far, &json).await;
                        let hard: Vec<_> = sum.notes.iter().filter(|n| !n.starts_with("lifecycle")).collect();
                        let d = format!(
                            "published {} events, {} acknowledged (build_id {}){}",
                            sum.events_sent,
                            sum.events_acked,
                            sum.build_id,
                            if sum.notes.is_empty() { String::new() } else { format!("\nnotes: {}", sum.notes.join("; ")) }
                        );
                        if sum.events_acked < sum.events_sent || !hard.is_empty() { ("FAIL", d) } else { ("ok", d) }
                    }
                    (None, Some(e)) => ("FAIL", format!("could not connect to bes_backend: {e}")),
                    (None, None) => ("skip", "no events published".to_string()),
                };
                println!("[{status:>4}] bes: {}", detail.replace('\n', "\n         "));
                if status == "ok" {
                    if let Some(u) = &results_url {
                        println!("         invocation page (if the server builds one from events): {u}");
                    }
                }
                rep.steps.push(smoke::Step { name: "bes".into(), status, detail, millis: t.elapsed().as_millis() });
            }
            if let Some(p) = a.json_out {
                std::fs::write(&p, serde_json::to_string_pretty(&rep)?)?;
                println!("wrote {}", p.display());
            }
            let failed = rep.steps.iter().filter(|x| x.status == "FAIL").count();
            let skipped = rep.steps.iter().filter(|x| x.status == "skip").count();
            println!("\nsummary: {} ok, {} skipped, {} failed", rep.steps.len() - failed - skipped, skipped, failed);
            if rep.failed() {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

fn summarize_log(path: &std::path::Path) -> (Vec<String>, bool) {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut counts: std::collections::BTreeMap<String, u64> = Default::default();
    let mut reasons: std::collections::BTreeMap<String, u64> = Default::default();
    let (mut down, mut up) = (0u64, 0u64);
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        *counts.entry(v["outcome"].as_str().unwrap_or("?").to_string()).or_default() += 1;
        down += v["bytes_down"].as_u64().unwrap_or(0);
        up += v["bytes_up"].as_u64().unwrap_or(0);
        if let Some(r) = v["reason"].as_str() {
            *reasons.entry(r.chars().take(110).collect()).or_default() += 1;
        }
        if let Some(r) = v["publish"].as_str().filter(|p| *p != "ok") {
            *reasons.entry(format!("publish: {}", r.chars().take(100).collect::<String>())).or_default() += 1;
        }
    }
    let total: u64 = counts.values().sum();
    let mut lines = vec![format!("cargo-rbe: {total} rustc invocations logged ({})", path.display())];
    for (k, n) in &counts {
        lines.push(format!("  {k:<22} {n}"));
    }
    lines.push(format!("  restored from cache: {down} bytes, uploaded: {up} bytes"));
    let mut rs: Vec<_> = reasons.into_iter().collect();
    rs.sort_by(|a, b| b.1.cmp(&a.1));
    for (r, n) in rs.into_iter().take(6) {
        lines.push(format!("  {n:>4} x {r}"));
    }
    (lines, total > 0)
}

async fn run_cargo(s: Settings, a: CargoArgs) -> Result<()> {
    let mode = cargo_rbe::wrap::Mode::parse(&a.mode).ok_or_else(|| anyhow!("--mode must be off, digest-only, read-only or cache"))?;
    let invocation_id = uuid::Uuid::new_v4().to_string();
    let wrapper = std::env::current_exe()?.with_file_name("cargo-rbe-wrap");
    if !wrapper.exists() {
        return Err(anyhow!("{} not found; build both binaries (cargo build --release)", wrapper.display()));
    }
    let cache = cargo_rbe::wrap::cache_dir();
    let log = a.log.clone().unwrap_or_else(|| cache.join(format!("actions-{invocation_id}.jsonl")));
    if let Some(d) = log.parent() {
        std::fs::create_dir_all(d)?;
    }
    let _ = std::fs::remove_file(&log);
    eprintln!("cargo-rbe: mode {} | invocation {invocation_id}", a.mode);
    if let Some(t) = &s.results_url {
        eprintln!("cargo-rbe: results {}", t.replace("{invocation_id}", &invocation_id));
    }

    let mut bes = None;
    if let Some(url) = &s.bes_backend {
        let mut bs = s.clone();
        bs.remote_header.extend(s.bes_header.clone());
        match rpc::connect_with(url, &bs, &invocation_id, "cargo").await {
            Ok(conn) => {
                let argv = bes::redact_args(&std::env::args().collect::<Vec<_>>());
                bes = Some(
                    bes::Bes::start(
                        conn,
                        bes::BesConfig {
                            invocation_id: invocation_id.clone(),
                            command_line: argv,
                            command: a.args.first().cloned().unwrap_or_else(|| "build".into()),
                            tool_tag: "cargo-rbe".into(),
                            build_metadata: s.build_metadata.clone(),
                            workspace_status: bes::ci_workspace_status(),
                        },
                    )
                    .await?,
                );
            }
            Err(e) => eprintln!("cargo-rbe: warning: BES backend not reachable, continuing without build events: {e:#}"),
        }
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = std::process::Command::new(cargo);
    cmd.args(&a.args)
        .env("RUSTC_WRAPPER", &wrapper)
        .env("CARGO_RBE_MODE", &a.mode)
        .env("CARGO_RBE_REMAP", &a.remap)
        .env("CARGO_RBE_LOG", &log)
        .env("CARGO_RBE_INVOCATION_ID", &invocation_id)
        .env("CARGO_RBE_WRAP_SETTINGS", serde_json::to_string(&s)?);
    if let Some(id) = cargo_rbe::wrap::rustc_identity_for_launcher() {
        cmd.env("CARGO_RBE_RUSTC_ID", id);
    }
    let status = if mode == cargo_rbe::wrap::Mode::Off { cmd.env_remove("RUSTC_WRAPPER").status() } else { cmd.status() }?;
    let code = status.code().unwrap_or(1);

    let (lines, _) = summarize_log(&log);
    for l in &lines {
        eprintln!("{l}");
    }
    if let Some(mut b) = bes {
        for l in &lines {
            b.progress(l).await;
        }
        let mut log_bytes = std::fs::read(&log).unwrap_or_default();
        log_bytes.truncate(1024 * 1024);
        let sum = b.finish(code == 0, &log_bytes).await;
        eprintln!("cargo-rbe: build events: {} sent, {} acknowledged{}", sum.events_sent, sum.events_acked, if sum.notes.is_empty() { String::new() } else { format!(" ({})", sum.notes.join("; ")) });
    }
    std::process::exit(code);
}
