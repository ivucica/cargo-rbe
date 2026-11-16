//! Minimal Build Event Stream (BES) publisher.
//!
//! Why: some servers build their invocation page from build events, not from REAPI calls. The
//! REAPI request metadata links executions to an invocation ID, but without events the server has
//! no invocation record ("Invocation not found"). This publishes the smallest well-formed stream
//! that Bazel-compatible BES servers accept: lifecycle events plus a tool event stream with
//! Started, command line, workspace status, metadata, progress (console output), BuildFinished
//! and BuildToolLogs (the smoke report attached as a file).
//!
//! Event order follows Bazel's: BuildEnqueued and InvocationAttemptStarted (lifecycle, controller
//! stream), then the tool stream, then InvocationAttemptFinished and BuildFinished (lifecycle).
//! Every build event declares the children that will follow, and the last one sets last_message.

use crate::proto::build_event_stream as bep;
use crate::proto::command_line as cl;
use crate::proto::google::devtools::build::v1 as v1;
use crate::rpc::Conn;
use anyhow::{anyhow, Result};
use prost::Message;
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;

fn now_ts() -> prost_types::Timestamp {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    prost_types::Timestamp { seconds: d.as_secs() as i64, nanos: d.subsec_nanos() as i32 }
}

fn id(i: bep::build_event_id::Id) -> bep::BuildEventId {
    bep::BuildEventId { id: Some(i) }
}
fn progress_id(n: i32) -> bep::BuildEventId {
    id(bep::build_event_id::Id::Progress(bep::build_event_id::ProgressId { opaque_count: n }))
}
fn started_id() -> bep::BuildEventId {
    id(bep::build_event_id::Id::Started(bep::build_event_id::BuildStartedId {}))
}
fn unstructured_id() -> bep::BuildEventId {
    id(bep::build_event_id::Id::UnstructuredCommandLine(bep::build_event_id::UnstructuredCommandLineId {}))
}
fn structured_id(label: &str) -> bep::BuildEventId {
    id(bep::build_event_id::Id::StructuredCommandLine(bep::build_event_id::StructuredCommandLineId { command_line_label: label.into() }))
}
fn workspace_status_id() -> bep::BuildEventId {
    id(bep::build_event_id::Id::WorkspaceStatus(bep::build_event_id::WorkspaceStatusId {}))
}
fn metadata_id() -> bep::BuildEventId {
    id(bep::build_event_id::Id::BuildMetadata(bep::build_event_id::BuildMetadataId {}))
}
fn finished_id() -> bep::BuildEventId {
    id(bep::build_event_id::Id::BuildFinished(bep::build_event_id::BuildFinishedId {}))
}
fn tool_logs_id() -> bep::BuildEventId {
    id(bep::build_event_id::Id::BuildToolLogs(bep::build_event_id::BuildToolLogsId {}))
}

fn any_bazel(ev: &bep::BuildEvent) -> prost_types::Any {
    prost_types::Any { type_url: "type.googleapis.com/build_event_stream.BuildEvent".into(), value: ev.encode_to_vec() }
}

pub struct BesConfig {
    pub invocation_id: String,
    /// Already redacted (no header values).
    pub command_line: Vec<String>,
    pub command: String,
    pub build_metadata: BTreeMap<String, String>,
    pub workspace_status: BTreeMap<String, String>,
}

#[derive(Debug)]
pub struct BesSummary {
    pub events_sent: usize,
    pub events_acked: usize,
    pub build_id: String,
    pub notes: Vec<String>,
}

pub struct Bes {
    conn: Conn,
    build_id: String,
    invocation_id: String,
    tx: Option<mpsc::Sender<v1::PublishBuildToolEventStreamRequest>>,
    task: Option<JoinHandle<Result<usize>>>,
    seq: i64,
    sent: usize,
    next_progress: i32,
    pending: Option<String>,
    notes: Vec<String>,
    send_error: Option<String>,
}

impl Bes {
    fn controller(&self, with_invocation: bool, seq: i64, ev: v1::build_event::Event) -> v1::PublishLifecycleEventRequest {
        v1::PublishLifecycleEventRequest {
            service_level: v1::publish_lifecycle_event_request::ServiceLevel::Interactive as i32,
            build_event: Some(v1::OrderedBuildEvent {
                stream_id: Some(v1::StreamId {
                    build_id: self.build_id.clone(),
                    invocation_id: if with_invocation { self.invocation_id.clone() } else { String::new() },
                    component: v1::stream_id::BuildComponent::Controller as i32,
                }),
                sequence_number: seq,
                event: Some(v1::BuildEvent { event_time: Some(now_ts()), event: Some(ev) }),
            }),
            stream_timeout: None,
            notification_keywords: vec![],
            project_id: String::new(),
            check_preceding_lifecycle_events_present: false,
        }
    }

    async fn lifecycle(&mut self, with_invocation: bool, seq: i64, ev: v1::build_event::Event) {
        let req = self.controller(with_invocation, seq, ev);
        if let Err(e) = self.conn.bes().publish_lifecycle_event(req).await {
            self.notes.push(format!("lifecycle event seq {seq} not accepted: {:?}: {}", e.code(), e.message()));
        }
    }

    async fn send(&mut self, bev: bep::BuildEvent) {
        self.seq += 1;
        let req = v1::PublishBuildToolEventStreamRequest {
            ordered_build_event: Some(v1::OrderedBuildEvent {
                stream_id: Some(v1::StreamId {
                    build_id: self.build_id.clone(),
                    invocation_id: self.invocation_id.clone(),
                    component: v1::stream_id::BuildComponent::Tool as i32,
                }),
                sequence_number: self.seq,
                event: Some(v1::BuildEvent { event_time: Some(now_ts()), event: Some(v1::build_event::Event::BazelEvent(any_bazel(&bev))) }),
            }),
            notification_keywords: vec![],
            project_id: String::new(),
            check_preceding_lifecycle_events_present: false,
        };
        self.push(req).await;
    }

    async fn push(&mut self, req: v1::PublishBuildToolEventStreamRequest) {
        match &self.tx {
            Some(tx) => {
                if tx.send(req).await.is_ok() {
                    self.sent += 1;
                } else if self.send_error.is_none() {
                    self.send_error = Some("event stream closed by server or transport".into());
                }
            }
            None => {}
        }
    }

    pub async fn start(conn: Conn, cfg: BesConfig) -> Result<Bes> {
        let mut bes = Bes {
            conn,
            build_id: uuid::Uuid::new_v4().to_string(),
            invocation_id: cfg.invocation_id.clone(),
            tx: None,
            task: None,
            seq: 0,
            sent: 0,
            next_progress: 1,
            pending: None,
            notes: vec![],
            send_error: None,
        };

        // Lifecycle: build enqueued, invocation attempt started (best effort; some servers only
        // implement the tool stream).
        bes.lifecycle(false, 1, v1::build_event::Event::BuildEnqueued(v1::build_event::BuildEnqueued { details: None })).await;
        bes.lifecycle(
            true,
            1,
            v1::build_event::Event::InvocationAttemptStarted(v1::build_event::InvocationAttemptStarted { attempt_number: 1, details: None }),
        )
        .await;

        // Tool stream: run the RPC in a task so a server that only sends response headers after
        // the first event cannot deadlock us; the task also drains acknowledgements.
        let (tx, rx) = mpsc::channel::<v1::PublishBuildToolEventStreamRequest>(256);
        let mut client = bes.conn.bes();
        bes.task = Some(tokio::spawn(async move {
            let resp = client
                .publish_build_tool_event_stream(ReceiverStream::new(rx))
                .await
                .map_err(|e| anyhow!("PublishBuildToolEventStream: {:?}: {}", e.code(), e.message()))?;
            let mut st = resp.into_inner();
            let mut acked = 0usize;
            while let Some(_r) = st.message().await.map_err(|e| anyhow!("BES ack stream: {:?}: {}", e.code(), e.message()))? {
                acked += 1;
            }
            Ok(acked)
        }));
        bes.tx = Some(tx);

        let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
        let host = std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| std::env::var("HOSTNAME").unwrap_or_default());
        let user = std::env::var("USER").unwrap_or_default();
        let t = now_ts();

        let started = bep::BuildEvent {
            id: Some(started_id()),
            children: vec![progress_id(0), unstructured_id(), structured_id("original"), workspace_status_id(), metadata_id(), finished_id()],
            last_message: false,
            payload: Some(bep::build_event::Payload::Started(bep::BuildStarted {
                uuid: cfg.invocation_id.clone(),
                start_time_millis: t.seconds * 1000 + (t.nanos as i64) / 1_000_000,
                start_time: Some(t),
                build_tool_version: env!("CARGO_PKG_VERSION").into(),
                options_description: cfg.command_line.join(" "),
                command: cfg.command.clone(),
                working_directory: cwd.clone(),
                workspace_directory: cwd,
                server_pid: std::process::id() as i64,
                host: host.clone(),
                user: user.clone(),
            })),
        };
        bes.send(started).await;

        bes.send(bep::BuildEvent {
            id: Some(unstructured_id()),
            children: vec![],
            last_message: false,
            payload: Some(bep::build_event::Payload::UnstructuredCommandLine(bep::UnstructuredCommandLine { args: cfg.command_line.clone() })),
        })
        .await;

        // Structured command line: executable, command, residual (all already redacted).
        let chunk = |s: Vec<String>| cl::CommandLineSection {
            section_label: String::new(),
            section_type: Some(cl::command_line_section::SectionType::ChunkList(cl::ChunkList { chunk: s })),
        };
        let mut exe = chunk(cfg.command_line.iter().take(1).cloned().collect());
        exe.section_label = "executable".into();
        let mut cmd = chunk(vec![cfg.command.clone()]);
        cmd.section_label = "command".into();
        let mut residual = chunk(cfg.command_line.iter().skip(1).cloned().collect());
        residual.section_label = "residual".into();
        bes.send(bep::BuildEvent {
            id: Some(structured_id("original")),
            children: vec![],
            last_message: false,
            payload: Some(bep::build_event::Payload::StructuredCommandLine(cl::CommandLine {
                command_line_label: "original".into(),
                sections: vec![exe, cmd, residual],
            })),
        })
        .await;

        let mut items = vec![
            bep::workspace_status::Item { key: "BUILD_USER".into(), value: user },
            bep::workspace_status::Item { key: "BUILD_HOST".into(), value: host },
        ];
        for (k, v) in &cfg.workspace_status {
            items.push(bep::workspace_status::Item { key: k.clone(), value: v.clone() });
        }
        bes.send(bep::BuildEvent {
            id: Some(workspace_status_id()),
            children: vec![],
            last_message: false,
            payload: Some(bep::build_event::Payload::WorkspaceStatus(bep::WorkspaceStatus { item: items })),
        })
        .await;

        bes.send(bep::BuildEvent {
            id: Some(metadata_id()),
            children: vec![],
            last_message: false,
            payload: Some(bep::build_event::Payload::BuildMetadata(bep::BuildMetadata { metadata: cfg.build_metadata.clone().into_iter().collect() })),
        })
        .await;

        // The first progress event (opaque_count 0) is emitted lazily, one step behind, so that
        // each progress event can declare its successor as a child.
        bes.pending = Some(String::new());
        Ok(bes)
    }

    async fn emit_progress(&mut self, text: String, has_next: bool) {
        let n = self.next_progress - 1;
        let children = if has_next { vec![progress_id(n + 1)] } else { vec![] };
        self.send(bep::BuildEvent {
            id: Some(progress_id(n)),
            children,
            last_message: false,
            payload: Some(bep::build_event::Payload::Progress(bep::Progress { stdout: String::new(), stderr: text })),
        })
        .await;
        self.next_progress += 1;
    }

    /// Console output for the invocation page. Delayed by one event (see `start`).
    pub async fn progress(&mut self, text: &str) {
        if let Some(prev) = self.pending.take() {
            self.emit_progress(prev, true).await;
        }
        self.pending = Some(format!("{text}\n"));
    }

    pub async fn finish(mut self, success: bool, report_json: &[u8]) -> BesSummary {
        if let Some(prev) = self.pending.take() {
            self.emit_progress(prev, true).await;
        }
        // Final, empty progress event closes the chain.
        self.emit_progress(String::new(), false).await;

        let t = now_ts();
        self.send(bep::BuildEvent {
            id: Some(finished_id()),
            children: vec![tool_logs_id()],
            last_message: false,
            payload: Some(bep::build_event::Payload::Finished(bep::BuildFinished {
                overall_success: success,
                exit_code: Some(bep::build_finished::ExitCode { name: if success { "SUCCESS".into() } else { "FAILURE".into() }, code: if success { 0 } else { 1 } }),
                finish_time_millis: t.seconds * 1000 + (t.nanos as i64) / 1_000_000,
                finish_time: Some(t),
                anomaly_report: None,
                failure_detail: None,
            })),
        })
        .await;
        self.send(bep::BuildEvent {
            id: Some(tool_logs_id()),
            children: vec![],
            last_message: true,
            payload: Some(bep::build_event::Payload::BuildToolLogs(bep::BuildToolLogs {
                log: vec![bep::File {
                    path_prefix: vec![],
                    name: "cargo-rbe-report.json".into(),
                    digest: String::new(),
                    length: report_json.len() as i64,
                    file: Some(bep::file::File::Contents(report_json.to_vec())),
                }],
            })),
        })
        .await;

        // Close the tool stream with ComponentStreamFinished, then half-close and wait for acks.
        self.seq += 1;
        let fin = v1::PublishBuildToolEventStreamRequest {
            ordered_build_event: Some(v1::OrderedBuildEvent {
                stream_id: Some(v1::StreamId { build_id: self.build_id.clone(), invocation_id: self.invocation_id.clone(), component: v1::stream_id::BuildComponent::Tool as i32 }),
                sequence_number: self.seq,
                event: Some(v1::BuildEvent {
                    event_time: Some(now_ts()),
                    event: Some(v1::build_event::Event::ComponentStreamFinished(v1::build_event::BuildComponentStreamFinished {
                        r#type: v1::build_event::build_component_stream_finished::FinishType::Finished as i32,
                    })),
                }),
            }),
            notification_keywords: vec![],
            project_id: String::new(),
            check_preceding_lifecycle_events_present: false,
        };
        self.push(fin).await;
        self.tx = None; // drop sender: half-closes the request stream

        let mut acked = 0usize;
        if let Some(task) = self.task.take() {
            match tokio::time::timeout(Duration::from_secs(60), task).await {
                Ok(Ok(Ok(n))) => acked = n,
                Ok(Ok(Err(e))) => self.notes.push(format!("tool stream error: {e:#}")),
                Ok(Err(e)) => self.notes.push(format!("tool stream task failed: {e}")),
                Err(_) => self.notes.push("timed out waiting for the server to acknowledge the tool stream".into()),
            }
        }
        if let Some(e) = self.send_error.clone() {
            self.notes.push(e);
        }

        let status = |ok: bool| v1::BuildStatus {
            result: if ok { v1::build_status::Result::CommandSucceeded as i32 } else { v1::build_status::Result::CommandFailed as i32 },
            final_invocation_id: String::new(),
            build_tool_exit_code: Some(if ok { 0 } else { 1 }),
            error_message: String::new(),
            details: None,
        };
        self.lifecycle(
            true,
            2,
            v1::build_event::Event::InvocationAttemptFinished(v1::build_event::InvocationAttemptFinished { invocation_status: Some(status(success)), details: None }),
        )
        .await;
        self.lifecycle(false, 2, v1::build_event::Event::BuildFinished(v1::build_event::BuildFinished { status: Some(status(success)), details: None })).await;

        BesSummary { events_sent: self.sent, events_acked: acked, build_id: self.build_id.clone(), notes: self.notes }
    }
}

/// Redact secrets from a command line before it is published: literal header values are
/// replaced; `@env:`/`@file:` references are kept (they are not secret).
pub fn redact_args(args: &[String]) -> Vec<String> {
    fn spec(s: &str) -> String {
        if s.starts_with('@') {
            return s.to_string();
        }
        match s.split_once('=') {
            Some((n, v)) if v.starts_with("@env:") || v.starts_with("@file:") => format!("{n}={v}"),
            Some((n, _)) => format!("{n}=<redacted>"),
            None => "<redacted>".into(),
        }
    }
    const FLAGS: [&str; 2] = ["--remote_header", "--bes_header"];
    let mut out = vec![];
    let mut redact_next = false;
    for a in args {
        if redact_next {
            out.push(spec(a));
            redact_next = false;
            continue;
        }
        if FLAGS.contains(&a.as_str()) {
            out.push(a.clone());
            redact_next = true;
            continue;
        }
        match FLAGS.iter().find_map(|f| a.strip_prefix(&format!("{f}=")).map(|v| (f, v))) {
            Some((f, v)) => out.push(format!("{f}={}", spec(v))),
            None => out.push(a.clone()),
        }
    }
    out
}

/// Workspace-status keys many BES servers understand, taken from the CI environment when present.
pub fn ci_workspace_status() -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    if std::env::var("GITHUB_ACTIONS").ok().as_deref() == Some("true") {
        m.insert("ROLE".into(), "CI".into());
        if let Ok(r) = std::env::var("GITHUB_REPOSITORY") {
            let server = std::env::var("GITHUB_SERVER_URL").unwrap_or_else(|_| "https://github.com".into());
            m.insert("REPO_URL".into(), format!("{server}/{r}"));
        }
        if let Ok(s) = std::env::var("GITHUB_SHA") {
            m.insert("COMMIT_SHA".into(), s);
        }
        let branch = std::env::var("GITHUB_HEAD_REF").ok().filter(|s| !s.is_empty()).or_else(|| std::env::var("GITHUB_REF_NAME").ok());
        if let Some(b) = branch {
            m.insert("BRANCH_NAME".into(), b);
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn redacts_literal_header_values_in_both_flag_forms() {
        let out = redact_args(&v(&["cargo-rbe", "--remote_header=x-key=SECRET", "--bes_header", "x-other=SECRET2", "smoke"]));
        assert_eq!(out, v(&["cargo-rbe", "--remote_header=x-key=<redacted>", "--bes_header", "x-other=<redacted>", "smoke"]));
        assert!(!out.join(" ").contains("SECRET"));
    }

    #[test]
    fn keeps_env_and_file_references() {
        let out = redact_args(&v(&["--remote_header=x-key=@env:RBE_AUTH", "--remote_header=@env:RBE_HEADERS", "--bes_header=@file:/tmp/h"]));
        assert_eq!(out, v(&["--remote_header=x-key=@env:RBE_AUTH", "--remote_header=@env:RBE_HEADERS", "--bes_header=@file:/tmp/h"]));
    }

    #[test]
    fn header_flag_without_equals_is_fully_redacted() {
        assert_eq!(redact_args(&v(&["--remote_header=tokenwithoutname"])), v(&["--remote_header=<redacted>"]));
    }

    #[test]
    fn other_args_pass_through() {
        let a = v(&["cargo-rbe", "--remote_executor=grpcs://x", "--build_metadata=A=B"]);
        assert_eq!(redact_args(&a), a);
    }
}
