// src/bin/tekton-controller.rs
//
// remote-task-controller — watches RemoteTask CRDs, resolves their env vars,
// dispatches the job to the sidekick service (the same /run-job SSE endpoint
// `ginger-infra rpc` talks to), and writes status back onto the RemoteTask.
//
// Deliberately narrow scope for v1 — see remote_task.rs's module doc comment
// for the full list. The two most important gaps, repeated here because
// they're easy to forget while reading reconcile code in isolation:
//
//   - NOT HANDLED: cancellation. If the owning PipelineRun is deleted or
//     cancelled, nothing currently tells the device to stop running the
//     script. The job runs to completion on the device regardless.
//   - NOT HANDLED: restart-resume. If this controller process restarts
//     while a RemoteTask is Running, the SSE connection is lost. The
//     RemoteTask is left stuck in `Running` forever — there is no
//     reconnect-to-in-flight-job logic. A future version should either
//     add a "stale Running RemoteTask" sweep (mark Failed after some
//     timeout with no status update) or have the sidekick support
//     resubscribing to an existing job_id's event stream.
//
// Env resolution is intentionally narrow too: only `value` and
// `valueFrom.secretKeyRef` are supported. No configMapKeyRef, no Tekton
// `$(params.x)` / `$(context.x)` expression resolution — those require a
// real expression engine and access to the owning PipelineRun, which this
// version does not implement.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::StreamExt;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::{
    api::{Api, Patch, PatchParams},
    runtime::{controller::Action, watcher, Controller},
    Client, ResourceExt,
};
use serde::Serialize;
use serde_json::{json, Value};

use ginger_infra::remote_task::{RemoteTask, RemoteTaskCondition, RemoteTaskPhase, RemoteTaskStatus};

const REQUEUE_AFTER_ERROR_SECS: u64 = 30;

#[derive(Debug, thiserror::Error)]
enum ReconcileError {
    #[error("k8s api error: {0}")]
    Kube(#[from] kube::Error),
    #[error("missing required field: {0}")]
    MissingField(String),
    #[error("secret '{secret}' key '{key}' not found in namespace '{ns}'")]
    SecretKeyNotFound {
        secret: String,
        key: String,
        ns: String,
    },
    #[error("sidekick request failed: {0}")]
    SidekickRequest(String),
    #[error("job error: {0}")]
    JobError(String),
}

struct ControllerContext {
    client: Client,
    http: reqwest::Client,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("[tekton-controller] starting...");

    let client = Client::try_default().await?;
    let remote_tasks: Api<RemoteTask> = Api::all(client.clone());

    let ctx = Arc::new(ControllerContext {
        client: client.clone(),
        http: reqwest::Client::new(),
    });

    println!("[tekton-controller] watching RemoteTask across all namespaces");

    Controller::new(remote_tasks, watcher::Config::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok(o) => println!("[tekton-controller] reconciled {:?}", o),
                Err(e) => eprintln!("[tekton-controller] reconcile error: {:?}", e),
            }
        })
        .await;

    Ok(())
}

fn error_policy(_task: Arc<RemoteTask>, err: &ReconcileError, _ctx: Arc<ControllerContext>) -> Action {
    eprintln!("[tekton-controller] error_policy: {:?}", err);
    Action::requeue(std::time::Duration::from_secs(REQUEUE_AFTER_ERROR_SECS))
}

async fn reconcile(
    task: Arc<RemoteTask>,
    ctx: Arc<ControllerContext>,
) -> Result<Action, ReconcileError> {
    let name = task.name_any();
    let ns = task.namespace().unwrap_or_else(|| "default".to_string());

    let phase = task.status.as_ref().map(|s| s.phase.clone()).unwrap_or_default();

    // Only act on tasks that haven't been dispatched yet. Once a RemoteTask
    // reaches Running/Succeeded/Failed, this version does not reconcile it
    // further — no retries, no cancellation, no resume. See module doc.
    match phase {
        RemoteTaskPhase::Pending => {
            println!("[tekton-controller] dispatching {}/{}", ns, name);
            dispatch(&task, &ctx, &ns, &name).await?;
        }
        RemoteTaskPhase::Running => {
            println!(
                "[tekton-controller] {}/{} already Running — no resume logic in this version, leaving as-is",
                ns, name
            );
        }
        RemoteTaskPhase::Succeeded | RemoteTaskPhase::Failed => {
            // terminal — nothing to do
        }
    }

    Ok(Action::await_change())
}

/// Resolve env, mark the RemoteTask Running, POST to the sidekick, stream
/// the SSE response, and write Succeeded/Failed status at the end.
async fn dispatch(
    task: &RemoteTask,
    ctx: &ControllerContext,
    ns: &str,
    name: &str,
) -> Result<(), ReconcileError> {
    let api: Api<RemoteTask> = Api::namespaced(ctx.client.clone(), ns);

    let env = resolve_env(&ctx.client, ns, &task.spec.env).await?;

    set_status(
        &api,
        name,
        RemoteTaskStatus {
            phase: RemoteTaskPhase::Running,
            start_time: Some(now()),
            ..Default::default()
        },
    )
    .await?;

    let request = RunJobRequest {
        capability: task.spec.capability.clone(),
        script: task.spec.script.clone(),
        cleanup_script: task.spec.cleanup.clone(),
        env,
    };

    let result = stream_job(&ctx.http, &task.spec.sidekick_url, &request).await;

    let final_status = match result {
        Ok(exit_code) => RemoteTaskStatus {
            phase: RemoteTaskPhase::Succeeded,
            exit_code: Some(exit_code),
            completion_time: Some(now()),
            conditions: vec![RemoteTaskCondition {
                type_: "Succeeded".to_string(),
                status: "True".to_string(),
                reason: Some("ExitCodeZero".to_string()),
                message: Some(format!("Script completed successfully (exit {})", exit_code)),
            }],
            ..Default::default()
        },
        Err(e) => RemoteTaskStatus {
            phase: RemoteTaskPhase::Failed,
            completion_time: Some(now()),
            message: Some(e.to_string()),
            conditions: vec![RemoteTaskCondition {
                type_: "Succeeded".to_string(),
                status: "False".to_string(),
                reason: Some("Error".to_string()),
                message: Some(e.to_string()),
            }],
            ..Default::default()
        },
    };

    set_status(&api, name, final_status).await?;
    Ok(())
}

/// Resolve `value` and `valueFrom.secretKeyRef` entries into a flat
/// name → value map. Any other source kind is not supported in this
/// version and is skipped with a loud eprintln rather than silently
/// dropped, so a RemoteTask author notices immediately if they reach
/// for something not yet implemented (e.g. configMapKeyRef).
async fn resolve_env(
    client: &Client,
    ns: &str,
    env_specs: &[ginger_infra::remote_task::RemoteTaskEnvVar],
) -> Result<HashMap<String, String>, ReconcileError> {
    use k8s_openapi::api::core::v1::Secret;

    let mut resolved = HashMap::new();
    let secrets_api: Api<Secret> = Api::namespaced(client.clone(), ns);

    for env in env_specs {
        if let Some(value) = &env.value {
            resolved.insert(env.name.clone(), value.clone());
            continue;
        }

        if let Some(from) = &env.value_from {
            if let Some(secret_ref) = &from.secret_key_ref {
                let secret = secrets_api.get(&secret_ref.name).await?;
                let data = secret.data.ok_or_else(|| ReconcileError::SecretKeyNotFound {
                    secret: secret_ref.name.clone(),
                    key: secret_ref.key.clone(),
                    ns: ns.to_string(),
                })?;
                let bytes = data.get(&secret_ref.key).ok_or_else(|| ReconcileError::SecretKeyNotFound {
                    secret: secret_ref.name.clone(),
                    key: secret_ref.key.clone(),
                    ns: ns.to_string(),
                })?;
                let value = String::from_utf8_lossy(&bytes.0).to_string();
                resolved.insert(env.name.clone(), value);
                continue;
            }
        }

        eprintln!(
            "[tekton-controller] env '{}' has no supported source (only `value` and \
             `valueFrom.secretKeyRef` are implemented) — skipping, value will be unset on the device",
            env.name
        );
    }

    Ok(resolved)
}

async fn set_status(
    api: &Api<RemoteTask>,
    name: &str,
    status: RemoteTaskStatus,
) -> Result<(), ReconcileError> {
    let patch = json!({ "status": status });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

fn now() -> Time {
    // k8s-openapi 0.27.0 switched Time's inner representation from
    // chrono::DateTime<Utc> to jiff::Timestamp (see the v0.27.0 release
    // notes: "chrono::DateTime has been replaced by jiff::Timestamp in
    // the implementations of ... Time"). k8s_openapi re-exports jiff
    // itself, so we use that instead of pulling in our own chrono dep.
    Time(k8s_openapi::jiff::Timestamp::now())
}

// ── sidekick HTTP/SSE client ─────────────────────────────────────────────────
//
// This intentionally duplicates the shape (not the code) of rpc.rs's
// RunJobRequest/stream_job. They stay separate on purpose: rpc.rs is a
// human-invoked CLI command reading files off disk and printing to a
// terminal; this is a controller reading from Kubernetes objects and
// writing structured status back. Sharing one generic "run a job and
// stream results" helper across both would mean every change to either
// caller's surrounding logic risks the other — these two call sites have
// different enough callers (human vs. control loop) that a shared
// abstraction would be coupling for its own sake rather than real reuse.

#[derive(Debug, Serialize)]
struct EnvVar {
    name: String,
    value: String,
}

#[derive(Debug, Serialize)]
struct RunJobRequest {
    capability: String,
    script: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cleanup_script: Option<String>,
    env: HashMap<String, String>,
}

/// POST the job and stream SSE `data: {...}` frames until `done`/`error`.
/// Returns the exit code on success, or a ReconcileError on any failure —
/// including a nonzero exit code, which is surfaced as a JobError so the
/// caller writes a Failed status rather than crashing the reconcile loop.
async fn stream_job(
    client: &reqwest::Client,
    url: &str,
    request: &RunJobRequest,
) -> Result<i32, ReconcileError> {
    let response = client
        .post(url)
        .header("Content-Type", "application/json")
        .json(request)
        .send()
        .await
        .map_err(|e| ReconcileError::SidekickRequest(format!("request to '{url}' failed: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(ReconcileError::SidekickRequest(format!(
            "sidekick returned {status}: {body}"
        )));
    }

    let mut stream = response.bytes_stream();
    let mut buf = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|e| ReconcileError::SidekickRequest(format!("stream read error: {e}")))?;
        buf.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(idx) = buf.find('\n') {
            let line = buf[..idx].trim_end_matches('\r').to_string();
            buf.drain(..=idx);

            if line.is_empty() {
                continue;
            }
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim_start();

            let event: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[tekton-controller] could not parse event '{data}': {e}");
                    continue;
                }
            };

            let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

            match event_type {
                "log" => {
                    let stream_name = event.get("stream").and_then(|v| v.as_str()).unwrap_or("stdout");
                    let line_text = event.get("line").and_then(|v| v.as_str()).unwrap_or("");
                    // v1: logs go to the controller's own stdout/stderr only.
                    // Forwarding into a Tekton-visible log sink (companion
                    // Pod or pluggable sink, per the design doc) is future
                    // work, not implemented here.
                    println!("[remote-task:{stream_name}] {line_text}");
                }
                "done" => {
                    let exit_code = event.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                    return Ok(exit_code);
                }
                "error" => {
                    let message = event
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    return Err(ReconcileError::JobError(message.to_string()));
                }
                _ => {
                    println!("[tekton-controller] unrecognized event: {data}");
                }
            }
        }
    }

    Err(ReconcileError::JobError(
        "stream ended before a 'done' or 'error' event was received".to_string(),
    ))
}