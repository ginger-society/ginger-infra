//! CustomRun reconciler.
//!
//! Tekton creates a CustomRun when a Pipeline task uses:
//!
//!   taskRef:
//!     apiVersion: gingersociety.org/v1alpha1
//!     kind: RemoteTask
//!
//! This reconciler:
//!   1. Creates a TaskRun labelled with the CustomRun name.
//!   2. Polls the TaskRun by label to mirror status back onto the CustomRun.
//!
//! NOTE: Tekton does NOT create its own TaskRun for a CustomRun step — that
//! is the whole point of the CustomRun extension mechanism. The controller
//! owns the TaskRun entirely. Tekton just watches the CustomRun status.
//!
//! The TaskRun this controller creates is named <customrun-name>-exec and
//! labelled `remotetask-customrun: <customrun-name>`.
//! The label key has NO slash — slashes in label keys are valid in Kubernetes
//! but the kube-rs ListParams label selector does not escape them, causing the
//! API server to reject the selector with a 400. Use a plain key instead.
//!
//! ## Credentials / workspace
//!
//! Previously the runner mounted a Kubernetes Secret containing auth.json.
//! That is replaced by the shared `creds` workspace written by the
//! init-credentials step injected by ginger-gitter. Tekton propagates the
//! PipelineRun's workspace bindings onto each CustomRun via
//! `spec.workspaces[]`, so we read the PVC claim name from there and pass it
//! to the TaskRun builder. The runner reads
//! /workspace/creds/ginger-society/auth.json directly.

use std::sync::Arc;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::{
    api::{Api, DynamicObject, ListParams, Patch, PatchParams},
    discovery::ApiResource,
    runtime::controller::Action,
    Client, ResourceExt,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::taskrun::{create_taskrun, TaskRunSpec};

pub const CUSTOMRUN_GROUP: &str = "tekton.dev";
pub const CUSTOMRUN_VERSION: &str = "v1beta1";
pub const CUSTOMRUN_KIND: &str = "CustomRun";
pub const CUSTOMRUN_PLURAL: &str = "customruns";

const OUR_GROUP: &str = "gingersociety.org";
const OUR_VERSION: &str = "v1alpha1";
const OUR_KIND: &str = "RemoteTask";

// No slash in the label key — avoids kube-rs label selector escaping issues.
const CUSTOMRUN_LABEL: &str = "remotetask-customrun";

/// The workspace name that ginger-gitter injects and init-credentials writes
/// to. Must match the name declared in the Pipeline's `workspaces:` list and
/// the name used in taskrun.rs.
const CREDS_WORKSPACE_NAME: &str = "creds";

const REQUEUE_AFTER_ERROR_SECS: u64 = 15;
const REQUEUE_WHILE_RUNNING_SECS: u64 = 5;

// ── context ───────────────────────────────────────────────────────────────────

pub struct CustomRunContext {
    pub client: Client,
    pub sidekick_url: String,
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

// ── CustomRun spec types ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CustomRunSpec {
    #[serde(rename = "customRef")]
    custom_ref: Option<CustomRef>,
    #[serde(default)]
    params: Vec<Param>,
    /// Workspace bindings propagated from the PipelineRun by Tekton.
    /// We look for the "creds" workspace here to get the PVC claim name.
    #[serde(default)]
    workspaces: Vec<WorkspaceBinding>,
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

/// A single workspace binding as Tekton copies it onto the CustomRun.
/// We only need `name` and the PVC claim name; other binding types
/// (emptyDir, configMap, secret) are not relevant for the creds workspace.
#[derive(Debug, Deserialize)]
struct WorkspaceBinding {
    name: String,
    #[serde(rename = "persistentVolumeClaim")]
    persistent_volume_claim: Option<PvcRef>,
}

#[derive(Debug, Deserialize)]
struct PvcRef {
    #[serde(rename = "claimName")]
    claim_name: String,
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
    env: Vec<serde_json::Value>,
}

fn parse_params(params: &[Param]) -> Result<ParsedSpec, CustomRunError> {
    let mut capability: Option<String> = None;
    let mut script: Option<String> = None;
    let mut cleanup: Option<String> = None;
    let mut env: Vec<serde_json::Value> = Vec::new();

    for p in params {
        match p.name.as_str() {
            "capability" => capability = p.value.as_str().map(str::to_string).or(capability),
            "script"     => script     = p.value.as_str().map(str::to_string).or(script),
            "cleanup"    => cleanup    = p.value.as_str().map(str::to_string).or(cleanup),
            "env" => {
                if let Some(raw) = p.value.as_str() {
                    let parsed: Vec<serde_json::Value> = serde_yaml::from_str(raw)
                        .map_err(|e| CustomRunError::ParamParse(format!("env: {e}")))?;
                    env = parsed;
                }
            }
            other => eprintln!("[customrun] ignoring unrecognized param '{other}'"),
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

/// Extract the PVC claim name for the `creds` workspace from the CustomRun's
/// workspace bindings. Returns `None` if no creds workspace was bound (e.g.
/// the pipeline didn't declare one, or this is a test/dev run).
fn extract_creds_claim(workspaces: &[WorkspaceBinding]) -> Option<String> {
    workspaces
        .iter()
        .find(|w| w.name == CREDS_WORKSPACE_NAME)
        .and_then(|w| w.persistent_volume_claim.as_ref())
        .map(|pvc| pvc.claim_name.clone())
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

    let spec_value = run
        .data
        .get("spec")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    let spec: CustomRunSpec = serde_json::from_value(spec_value)?;

    // Only handle CustomRuns that target our CRD.
    let custom_ref = spec
        .custom_ref
        .ok_or_else(|| CustomRunError::MissingCustomRef(name.clone()))?;

    let targets_us =
        custom_ref.api_version.as_deref() == Some(&format!("{OUR_GROUP}/{OUR_VERSION}"))
            && custom_ref.kind.as_deref() == Some(OUR_KIND);

    if !targets_us {
        return Ok(Action::await_change());
    }

    // Already terminal on the CustomRun itself — nothing more to do.
    if is_terminal(&run) {
        return Ok(Action::await_change());
    }

    let customruns = customrun_api(&ctx.client, &ns);

    // Look up OUR TaskRun by label (no slash in key = no selector escaping).
    let our_taskrun = find_our_taskrun(&ctx.client, &ns, &name).await?;

    // Resolve the creds workspace PVC claim from the CustomRun's workspace
    // bindings (propagated there by Tekton from the PipelineRun).
    let creds_workspace_claim = extract_creds_claim(&spec.workspaces);
    if creds_workspace_claim.is_none() {
        eprintln!(
            "[customrun] '{name}' has no '{}' workspace binding — \
             runner will not have access to credentials written by init-credentials",
            CREDS_WORKSPACE_NAME
        );
    }

    match our_taskrun {
        Some(taskrun) => {
            // TaskRun exists — sync its status onto the CustomRun.
            let terminal = sync_status_from_taskrun(&customruns, &name, &taskrun).await?;
            if terminal {
                return Ok(Action::await_change());
            }
            // Still running — keep polling.
            Ok(Action::requeue(std::time::Duration::from_secs(
                REQUEUE_WHILE_RUNNING_SECS,
            )))
        }
        None => {
            // No TaskRun yet — parse params and create one.
            let parsed = match parse_params(&spec.params) {
                Ok(p) => p,
                Err(e) => {
                    set_failed(&customruns, &name, &format!("invalid params: {e}")).await?;
                    return Ok(Action::await_change());
                }
            };

            let taskrun_name = format!("{name}-exec");
            let owner_uid = run.uid().unwrap_or_default();

            let mut extra_labels = std::collections::BTreeMap::new();
            extra_labels.insert(CUSTOMRUN_LABEL.to_string(), name.clone());

            // Forward Tekton's pipeline labels from the CustomRun onto our
            // TaskRun. The Tekton dashboard finds logs for a pipeline step by
            // looking for a TaskRun with these labels — without them the log
            // panel stays empty even when the step completes.
            for key in &[
                "tekton.dev/pipeline",
                "tekton.dev/pipelineRun",
                "tekton.dev/pipelineRunUID",
                "tekton.dev/pipelineTask",
                "tekton.dev/memberOf",
            ] {
                if let Some(val) = run.metadata.labels.as_ref()
                    .and_then(|l| l.get(*key))
                {
                    extra_labels.insert(key.to_string(), val.clone());
                }
            }

            let taskrun_spec = TaskRunSpec {
                name: &taskrun_name,
                ns: &ns,
                owner_api_version: &format!("{CUSTOMRUN_GROUP}/{CUSTOMRUN_VERSION}"),
                owner_kind: CUSTOMRUN_KIND,
                owner_name: &name,
                owner_uid: &owner_uid,
                capability: &parsed.capability,
                script: &parsed.script,
                cleanup: parsed.cleanup.as_deref(),
                env: parsed.env,
                sidekick_url: &ctx.sidekick_url,
                extra_labels,
                creds_workspace_claim,
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
    }
}

pub fn customrun_error_policy(
    _run: Arc<DynamicObject>,
    err: &CustomRunError,
    _ctx: Arc<CustomRunContext>,
) -> Action {
    eprintln!("[customrun] error_policy: {:?}", err);
    Action::requeue(std::time::Duration::from_secs(REQUEUE_AFTER_ERROR_SECS))
}

// ── TaskRun lookup ────────────────────────────────────────────────────────────

async fn find_our_taskrun(
    client: &Client,
    ns: &str,
    customrun_name: &str,
) -> Result<Option<DynamicObject>, CustomRunError> {
    let ar = ApiResource {
        group: "tekton.dev".into(),
        version: "v1".into(),
        api_version: "tekton.dev/v1".into(),
        kind: "TaskRun".into(),
        plural: "taskruns".into(),
    };
    let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), ns, &ar);
    let lp = ListParams::default()
        .labels(&format!("{CUSTOMRUN_LABEL}={customrun_name}"));

    let list = api.list(&lp).await?;

    if list.items.is_empty() {
        println!("[customrun] no TaskRun found with label {CUSTOMRUN_LABEL}={customrun_name}");
    } else {
        println!(
            "[customrun] found TaskRun '{}' for customrun '{customrun_name}'",
            list.items[0].name_any()
        );
    }

    Ok(list.items.into_iter().next())
}

// ── status sync ───────────────────────────────────────────────────────────────

async fn sync_status_from_taskrun(
    customruns: &Api<DynamicObject>,
    customrun_name: &str,
    taskrun: &DynamicObject,
) -> Result<bool, CustomRunError> {
    let conditions = taskrun
        .data
        .get("status")
        .and_then(|s| s.get("conditions"))
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let succeeded = conditions
        .iter()
        .find(|c| c.get("type").and_then(|t| t.as_str()) == Some("Succeeded"));

    match succeeded {
        None => {
            println!("[customrun] TaskRun for '{customrun_name}' has no Succeeded condition yet");
            set_running(customruns, customrun_name).await?;
            Ok(false)
        }
        Some(c) => {
            let status = c.get("status").and_then(|s| s.as_str()).unwrap_or("Unknown");
            println!("[customrun] TaskRun for '{customrun_name}' Succeeded={status}");
            match status {
                "True" => {
                    set_succeeded(customruns, customrun_name).await?;
                    Ok(true)
                }
                "False" => {
                    let msg = c
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("TaskRun failed");
                    set_failed(customruns, customrun_name, msg).await?;
                    Ok(true)
                }
                _ => {
                    set_running(customruns, customrun_name).await?;
                    Ok(false)
                }
            }
        }
    }
}

// ── status patchers ───────────────────────────────────────────────────────────

async fn set_running(customruns: &Api<DynamicObject>, name: &str) -> Result<(), CustomRunError> {
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