//! D1 smoke test: exercises the REAPI surface we depend on, step by step, and reports exactly
//! what a given server does. Every step is independent where possible so one failure does not
//! hide the others. Nothing here is specific to a particular service.

use crate::config::{parse_duration, Settings};
use crate::proto::build::bazel::remote::execution::v2 as re;
use crate::proto::google::longrunning::operation::Result as OpResult;
use crate::rpc::{self, digest_of, fmt_digest, Conn};
use anyhow::{anyhow, bail, Result};
use prost::Message;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use std::time::{Duration, Instant};

pub enum Outcome {
    Ok(String),
    Skip(String),
}

#[derive(Serialize)]
pub struct Step {
    pub name: String,
    pub status: &'static str,
    pub detail: String,
    pub millis: u128,
}

#[derive(Serialize, Default)]
pub struct Report {
    pub tool_version: String,
    pub invocation_id: String,
    pub results_url: Option<String>,
    pub cache_endpoint: Option<String>,
    pub exec_endpoint: Option<String>,
    pub instance_name: String,
    pub header_names: Vec<String>,
    pub exec_properties: std::collections::BTreeMap<String, String>,
    pub capabilities: serde_json::Value,
    pub steps: Vec<Step>,
    #[serde(skip)]
    pub bes: Option<crate::bes::Bes>,
}

impl Report {
    /// Print a line and forward it to the BES console output (if publishing).
    pub async fn log(&mut self, line: String) {
        println!("{line}");
        if let Some(b) = self.bes.as_mut() {
            b.progress(&line).await;
        }
    }
    pub fn failed(&self) -> bool {
        self.steps.iter().any(|s| s.status == "FAIL")
    }
    async fn run<F: std::future::Future<Output = Result<Outcome>>>(&mut self, name: &str, timeout: Duration, f: F) {
        let t = Instant::now();
        let r = match tokio::time::timeout(timeout, f).await {
            Ok(r) => r,
            Err(_) => Err(anyhow!("timed out after {:?}", timeout)),
        };
        let (status, detail) = match r {
            Ok(Outcome::Ok(d)) => ("ok", d),
            Ok(Outcome::Skip(d)) => ("skip", d),
            Err(e) => ("FAIL", format!("{e:#}")),
        };
        let line = format!("[{status:>4}] {name}: {}", detail.replace('\n', "\n         "));
        self.log(line).await;
        self.steps.push(Step { name: name.into(), status, detail, millis: t.elapsed().as_millis() });
    }
}

pub struct Ctx<'a> {
    pub cache: &'a Conn,
    pub exec: Option<&'a Conn>,
    pub instance: String,
    pub platform: re::Platform,
    pub nonce: String,
}

fn semver(v: &Option<crate::proto::build::bazel::semver::SemVer>) -> String {
    match v {
        Some(s) => format!("{}.{}.{}{}", s.major, s.minor, s.patch, if s.prerelease.is_empty() { String::new() } else { format!("-{}", s.prerelease) }),
        None => "-".into(),
    }
}

fn names<T: TryFrom<i32>>(vals: &[i32], f: impl Fn(T) -> &'static str) -> Vec<&'static str> {
    vals.iter().map(|v| T::try_from(*v).map(&f).unwrap_or("UNKNOWN")).collect()
}

async fn caps_step(c: &Conn, instance: &str, label: &str) -> Result<(Outcome, re::ServerCapabilities)> {
    let caps = c
        .capabilities()
        .get_capabilities(re::GetCapabilitiesRequest { instance_name: instance.into() })
        .await
        .map_err(|e| anyhow!("GetCapabilities on {label} ({}): {e}", c.url))?
        .into_inner();
    let mut lines = vec![format!("endpoint ({label}): {}", c.url)];
    lines.push(format!("api versions: low={} high={} deprecated={}", semver(&caps.low_api_version), semver(&caps.high_api_version), semver(&caps.deprecated_api_version)));
    if let Some(cc) = &caps.cache_capabilities {
        lines.push(format!("cache: digest_functions={:?}", names::<re::digest_function::Value>(&cc.digest_functions, |v| v.as_str_name())));
        lines.push(format!(
            "cache: ac_update={} max_batch_bytes={} max_blob_bytes={} compressors={:?} batch_update_compressors={:?}",
            cc.action_cache_update_capabilities.as_ref().map(|u| u.update_enabled).unwrap_or(false),
            cc.max_batch_total_size_bytes,
            cc.max_cas_blob_size_bytes,
            names::<re::compressor::Value>(&cc.supported_compressors, |v| v.as_str_name()),
            names::<re::compressor::Value>(&cc.supported_batch_update_compressors, |v| v.as_str_name()),
        ));
        let sha = re::digest_function::Value::Sha256 as i32;
        if !cc.digest_functions.is_empty() && !cc.digest_functions.contains(&sha) {
            bail!("server does not list SHA256 among cache digest functions; this version only supports SHA256\n{}", lines.join("\n"));
        }
    } else {
        lines.push("cache capabilities: not advertised".into());
    }
    if let Some(ec) = &caps.execution_capabilities {
        let mut all = ec.digest_functions.clone();
        if all.is_empty() {
            all.push(ec.digest_function);
        }
        lines.push(format!("execution: enabled={} digest_functions={:?} node_properties={:?}", ec.exec_enabled, names::<re::digest_function::Value>(&all, |v| v.as_str_name()), ec.supported_node_properties));
    } else {
        lines.push("execution capabilities: not advertised".into());
    }
    Ok((Outcome::Ok(lines.join("\n")), caps))
}

fn nonce_bytes(nonce: &str, n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n);
    let mut i = 0u64;
    while out.len() < n {
        let mut h = Sha256::new();
        h.update(nonce.as_bytes());
        h.update(i.to_le_bytes());
        out.extend_from_slice(&h.finalize());
        i += 1;
    }
    out.truncate(n);
    out
}

async fn cas_batch(ctx: &Ctx<'_>) -> Result<Outcome> {
    let a = format!("cargo-rbe cas-batch A {}\n", ctx.nonce).into_bytes();
    let b = format!("cargo-rbe cas-batch B {}\n", ctx.nonce).into_bytes();
    let blobs = vec![(digest_of(&a), a.clone()), (digest_of(&b), b.clone())];
    let first = rpc::ensure_blobs(ctx.cache, &ctx.instance, &blobs).await?;
    let second = rpc::ensure_blobs(ctx.cache, &ctx.instance, &blobs).await?;
    let back = rpc::fetch(ctx.cache, &ctx.instance, &blobs[0].0).await?;
    if back != a {
        bail!("BatchReadBlobs returned different bytes than uploaded");
    }
    if first != 2 || second != 0 {
        bail!("expected 2 uploads then 0 (dedup); got {first} then {second}");
    }
    Ok(Outcome::Ok(format!("FindMissingBlobs+BatchUpdateBlobs+BatchReadBlobs ok; uploaded {first}, then {second} (deduplicated)")))
}

async fn cas_bytestream(ctx: &Ctx<'_>) -> Result<Outcome> {
    let data = nonce_bytes(&ctx.nonce, 1024 * 1024 + 17);
    let d = digest_of(&data);
    rpc::bs_write(ctx.cache, &ctx.instance, &data).await?;
    let missing = ctx
        .cache
        .cas()
        .find_missing_blobs(re::FindMissingBlobsRequest { instance_name: ctx.instance.clone(), blob_digests: vec![d.clone()], digest_function: 0 })
        .await?
        .into_inner()
        .missing_blob_digests;
    if !missing.is_empty() {
        bail!("blob {} still reported missing after ByteStream.Write", fmt_digest(&d));
    }
    let back = rpc::bs_read(ctx.cache, &ctx.instance, &d).await?;
    if digest_of(&back) != d {
        bail!("ByteStream.Read returned different bytes ({} bytes)", back.len());
    }
    Ok(Outcome::Ok(format!("ByteStream write+read of {} bytes verified ({})", data.len(), fmt_digest(&d))))
}

fn make_action(ctx: &Ctx<'_>, cmd: re::Command, input_root: re::Directory, extra: Vec<(re::Digest, Vec<u8>)>) -> (re::Digest, Vec<(re::Digest, Vec<u8>)>) {
    let cmd_bytes = cmd.encode_to_vec();
    let dir_bytes = input_root.encode_to_vec();
    let action = re::Action {
        command_digest: Some(digest_of(&cmd_bytes)),
        input_root_digest: Some(digest_of(&dir_bytes)),
        timeout: Some(prost_types::Duration { seconds: 120, nanos: 0 }),
        do_not_cache: false,
        salt: vec![],
        platform: Some(ctx.platform.clone()),
    };
    let action_bytes = action.encode_to_vec();
    let ad = digest_of(&action_bytes);
    let mut blobs = extra;
    blobs.push((digest_of(&cmd_bytes), cmd_bytes));
    blobs.push((digest_of(&dir_bytes), dir_bytes));
    blobs.push((ad.clone(), action_bytes));
    (ad, blobs)
}

async fn ac_roundtrip(ctx: &Ctx<'_>, update_enabled: Option<bool>) -> Result<Outcome> {
    if update_enabled == Some(false) {
        return Ok(Outcome::Skip("server reports ActionCache updates disabled for this client".into()));
    }
    let cmd = re::Command { arguments: vec!["true".into()], platform: Some(ctx.platform.clone()), output_paths: vec![], working_directory: String::new(), ..Default::default() };
    let mut cmd = cmd;
    cmd.environment_variables.push(re::command::EnvironmentVariable { name: "CARGO_RBE_AC_NONCE".into(), value: ctx.nonce.clone() });
    let (ad, blobs) = make_action(ctx, cmd, re::Directory::default(), vec![]);
    rpc::ensure_blobs(ctx.cache, &ctx.instance, &blobs).await?;
    let mut ac = ctx.cache.ac();
    let get = |ad: re::Digest| re::GetActionResultRequest { instance_name: ctx.instance.clone(), action_digest: Some(ad), inline_stdout: true, inline_stderr: true, inline_output_files: vec![], digest_function: 0 };
    let before = ac.get_action_result(get(ad.clone())).await;
    let before_s = match &before {
        Ok(_) => "unexpected hit".to_string(),
        Err(e) if e.code() == tonic::Code::NotFound => "miss (expected)".to_string(),
        Err(e) => format!("error {:?}: {}", e.code(), e.message()),
    };
    let result = re::ActionResult { exit_code: 0, stdout_raw: b"cargo-rbe ac roundtrip\n".to_vec(), ..Default::default() };
    ac.update_action_result(re::UpdateActionResultRequest { instance_name: ctx.instance.clone(), action_digest: Some(ad.clone()), action_result: Some(result), results_cache_policy: None, digest_function: 0 })
        .await
        .map_err(|e| anyhow!("UpdateActionResult: {:?}: {}", e.code(), e.message()))?;
    let after = ac.get_action_result(get(ad.clone())).await.map_err(|e| anyhow!("GetActionResult after update: {:?}: {}", e.code(), e.message()))?.into_inner();
    let mut stdout = after.stdout_raw.clone();
    if stdout.is_empty() {
        if let Some(d) = &after.stdout_digest {
            stdout = rpc::fetch(ctx.cache, &ctx.instance, d).await?;
        }
    }
    if stdout != b"cargo-rbe ac roundtrip\n" || after.exit_code != 0 {
        bail!("ActionResult did not round-trip (exit_code={}, stdout={:?})", after.exit_code, String::from_utf8_lossy(&stdout));
    }
    Ok(Outcome::Ok(format!("GetActionResult before update: {before_s}; UpdateActionResult ok; GetActionResult after: hit, round-tripped")))
}

struct Exec {
    resp: re::ExecuteResponse,
    stages: Vec<String>,
    op_name: String,
}

async fn execute(ctx: &Ctx<'_>, ad: &re::Digest) -> Result<Exec> {
    let ec = ctx.exec.ok_or_else(|| anyhow!("no executor configured"))?;
    let mut client = ec.exec();
    let mut stream = client
        .execute(re::ExecuteRequest { instance_name: ctx.instance.clone(), skip_cache_lookup: false, action_digest: Some(ad.clone()), ..Default::default() })
        .await
        .map_err(|e| anyhow!("Execute: {:?}: {}", e.code(), e.message()))?
        .into_inner();
    let mut stages = vec![];
    let mut op_name = String::new();
    let mut retries = 0;
    loop {
        match stream.message().await {
            Ok(Some(op)) => {
                if !op.name.is_empty() {
                    op_name = op.name.clone();
                }
                if let Some(m) = &op.metadata {
                    if m.type_url.ends_with("ExecuteOperationMetadata") {
                        if let Ok(md) = re::ExecuteOperationMetadata::decode(&m.value[..]) {
                            let s = re::execution_stage::Value::try_from(md.stage).map(|s| s.as_str_name()).unwrap_or("?");
                            if stages.last().map(|l: &String| l != s).unwrap_or(true) {
                                stages.push(s.to_string());
                            }
                        }
                    }
                }
                if op.done {
                    return match op.result {
                        Some(OpResult::Response(any)) => {
                            let resp = re::ExecuteResponse::decode(&any.value[..])?;
                            Ok(Exec { resp, stages, op_name })
                        }
                        Some(OpResult::Error(st)) => bail!("operation failed: code {} {}", st.code, st.message),
                        None => bail!("operation done without a result"),
                    };
                }
            }
            Ok(None) | Err(_) if !op_name.is_empty() && retries < 5 => {
                retries += 1;
                stream = client
                    .wait_execution(re::WaitExecutionRequest { name: op_name.clone() })
                    .await
                    .map_err(|e| anyhow!("WaitExecution: {:?}: {}", e.code(), e.message()))?
                    .into_inner();
            }
            Ok(None) => bail!("Execute stream ended before the operation completed (no operation name to resume)"),
            Err(e) => bail!("Execute stream error: {:?}: {}", e.code(), e.message()),
        }
    }
}

async fn blob_or_raw(ctx: &Ctx<'_>, raw: &[u8], d: &Option<re::Digest>) -> Result<Vec<u8>> {
    if !raw.is_empty() {
        return Ok(raw.to_vec());
    }
    match d {
        Some(d) if d.size_bytes > 0 => rpc::fetch(ctx.cache, &ctx.instance, d).await,
        _ => Ok(vec![]),
    }
}

async fn exec_step(ctx: &Ctx<'_>, second: bool) -> Result<Outcome> {
    let input = format!("hello from cargo-rbe smoke\n").into_bytes();
    let input_d = digest_of(&input);
    let dir = re::Directory { files: vec![re::FileNode { name: "in.txt".into(), digest: Some(input_d.clone()), is_executable: false, node_properties: None }], ..Default::default() };
    let cmd = re::Command {
        arguments: vec!["/bin/sh".into(), "-c".into(), "uname -a; cat in.txt > out.txt; echo \"$CARGO_RBE_NONCE\" >> out.txt".into()],
        environment_variables: vec![
            re::command::EnvironmentVariable { name: "CARGO_RBE_NONCE".into(), value: ctx.nonce.clone() },
            re::command::EnvironmentVariable { name: "PATH".into(), value: "/usr/bin:/bin".into() },
        ],
        output_paths: vec!["out.txt".into()],
        platform: Some(ctx.platform.clone()),
        working_directory: String::new(),
        ..Default::default()
    };
    let (ad, blobs) = make_action(ctx, cmd, dir, vec![(input_d, input)]);
    let uploaded = rpc::ensure_blobs(ctx.cache, &ctx.instance, &blobs).await?;
    let mut notes = vec![format!("action {} ({} blobs uploaded)", fmt_digest(&ad), uploaded)];
    if !second {
        let pre = ctx
            .cache
            .ac()
            .get_action_result(re::GetActionResultRequest { instance_name: ctx.instance.clone(), action_digest: Some(ad.clone()), inline_stdout: false, inline_stderr: false, inline_output_files: vec![], digest_function: 0 })
            .await;
        notes.push(match pre {
            Ok(_) => "pre-execute GetActionResult: already cached".into(),
            Err(e) if e.code() == tonic::Code::NotFound => "pre-execute GetActionResult: miss (expected)".into(),
            Err(e) => format!("pre-execute GetActionResult: {:?}: {}", e.code(), e.message()),
        });
    }
    let t = Instant::now();
    let ex = match execute(ctx, &ad).await {
        Ok(x) => x,
        Err(e) if format!("{e}").contains("Unimplemented") => {
            return Ok(Outcome::Skip(format!("server does not implement the Execution service ({e})")));
        }
        Err(e) => return Err(e),
    };
    notes.push(format!("operation {} finished in {:?}; stages seen: {:?}", ex.op_name, t.elapsed(), ex.stages));
    if let Some(st) = &ex.resp.status {
        if st.code != 0 {
            bail!("{}\nExecuteResponse status: code {} {} {}", notes.join("\n"), st.code, st.message, ex.resp.message);
        }
    }
    let res = ex.resp.result.clone().ok_or_else(|| anyhow!("ExecuteResponse has no result"))?;
    let stdout = blob_or_raw(ctx, &res.stdout_raw, &res.stdout_digest).await?;
    let stderr = blob_or_raw(ctx, &res.stderr_raw, &res.stderr_digest).await?;
    notes.push(format!("cached_result={} exit_code={}", ex.resp.cached_result, res.exit_code));
    if let Some(m) = &res.execution_metadata {
        if !m.worker.is_empty() {
            notes.push(format!("worker: {}", m.worker));
        }
    }
    notes.push(format!("stdout: {}", String::from_utf8_lossy(&stdout).trim_end()));
    if !stderr.is_empty() {
        notes.push(format!("stderr: {}", String::from_utf8_lossy(&stderr).trim_end()));
    }
    if res.exit_code != 0 {
        bail!("{}\nnon-zero exit code", notes.join("\n"));
    }
    let of = res.output_files.iter().find(|o| o.path == "out.txt").ok_or_else(|| anyhow!("{}\nout.txt missing from ActionResult.output_files", notes.join("\n")))?;
    let d = of.digest.clone().ok_or_else(|| anyhow!("out.txt has no digest"))?;
    let out = rpc::fetch(ctx.cache, &ctx.instance, &d).await?;
    let want = format!("hello from cargo-rbe smoke\n{}\n", ctx.nonce);
    if String::from_utf8_lossy(&out) != want {
        bail!("{}\nout.txt content mismatch: {:?}", notes.join("\n"), String::from_utf8_lossy(&out));
    }
    notes.push("out.txt downloaded from CAS and verified".into());
    if second && !ex.resp.cached_result {
        notes.push("NOTE: identical action was re-executed rather than served from the action cache (server may not cache, or caching may be restricted for this client)".into());
    }
    Ok(Outcome::Ok(notes.join("\n")))
}

pub async fn run(
    s: &Settings,
    cache: &Conn,
    exec: Option<&Conn>,
    invocation_id: &str,
    nonce: String,
    results_url: Option<String>,
    bes: Option<crate::bes::Bes>,
) -> Result<Report> {
    let timeout = parse_duration(s.remote_timeout.as_deref().unwrap_or("600s"))?;
    let instance = s.remote_instance_name.clone().unwrap_or_default();
    let platform = re::Platform { properties: s.exec_properties.iter().map(|(k, v)| re::platform::Property { name: k.clone(), value: v.clone() }).collect() };
    let mut rep = Report {
        tool_version: env!("CARGO_PKG_VERSION").into(),
        invocation_id: invocation_id.into(),
        results_url,
        cache_endpoint: Some(cache.url.clone()),
        exec_endpoint: exec.map(|e| e.url.clone()),
        instance_name: instance.clone(),
        header_names: rpc::header_names(s),
        exec_properties: s.exec_properties.clone(),
        bes,
        ..Default::default()
    };
    let ctx = Ctx { cache, exec, instance: instance.clone(), platform, nonce };

    let mut exec_enabled: Option<bool> = None;
    let mut ac_update: Option<bool> = None;
    let mut caps_json = serde_json::Map::new();
    {
        let t = Instant::now();
        let r = tokio::time::timeout(timeout, caps_step(cache, &instance, "cache")).await;
        let (status, detail) = match r {
            Ok(Ok((Outcome::Ok(d), caps))) => {
                ac_update = caps.cache_capabilities.as_ref().and_then(|c| c.action_cache_update_capabilities.as_ref()).map(|u| u.update_enabled);
                if exec.is_none() || exec.map(|e| e.url == cache.url).unwrap_or(false) {
                    exec_enabled = caps.execution_capabilities.as_ref().map(|e| e.exec_enabled);
                }
                caps_json.insert("cache_endpoint".into(), serde_json::json!({
                    "low": semver(&caps.low_api_version), "high": semver(&caps.high_api_version),
                    "exec_enabled": caps.execution_capabilities.as_ref().map(|e| e.exec_enabled),
                    "ac_update": ac_update,
                    "max_batch_bytes": caps.cache_capabilities.as_ref().map(|c| c.max_batch_total_size_bytes),
                }));
                ("ok", d)
            }
            Ok(Ok((Outcome::Skip(d), _))) => ("skip", d),
            Ok(Err(e)) => ("FAIL", format!("{e:#}")),
            Err(_) => ("FAIL", "timed out".into()),
        };
        rep.log(format!("[{status:>4}] capabilities: {}", detail.replace('\n', "\n         "))).await;
        rep.steps.push(Step { name: "capabilities".into(), status, detail, millis: t.elapsed().as_millis() });
    }
    if let Some(e) = exec {
        if e.url != cache.url {
            let t = Instant::now();
            match tokio::time::timeout(timeout, caps_step(e, &instance, "executor")).await {
                Ok(Ok((Outcome::Ok(d), caps))) => {
                    exec_enabled = caps.execution_capabilities.as_ref().map(|x| x.exec_enabled);
                    rep.log(format!("[  ok] capabilities (executor): {}", d.replace('\n', "\n         "))).await;
                    caps_json.insert("exec_endpoint".into(), serde_json::json!({"low": semver(&caps.low_api_version), "high": semver(&caps.high_api_version), "exec_enabled": exec_enabled}));
                    rep.steps.push(Step { name: "capabilities-executor".into(), status: "ok", detail: d, millis: t.elapsed().as_millis() });
                }
                other => {
                    let d = match other { Ok(Err(e)) => format!("{e:#}"), _ => "timed out".into() };
                    rep.log(format!("[FAIL] capabilities (executor): {d}")).await;
                    rep.steps.push(Step { name: "capabilities-executor".into(), status: "FAIL", detail: d, millis: t.elapsed().as_millis() });
                }
            }
        }
    }
    rep.capabilities = serde_json::Value::Object(caps_json);
    if rep.failed() {
        return Ok(rep); // cannot continue meaningfully without a working connection
    }

    rep.run("cas-batch", timeout, cas_batch(&ctx)).await;
    rep.run("cas-bytestream", timeout, cas_bytestream(&ctx)).await;
    rep.run("action-cache-roundtrip", timeout, ac_roundtrip(&ctx, ac_update)).await;

    match (exec, exec_enabled) {
        (None, _) => {
            rep.run("execute", timeout, async { Ok(Outcome::Skip("no remote_executor configured".into())) }).await;
        }
        (Some(_), Some(false)) => {
            rep.run("execute", timeout, async { Ok(Outcome::Skip("server reports execution disabled in GetCapabilities".into())) }).await;
        }
        (Some(_), _) => {
            rep.run("execute", timeout, exec_step(&ctx, false)).await;
            if rep.steps.last().map(|s| s.status) == Some("ok") {
                rep.run("execute-again", timeout, exec_step(&ctx, true)).await;
            } else {
                rep.log("[skip] execute-again: first execution did not succeed".to_string()).await;
            }
        }
    }
    Ok(rep)
}
