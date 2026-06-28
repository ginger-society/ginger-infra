//! RemoteTask reconciler.
//!
//! Watches standalone RemoteTask CRD objects (applied directly with kubectl,
//! not via a pipeline) and creates a Tekton TaskRun for each one.
//!
//! This is the path used when a developer applies a RemoteTask directly:
//!
//!   kubectl apply -f my-task.yaml
//!
//! For the pipeline path (taskRef: kind: RemoteTask) see customrun.rs.
//!
//! ## Credentials
//!
//! Standalone RemoteTasks are not part of a pipeline and therefore have no
//! shared workspace written by init-credentials. The runner image is expected
//! to find auth.json at its default location (~/.ginger-society/auth.json) on
//! the node — useful for dev/test scenarios. Pass `creds_workspace_claim: None`
//! to taskrun.rs to skip the workspace volume entirely.

use std::sync::Arc;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::{
    api::{Api, Patch, PatchParams},
    runtime::controller::Action,
    Client, ResourceExt,
};
use serde_json::json;

use ginger_infra::remote_task::{RemoteTask, RemoteTaskEnvVar, RemoteTaskPhase, RemoteTaskStatus};

use crate::taskrun::{create_taskrun, taskrun_api, TaskRunSpec};

const REQUEUE_AFTER_ERROR_SECS: u64 = 30;

// ── context ───────────────────────────────────────────────────────────────────

pub struct RemoteTaskContext {
    pub client: Client,
    pub sidekick_url: String,
}

// ── errors ────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum RemoteTaskError {
    #[error("kube: {0}")]
    Kube(#[from] kube::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("taskrun: {0}")]
    TaskRun(#[from] crate::taskrun::TaskRunError),
}

// ── reconciler ────────────────────────────────────────────────────────────────

pub async fn reconcile_remote_task(
    task: Arc<RemoteTask>,
    ctx: Arc<RemoteTaskContext>,
) -> Result<Action, RemoteTaskError> {
    let name = task.name_any();
    let ns = task.namespace().unwrap_or_else(|| "default".into());

    let current_phase = task
        .status
        .as_ref()
        .map(|s| s.phase.clone())
        .unwrap_or_default();

    // Already handed off to Tekton — nothing more to do.
    if matches!(
        current_phase,
        RemoteTaskPhase::Running | RemoteTaskPhase::Succeeded | RemoteTaskPhase::Failed
    ) {
        return Ok(Action::await_change());
    }

    // Idempotent: if TaskRun already exists just mark running and stop.
    if taskrun_api(&ctx.client, &ns)
        .get_opt(&name)
        .await?
        .is_some()
    {
        mark_running(&ctx.client, &ns, &name).await?;
        return Ok(Action::await_change());
    }

    println!("[remotetask] {ns}/{name} — creating TaskRun");

    let env = build_env(&task.spec.env);
    let owner_uid = task.uid().unwrap_or_default();
    let taskrun_spec = TaskRunSpec {
        name: &name,
        ns: &ns,
        owner_api_version: "gingersociety.org/v1alpha1",
        owner_kind: "RemoteTask",
        owner_name: &name,
        owner_uid: owner_uid.as_str(),
        capability: &task.spec.capability,
        script: &task.spec.script,
        cleanup: task.spec.cleanup.as_deref(),
        env,
        sidekick_url: &ctx.sidekick_url,
        extra_labels: std::collections::BTreeMap::new(),
        // Standalone RemoteTasks have no pipeline workspace — the runner
        // will use whatever credentials already exist on the node.
        creds_workspace_claim: None,
    };

    create_taskrun(&ctx.client, taskrun_spec).await?;
    mark_running(&ctx.client, &ns, &name).await?;

    Ok(Action::await_change())
}

pub fn error_policy(
    _task: Arc<RemoteTask>,
    err: &RemoteTaskError,
    _ctx: Arc<RemoteTaskContext>,
) -> Action {
    eprintln!("[remotetask] error_policy: {:?}", err);
    Action::requeue(std::time::Duration::from_secs(REQUEUE_AFTER_ERROR_SECS))
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Convert RemoteTaskEnvVar entries into JSON values ready for the TaskRun
/// step spec. secretKeyRef entries are passed through verbatim so Kubernetes
/// injects the value at pod start — the controller never reads secret values.
pub fn build_env(env_specs: &[RemoteTaskEnvVar]) -> Vec<serde_json::Value> {
    env_specs
        .iter()
        .map(|e| {
            if let Some(v) = &e.value {
                json!({ "name": e.name, "value": v })
            } else if let Some(from) = &e.value_from {
                if let Some(sr) = &from.secret_key_ref {
                    json!({
                        "name": e.name,
                        "valueFrom": {
                            "secretKeyRef": { "name": sr.name, "key": sr.key }
                        }
                    })
                } else {
                    json!({ "name": e.name, "value": "" })
                }
            } else {
                json!({ "name": e.name, "value": "" })
            }
        })
        .collect()
}

async fn mark_running(
    client: &Client,
    ns: &str,
    name: &str,
) -> Result<(), RemoteTaskError> {
    let api: Api<RemoteTask> = Api::namespaced(client.clone(), ns);
    let status = RemoteTaskStatus {
        phase: RemoteTaskPhase::Running,
        start_time: Some(Time(k8s_openapi::jiff::Timestamp::now())),
        ..Default::default()
    };
    let patch = json!({ "status": status });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}