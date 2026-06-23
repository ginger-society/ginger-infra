//! Bridges Tekton `CustomRun` (tekton.dev/v1beta1) objects whose `customRef`
//! points at `gingersociety.org/v1alpha1, Kind=RemoteTask` to the existing
//! RemoteTask CRD + controller, with no pod involved on the Tekton side.
//!
//! Flow:
//!   1. A PipelineRun's PipelineTask sets `taskRef.apiVersion` + `taskRef.kind`
//!      (instead of `taskRef.name`), so Tekton creates a `CustomRun` instead
//!      of a `TaskRun`/pod.
//!   2. We reconcile that CustomRun here: parse its `spec.params` into a
//!      `RemoteTaskSpec`, then create a `RemoteTask` (owned by the CustomRun)
//!      with that spec — same struct the existing reconciler already knows
//!      how to dispatch to sidekick.
//!   3. While the owned RemoteTask is Pending/Running, we keep CustomRun's
//!      `Succeeded` condition at `status: "Unknown"` — Tekton's signal to
//!      keep watching rather than treat the step as finished. Each `log`
//!      event the `stream_job` loop in main.rs sees is also emitted as a
//!      Kubernetes `Event` on the RemoteTask (via `emit_event`, see
//!      events.rs), which `tkn pr logs` / the dashboard pick up even with
//!      no pod.
//!   4. On RemoteTask terminal phase, we patch CustomRun.status to
//!      True/False and copy the exit code into `status.results`.
//!
//! IMPORTANT — integration notes:
//!   - There is no published Rust crate with native Tekton `CustomRun` types,
//!     so this file defines minimal wire structs (`CustomRunSpec`, `Param`,
//!     etc.) for just the fields we read/write, and drives the actual object
//!     through `kube::api::DynamicObject` + `kube::discovery::ApiResource`.
//!   - This file assumes `kube = "3.1.0"` / `k8s-openapi = "0.27.0"` (the
//!     jiff-based `Time`), matching the existing `remote-task-controller`
//!     binary's use of `k8s_openapi::jiff::Timestamp`. NOT compiled against
//!     the real workspace — see the integration notes in chat before building.

use std::sync::Arc;

use k8s_openapi::api::core::v1::ObjectReference;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{OwnerReference, Time};
use kube::{
    api::{Api, DynamicObject, Patch, PatchParams, PostParams},
    core::ObjectMeta,
    discovery::ApiResource,
    runtime::controller::Action,
    Client, Resource, ResourceExt,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use ginger_infra::remote_task::{RemoteTask, RemoteTaskEnvVar, RemoteTaskPhase, RemoteTaskSpec};

use crate::events::emit_event;

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
}

pub struct CustomRunContext {
    pub client: Client,
}

// ---- minimal CustomRun wire types ------------------------------------------
//
// We only need spec.customRef, spec.params, and the ability to write
// status.conditions / status.results, so plain serde structs read off a
// DynamicObject's `data` field are enough — no need for a full Tekton types
// crate (none exists for Rust).

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

/// Tekton's ParamValue can be a bare string or an array of strings.
/// We only ever produce/consume strings here (the `env` param is a YAML
/// blob string we parse ourselves), so Array just falls back to None.
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
    status: String, // "True" | "False" | "Unknown"
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

// ---- param parsing ----------------------------------------------------

/// Pulls `capability`, `script`, `cleanup`, and `env` params out of a
/// CustomRun and builds the same `RemoteTaskSpec` a developer would have
/// written directly under a RemoteTask's `spec:`.
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
                    // YAML blob string, e.g.:
                    //   - name: TEST_USER
                    //     value: "ginger-tester"
                    //   - name: TOKEN
                    //     valueFrom:
                    //       secretKeyRef: { name: my-secret, key: token }
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

// ---- reconcile --------------------------------------------------------

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

    let targets_us = custom_ref.api_version.as_deref() == Some(&format!("{OUR_GROUP}/{OUR_VERSION}"))
        && custom_ref.kind.as_deref() == Some(OUR_KIND);

    if !targets_us {
        // Not for us — some other custom task controller owns this CustomRun.
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

    // The owned RemoteTask shares the CustomRun's name 1:1 — simplest
    // possible mapping; avoids extra bookkeeping to find "our" RemoteTask.
    match remote_tasks.get_opt(&name).await? {
        Some(existing) => {
            sync_status_from_remote_task(&customruns, &name, &existing).await?;
        }
        None => {
            let task_spec = match spec_from_params(&spec.params) {
                Ok(s) => s,
                Err(e) => {
                    set_customrun_failed(&customruns, &name, &format!("invalid params: {e}"))
                        .await?;
                    return Ok(Action::await_change());
                }
            };

            let task = build_owned_remote_task(&run, &name, &ns, task_spec);

            remote_tasks.create(&PostParams::default(), &task).await?;

            emit_event(
                &ctx.client,
                &ns,
                &object_ref_for(&run),
                "Normal",
                "RemoteTaskCreated",
                &format!("Created RemoteTask {name} from CustomRun params"),
            )
            .await?;

            set_customrun_running(&customruns, &name).await?;
        }
    }

    Ok(Action::requeue(std::time::Duration::from_secs(
        REQUEUE_WHILE_RUNNING_SECS,
    )))
}

pub fn customrun_error_policy(
    _run: Arc<DynamicObject>,
    err: &CustomRunError,
    _ctx: Arc<CustomRunContext>,
) -> Action {
    eprintln!("[customrun-controller] error_policy: {:?}", err);
    Action::requeue(std::time::Duration::from_secs(REQUEUE_AFTER_ERROR_SECS))
}

// ---- status syncing -----------------------------------------------------

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
            let exit_code = task.status.as_ref().and_then(|s| s.exit_code).unwrap_or(0);
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
    )
    .await
}

async fn patch_status(
    customruns: &Api<DynamicObject>,
    name: &str,
    conditions: Vec<CustomRunCondition>,
    results: Option<Vec<CustomRunResult>>,
) -> Result<(), CustomRunError> {
    let mut status = json!({ "conditions": conditions });
    if let Some(results) = results {
        status["results"] = json!(results);
    }
    let patch = json!({ "status": status });
    customruns
        .patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

// ---- helpers --------------------------------------------------------------

/// Builds the `ApiResource` + `Api<DynamicObject>` for `tekton.dev/v1beta1
/// CustomRun` without doing a live discovery round-trip — we already know
/// the exact group/version/kind/plural, so this is a static construction.
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
    let owner = OwnerReference {
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

fn now() -> Time {
    Time(k8s_openapi::jiff::Timestamp::now())
}