//! CustomRun reconciler.
//!
//! Tekton creates a CustomRun when a Pipeline task uses:
//!
//!   taskRef:
//!     apiVersion: gingersociety.org/v1alpha1
//!     kind: RemoteTask
//!
//! This reconciler reads the CustomRun's params (capability, script, cleanup,
//! env) and creates a Tekton TaskRun that runs the runner image. Status is
//! then owned by Tekton — we just need to mark the CustomRun as Running once
//! the TaskRun exists, and Succeeded/Failed when we can observe the TaskRun's
//! own completion (or let Tekton's own signals propagate via the ownerRef).
//!
//! Compared to the old implementation: no Job, no Secret, no ConfigMap,
//! no SSE streaming, no log mirroring.

use std::sync::Arc;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::{
    api::{Api, DynamicObject, Patch, PatchParams},
    discovery::ApiResource,
    runtime::controller::Action,
    Client, ResourceExt,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::taskrun::{create_taskrun, taskrun_api, TaskRunSpec};

pub const CUSTOMRUN_GROUP: &str = "tekton.dev";
pub const CUSTOMRUN_VERSION: &str = "v1beta1";
pub const CUSTOMRUN_KIND: &str = "CustomRun";
pub const CUSTOMRUN_PLURAL: &str = "customruns";

const OUR_GROUP: &str = "gingersociety.org";
const OUR_VERSION: &str = "v1alpha1";
const OUR_KIND: &str = "RemoteTask";

const REQUEUE_AFTER_ERROR_SECS: u64 = 15;
const REQUEUE_WHILE_RUNNING_SECS: u64 = 5;

// ── context ───────────────────────────────────────────────────────────────────

pub struct CustomRunContext {
    pub client: Client,
    pub sidekick_url: String,
    pub auth_secret_name: String,
}

// ── errors ────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum CustomRunError {
    #[error("kube: {0}")]
    Kube(#[from] kube::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("taskrun: {0}")]
    TaskRun(#[from] crate::taskrun::TaskRunError),
    #[error("customRun '{0}' is missing spec.customRef")]
    MissingCustomRef(String),
    #[error("param parse: {0}")]
    ParamParse(String),
}

// ── CustomRun spec types (minimal, only what we need) ─────────────────────────

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

// ── status types ──────────────────────────────────────────────────────────────

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

// ── param parsing ─────────────────────────────────────────────────────────────

struct ParsedSpec {
    capability: String,
    script: String,
    cleanup: Option<String>,
    /// JSON env entries ready to embed in the TaskRun step spec.
    env: Vec<serde_json::Value>,
}

fn parse_params(params: &[Param]) -> Result<ParsedSpec, CustomRunError> {
    let mut capability: Option<String> = None;
    let mut script: Option<String> = None;
    let mut cleanup: Option<String> = None;
    let mut env: Vec<serde_json::Value> = Vec::new();

    for p in params {
        match p.name.as_str() {
            "capability" => {
                capability = p.value.as_str().map(str::to_string).or(capability)
            }
            "script" => script = p.value.as_str().map(str::to_string).or(script),
            "cleanup" => cleanup = p.value.as_str().map(str::to_string).or(cleanup),
            "env" => {
                if let Some(raw) = p.value.as_str() {
                    // The env param is a YAML list of {name, value} or
                    // {name, valueFrom: {secretKeyRef: ...}} entries —
                    // exactly the same shape as a Kubernetes EnvVar.
                    let parsed: Vec<serde_json::Value> = serde_yaml::from_str(raw)
                        .map_err(|e| CustomRunError::ParamParse(format!("env: {e}")))?;
                    env = parsed;
                }
            }
            other => {
                eprintln!("[customrun] ignoring unrecognized param '{other}'");
            }
        }
    }

    Ok(ParsedSpec {
        capability: capability.unwrap_or_else(|| "unix".to_string()),
        script: script
            .ok_or_else(|| CustomRunError::ParamParse("missing required param 'script'".into()))?,
        cleanup,
        env,
    })
}

// ── reconciler ────────────────────────────────────────────────────────────────

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

    // Parse the spec
    let spec_value = run
        .data
        .get("spec")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    let spec: CustomRunSpec = serde_json::from_value(spec_value)?;

    // Only handle CustomRuns that target our CRD
    let custom_ref = spec
        .custom_ref
        .ok_or_else(|| CustomRunError::MissingCustomRef(name.clone()))?;

    let targets_us =
        custom_ref.api_version.as_deref() == Some(&format!("{OUR_GROUP}/{OUR_VERSION}"))
            && custom_ref.kind.as_deref() == Some(OUR_KIND);

    if !targets_us {
        return Ok(Action::await_change());
    }

    // Already terminal — nothing to do
    if is_terminal(&run) {
        return Ok(Action::await_change());
    }

    let customruns = customrun_api(&ctx.client, &ns);

    // If the TaskRun already exists we are already running — just sync status
    let taskrun_exists = taskrun_api(&ctx.client, &ns)
        .get_opt(&name)
        .await?
        .is_some();

    if taskrun_exists {
        // Sync TaskRun status → CustomRun status
        sync_status(&customruns, &ctx.client, &ns, &name).await?;
        return Ok(Action::requeue(std::time::Duration::from_secs(
            REQUEUE_WHILE_RUNNING_SECS,
        )));
    }

    // Parse params and create the TaskRun
    let parsed = match parse_params(&spec.params) {
        Ok(p) => p,
        Err(e) => {
            set_failed(&customruns, &name, &format!("invalid params: {e}")).await?;
            return Ok(Action::await_change());
        }
    };

    let owner_uid = run.uid().unwrap_or_default();

    let taskrun_spec = TaskRunSpec {
        name: &name,
        ns: &ns,
        owner_api_version: &format!("{CUSTOMRUN_GROUP}/{CUSTOMRUN_VERSION}"),
        owner_kind: CUSTOMRUN_KIND,
        owner_uid: owner_uid.as_str(),
        capability: &parsed.capability,
        script: &parsed.script,
        cleanup: parsed.cleanup.as_deref(),
        env: parsed.env,
        sidekick_url: &ctx.sidekick_url,
        auth_secret_name: &ctx.auth_secret_name,
    };

    if let Err(e) = create_taskrun(&ctx.client, taskrun_spec).await {
        set_failed(&customruns, &name, &format!("failed to create TaskRun: {e}")).await?;
        return Ok(Action::await_change());
    }

    set_running(&customruns, &name).await?;

    Ok(Action::requeue(std::time::Duration::from_secs(
        REQUEUE_WHILE_RUNNING_SECS,
    )))
}

pub fn customrun_error_policy(
    _run: Arc<DynamicObject>,
    err: &CustomRunError,
    _ctx: Arc<CustomRunContext>,
) -> Action {
    eprintln!("[customrun] error_policy: {:?}", err);
    Action::requeue(std::time::Duration::from_secs(REQUEUE_AFTER_ERROR_SECS))
}

// ── status helpers ────────────────────────────────────────────────────────────

fn is_terminal(run: &DynamicObject) -> bool {
    run.data
        .get("status")
        .and_then(|s| s.get("conditions"))
        .and_then(|c| c.as_array())
        .map(|conds| {
            conds.iter().any(|c| {
                c.get("type").and_then(|t| t.as_str()) == Some("Succeeded")
                    && c.get("status").and_then(|s| s.as_str()) != Some("Unknown")
            })
        })
        .unwrap_or(false)
}

/// Read the TaskRun's status and mirror it onto the CustomRun.
async fn sync_status(
    customruns: &Api<DynamicObject>,
    client: &Client,
    ns: &str,
    name: &str,
) -> Result<(), CustomRunError> {
    let taskrun = match taskrun_api(client, ns).get_opt(name).await? {
        Some(tr) => tr,
        None => return Ok(()), // not yet visible
    };

    // Read the TaskRun's Succeeded condition
    let conditions = taskrun
        .data
        .get("status")
        .and_then(|s| s.get("conditions"))
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let succeeded_cond = conditions
        .iter()
        .find(|c| c.get("type").and_then(|t| t.as_str()) == Some("Succeeded"));

    match succeeded_cond {
        None => {
            // TaskRun exists but no condition yet — still starting
            set_running(customruns, name).await?;
        }
        Some(c) => {
            let status = c.get("status").and_then(|s| s.as_str()).unwrap_or("Unknown");
            match status {
                "True" => set_succeeded(customruns, name).await?,
                "False" => {
                    let msg = c
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("TaskRun failed");
                    set_failed(customruns, name, msg).await?;
                }
                _ => {
                    set_running(customruns, name).await?;
                }
            }
        }
    }

    Ok(())
}

async fn set_running(
    customruns: &Api<DynamicObject>,
    name: &str,
) -> Result<(), CustomRunError> {
    patch_status(
        customruns,
        name,
        CustomRunCondition {
            type_: "Succeeded".into(),
            status: "Unknown".into(),
            reason: "Running".into(),
            message: "TaskRun is running".into(),
            last_transition_time: Some(now()),
        },
        false,
    )
    .await
}

async fn set_succeeded(
    customruns: &Api<DynamicObject>,
    name: &str,
) -> Result<(), CustomRunError> {
    patch_status(
        customruns,
        name,
        CustomRunCondition {
            type_: "Succeeded".into(),
            status: "True".into(),
            reason: "TaskRunSucceeded".into(),
            message: "TaskRun completed successfully".into(),
            last_transition_time: Some(now()),
        },
        true,
    )
    .await
}

async fn set_failed(
    customruns: &Api<DynamicObject>,
    name: &str,
    message: &str,
) -> Result<(), CustomRunError> {
    patch_status(
        customruns,
        name,
        CustomRunCondition {
            type_: "Succeeded".into(),
            status: "False".into(),
            reason: "TaskRunFailed".into(),
            message: message.to_string(),
            last_transition_time: Some(now()),
        },
        true,
    )
    .await
}

async fn patch_status(
    customruns: &Api<DynamicObject>,
    name: &str,
    condition: CustomRunCondition,
    is_terminal: bool,
) -> Result<(), CustomRunError> {
    let mut status = json!({ "conditions": [condition] });
    if is_terminal {
        status["completionTime"] = json!(rfc3339_now());
    }
    let patch = json!({ "status": status });
    customruns
        .patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

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

fn now() -> Time {
    Time(k8s_openapi::jiff::Timestamp::now())
}

fn rfc3339_now() -> String {
    k8s_openapi::jiff::Timestamp::now()
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}