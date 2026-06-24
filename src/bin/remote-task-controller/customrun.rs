//! Bridges Tekton `CustomRun` (tekton.dev/v1beta1) objects whose `customRef`
//! points at `gingersociety.org/v1alpha1, Kind=RemoteTask` to the existing
//! RemoteTask CRD + controller.
//!
//! Execution itself happens in a Kubernetes Job (see job.rs) running the
//! `external-executor-runner` image — this reconciler's job is just to
//! translate CustomRun params into a RemoteTask, kick off the Job that will
//! actually run it, and mirror status back up. It does not execute anything
//! itself and holds no HTTP client to the sidekick.

use std::sync::Arc;

use k8s_openapi::api::core::v1::ObjectReference;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::{
    api::{Api, DynamicObject, Patch, PatchParams, PostParams},
    core::ObjectMeta,
    discovery::ApiResource,
    runtime::controller::Action,
    Client, ResourceExt,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use ginger_infra::remote_task::{RemoteTask, RemoteTaskEnvVar, RemoteTaskPhase, RemoteTaskSpec};

use crate::dispatch::{now, resolve_env};
use crate::events::emit_event;
use crate::job::{create_envrc_secret, create_execution_job, create_scripts_configmap};

pub const CUSTOMRUN_GROUP: &str = "tekton.dev";
pub const CUSTOMRUN_VERSION: &str = "v1beta1";
pub const CUSTOMRUN_KIND: &str = "CustomRun";
pub const CUSTOMRUN_PLURAL: &str = "customruns";

const OUR_GROUP: &str = "gingersociety.org";
const OUR_VERSION: &str = "v1alpha1";
const OUR_KIND: &str = "RemoteTask";

const REQUEUE_AFTER_ERROR_SECS: u64 = 15;
const REQUEUE_WHILE_RUNNING_SECS: u64 = 5;

#[derive(Debug, thiserror::Error)]
pub enum CustomRunError {
    #[error("k8s api error: {0}")]
    Kube(#[from] kube::Error),
    #[error("customRun '{0}' is missing spec.customRef")]
    MissingCustomRef(String),
    #[error("failed to parse params into RemoteTaskSpec: {0}")]
    ParamParse(String),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("dispatch error: {0}")]
    Dispatch(#[from] crate::dispatch::DispatchError),
    #[error("job error: {0}")]
    Job(#[from] crate::job::JobError),
}

pub struct CustomRunContext {
    pub client: Client,
}

#[derive(Debug, Deserialize)]
struct CustomRunSpec {
    #[serde(rename = "customRef")]
    custom_ref: Option<CustomRef>,
    #[serde(default)]
    params: Vec<Param>,
}

#[derive(Debug, Deserialize)]
struct CustomRef {
    #[serde(rename = "apiVersion")]
    api_version: Option<String>,
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Param {
    name: String,
    value: ParamValue,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ParamValue {
    String(String),
    Array(Vec<String>),
}

impl ParamValue {
    fn as_str(&self) -> Option<&str> {
        match self {
            ParamValue::String(s) => Some(s),
            ParamValue::Array(_) => None,
        }
    }
}

#[derive(Debug, Serialize)]
struct CustomRunCondition {
    #[serde(rename = "type")]
    type_: String,
    status: String,
    reason: String,
    message: String,
    #[serde(rename = "lastTransitionTime", skip_serializing_if = "Option::is_none")]
    last_transition_time: Option<Time>,
}

#[derive(Debug, Serialize)]
struct CustomRunResult {
    name: String,
    value: String,
}

// ---- param parsing -------------------------------------------------------

fn spec_from_params(params: &[Param]) -> Result<RemoteTaskSpec, CustomRunError> {
    let mut capability: Option<String> = None;
    let mut script: Option<String> = None;
    let mut cleanup: Option<String> = None;
    let mut env: Vec<RemoteTaskEnvVar> = Vec::new();

    for p in params {
        match p.name.as_str() {
            "capability" => capability = p.value.as_str().map(str::to_string).or(capability),
            "script" => script = p.value.as_str().map(str::to_string).or(script),
            "cleanup" => cleanup = p.value.as_str().map(str::to_string).or(cleanup),
            "env" => {
                if let Some(raw) = p.value.as_str() {
                    let parsed: Vec<RemoteTaskEnvVar> = serde_yaml::from_str(raw)
                        .map_err(|e| CustomRunError::ParamParse(format!("env: {e}")))?;
                    env = parsed;
                }
            }
            other => {
                eprintln!("[customrun-controller] ignoring unrecognized param '{other}'");
            }
        }
    }

    let capability = capability
        .ok_or_else(|| CustomRunError::ParamParse("missing required param 'capability'".into()))?;
    let script = script
        .ok_or_else(|| CustomRunError::ParamParse("missing required param 'script'".into()))?;

    Ok(RemoteTaskSpec {
        capability,
        env,
        script,
        cleanup,
    })
}

// ---- reconcile -----------------------------------------------------------

pub async fn reconcile_customrun(
    run: Arc<DynamicObject>,
    ctx: Arc<CustomRunContext>,
) -> Result<Action, CustomRunError> {
    let name = run.name_any();
    let ns = run
        .metadata
        .namespace
        .clone()
        .unwrap_or_else(|| "default".to_string());

    let spec_value = run
        .data
        .get("spec")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    let spec: CustomRunSpec = serde_json::from_value(spec_value)?;

    let custom_ref = spec
        .custom_ref
        .ok_or_else(|| CustomRunError::MissingCustomRef(name.clone()))?;

    let targets_us =
        custom_ref.api_version.as_deref() == Some(&format!("{OUR_GROUP}/{OUR_VERSION}"))
            && custom_ref.kind.as_deref() == Some(OUR_KIND);

    if !targets_us {
        return Ok(Action::await_change());
    }

    let already_terminal = run
        .data
        .get("status")
        .and_then(|s| s.get("conditions"))
        .and_then(|c| c.as_array())
        .map(|conds| {
            conds.iter().any(|c| {
                c.get("type").and_then(|t| t.as_str()) == Some("Succeeded")
                    && c.get("status").and_then(|s| s.as_str()) != Some("Unknown")
            })
        })
        .unwrap_or(false);

    if already_terminal {
        return Ok(Action::await_change());
    }

    let remote_tasks: Api<RemoteTask> = Api::namespaced(ctx.client.clone(), &ns);
    let customruns = customrun_api(&ctx.client, &ns);

    match remote_tasks.get_opt(&name).await? {
        Some(existing) => {
            // Status sync only: the RemoteTask reconciler is the one that
            // watches the execution Job and updates RemoteTask.status. We
            // just reflect that onto the CustomRun.
            sync_status_from_remote_task(&customruns, &name, &existing).await?;
        }
        None => {
            // ── 1. parse params ───────────────────────────────────────────────
            let task_spec = match spec_from_params(&spec.params) {
                Ok(s) => s,
                Err(e) => {
                    set_customrun_failed(
                        &customruns,
                        &name,
                        &format!("invalid params: {e}"),
                    )
                    .await?;
                    return Ok(Action::await_change());
                }
            };

            // ── 2. create RemoteTask ──────────────────────────────────────────
            let task = build_owned_remote_task(&run, &name, &ns, task_spec.clone());
            let created = remote_tasks.create(&PostParams::default(), &task).await?;
            let task_uid = created.uid().unwrap_or_default();

            println!("[customrun-controller] created RemoteTask {ns}/{name} uid={task_uid}");

            // ── 3. resolve env + write Secret/ConfigMap, then create the Job ───
            //
            // This is the only "work" the CustomRun reconciler does — it does
            // not execute the script itself. The Job it creates here, running
            // the external-executor-runner image, is what actually calls out
            // to the sidekick. The RemoteTask reconciler picks up the Job's
            // status independently.
            if let Err(e) = dispatch_via_job(&ctx.client, &ns, &task_uid, &created).await {
                eprintln!("[customrun-controller] failed to dispatch {ns}/{name}: {e}");
                set_customrun_failed(&customruns, &name, &format!("dispatch failed: {e}"))
                    .await?;
                return Ok(Action::await_change());
            }

            // ── 4. emit a single lifecycle event — not a per-log-line stream ──
            emit_event(
                &ctx.client,
                &ns,
                &object_ref_for(&run),
                "Normal",
                "RemoteTaskCreated",
                &format!("Created RemoteTask {name} and started execution Job"),
            )
            .await?;

            // ── 5. mark CustomRun as Running ────────────────────────────────────
            set_customrun_running(&customruns, &name).await?;
        }
    }

    Ok(Action::requeue(std::time::Duration::from_secs(
        REQUEUE_WHILE_RUNNING_SECS,
    )))
}

/// Resolve env vars and create the Secret + ConfigMap + Job that will
/// execute this RemoteTask. Pure setup — no HTTP calls to the sidekick
/// happen here or anywhere else in the controller process.
async fn dispatch_via_job(
    client: &Client,
    ns: &str,
    task_uid: &str,
    task: &RemoteTask,
) -> Result<(), CustomRunError> {
    let name = task.name_any();

    let env_map = resolve_env(client, ns, &task.spec.env).await?;

    let secret_name = create_envrc_secret(client, ns, &name, task_uid, &env_map).await?;

    let configmap_name = create_scripts_configmap(
        client,
        ns,
        &name,
        task_uid,
        &task.spec.script,
        task.spec.cleanup.as_deref(),
    )
    .await?;

    create_execution_job(
        client,
        ns,
        task,
        task_uid,
        &configmap_name,
        &secret_name,
        task.spec.cleanup.is_some(),
    )
    .await?;

    Ok(())
}

pub fn customrun_error_policy(
    _run: Arc<DynamicObject>,
    err: &CustomRunError,
    _ctx: Arc<CustomRunContext>,
) -> Action {
    eprintln!("[customrun-controller] error_policy: {:?}", err);
    Action::requeue(std::time::Duration::from_secs(REQUEUE_AFTER_ERROR_SECS))
}

// ---- status syncing ------------------------------------------------------

async fn sync_status_from_remote_task(
    customruns: &Api<DynamicObject>,
    name: &str,
    task: &RemoteTask,
) -> Result<(), CustomRunError> {
    let phase = task
        .status
        .as_ref()
        .map(|s| s.phase.clone())
        .unwrap_or_default();

    match phase {
        RemoteTaskPhase::Pending | RemoteTaskPhase::Running => {
            set_customrun_running(customruns, name).await?;
        }
        RemoteTaskPhase::Succeeded => {
            let exit_code = task
                .status
                .as_ref()
                .and_then(|s| s.exit_code)
                .unwrap_or(0);
            set_customrun_succeeded(customruns, name, exit_code).await?;
        }
        RemoteTaskPhase::Failed => {
            let msg = task
                .status
                .as_ref()
                .and_then(|s| s.message.clone())
                .unwrap_or_else(|| "RemoteTask failed".to_string());
            set_customrun_failed(customruns, name, &msg).await?;
        }
    }

    Ok(())
}

async fn set_customrun_running(
    customruns: &Api<DynamicObject>,
    name: &str,
) -> Result<(), CustomRunError> {
    patch_status(
        customruns,
        name,
        vec![CustomRunCondition {
            type_: "Succeeded".to_string(),
            status: "Unknown".to_string(),
            reason: "Running".to_string(),
            message: "RemoteTask is running".to_string(),
            last_transition_time: Some(now()),
        }],
        None,
        false,
    )
    .await
}

async fn set_customrun_succeeded(
    customruns: &Api<DynamicObject>,
    name: &str,
    exit_code: i32,
) -> Result<(), CustomRunError> {
    patch_status(
        customruns,
        name,
        vec![CustomRunCondition {
            type_: "Succeeded".to_string(),
            status: "True".to_string(),
            reason: "RemoteTaskSucceeded".to_string(),
            message: format!("script completed (exit {exit_code})"),
            last_transition_time: Some(now()),
        }],
        Some(vec![CustomRunResult {
            name: "exitCode".to_string(),
            value: exit_code.to_string(),
        }]),
        true,
    )
    .await
}

async fn set_customrun_failed(
    customruns: &Api<DynamicObject>,
    name: &str,
    message: &str,
) -> Result<(), CustomRunError> {
    patch_status(
        customruns,
        name,
        vec![CustomRunCondition {
            type_: "Succeeded".to_string(),
            status: "False".to_string(),
            reason: "RemoteTaskFailed".to_string(),
            message: message.to_string(),
            last_transition_time: Some(now()),
        }],
        None,
        true,
    )
    .await
}

async fn patch_status(
    customruns: &Api<DynamicObject>,
    name: &str,
    conditions: Vec<CustomRunCondition>,
    results: Option<Vec<CustomRunResult>>,
    is_terminal: bool,
) -> Result<(), CustomRunError> {
    let mut status = json!({ "conditions": conditions });
    if let Some(results) = results {
        status["results"] = json!(results);
    }
    if is_terminal {
        status["completionTime"] = json!(rfc3339_now());
    }
    let patch = json!({ "status": status });
    customruns
        .patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

// ---- helpers -------------------------------------------------------------

fn customrun_api(client: &Client, ns: &str) -> Api<DynamicObject> {
    let ar = ApiResource {
        group: CUSTOMRUN_GROUP.to_string(),
        version: CUSTOMRUN_VERSION.to_string(),
        api_version: format!("{CUSTOMRUN_GROUP}/{CUSTOMRUN_VERSION}"),
        kind: CUSTOMRUN_KIND.to_string(),
        plural: CUSTOMRUN_PLURAL.to_string(),
    };
    Api::namespaced_with(client.clone(), ns, &ar)
}

fn build_owned_remote_task(
    run: &DynamicObject,
    name: &str,
    ns: &str,
    spec: RemoteTaskSpec,
) -> RemoteTask {
    let owner = k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
        api_version: format!("{CUSTOMRUN_GROUP}/{CUSTOMRUN_VERSION}"),
        kind: CUSTOMRUN_KIND.to_string(),
        name: run.name_any(),
        uid: run.uid().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    };

    RemoteTask {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(ns.to_string()),
            owner_references: Some(vec![owner]),
            ..Default::default()
        },
        spec,
        status: None,
    }
}

fn object_ref_for(run: &DynamicObject) -> ObjectReference {
    ObjectReference {
        api_version: Some(format!("{CUSTOMRUN_GROUP}/{CUSTOMRUN_VERSION}")),
        kind: Some(CUSTOMRUN_KIND.to_string()),
        name: Some(run.name_any()),
        namespace: run.metadata.namespace.clone(),
        uid: run.uid(),
        ..Default::default()
    }
}

fn rfc3339_now() -> String {
    k8s_openapi::jiff::Timestamp::now()
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}