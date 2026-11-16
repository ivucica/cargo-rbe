mod bes;
mod config;
#[allow(clippy::all, dead_code, non_camel_case_types, unused_qualifications)]
mod proto;
mod rpc;
mod smoke;

use anyhow::{anyhow, Result};
use clap::{Args, Parser, Subcommand};
use config::Settings;
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
