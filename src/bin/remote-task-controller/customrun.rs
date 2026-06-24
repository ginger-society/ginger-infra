//! CustomRun reconciler.
//!
//! Tekton creates a CustomRun when a Pipeline task uses:
//!
//!   taskRef:
//!     apiVersion: gingersociety.org/v1alpha1
//!     kind: RemoteTask
//!
//! ## Why no TaskRun
//!
//! Creating a *TaskRun* from inside a CustomRun reconciler causes two problems:
//!
//!   1. Tekton itself also creates a TaskRun for the CustomRun step (named with
//!      a generated suffix). That TaskRun is stuck Pending forever because
//!      Tekton is waiting for the CustomRun's status to go terminal — which
//!      never happens because the controller is watching *its own* TaskRun
//!      instead of updating the CustomRun.
//!
//!   2. The pipeline view always shows the Tekton-created TaskRun (Pending),
//!      not the controller-created one (Succeeded).
//!
//! The correct pattern for a CustomRun controller is:
//!
//!   - Run the actual work yourself (here: a Kubernetes Job).
//!   - Poll the Job for completion.
//!   - Patch the CustomRun status directly when the Job finishes.
//!
//! Tekton sees the CustomRun go terminal and marks the pipeline step done.

use std::sync::Arc;

use k8s_openapi::{
    api::{
        batch::v1::{Job, JobSpec},
        core::v1::{
            Container, EnvVar, EnvVarSource, PodSpec, PodTemplateSpec, SecretKeySelector,
            Volume, VolumeMount,
        },
    },
    apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference, Time},
};
use kube::{
    api::{Api, DynamicObject, Patch, PatchParams, PostParams},
    runtime::controller::Action,
    Client, ResourceExt,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const CUSTOMRUN_GROUP: &str = "tekton.dev";
pub const CUSTOMRUN_VERSION: &str = "v1beta1";
pub const CUSTOMRUN_KIND: &str = "CustomRun";
pub const CUSTOMRUN_PLURAL: &str = "customruns";

const OUR_GROUP: &str = "gingersociety.org";
const OUR_VERSION: &str = "v1alpha1";
const OUR_KIND: &str = "RemoteTask";

/// Label stamped on every Job we create so we can find it by label.
const CUSTOMRUN_LABEL: &str = "remote-task-controller/customrun";

const RUNNER_IMAGE_ENV: &str = "RUNNER_IMAGE";
const DEFAULT_RUNNER_IMAGE: &str = "gingersociety/external-executor-runner:latest";

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
    env: Vec<EnvVar>,
}

fn parse_params(params: &[Param]) -> Result<ParsedSpec, CustomRunError> {
    let mut capability: Option<String> = None;
    let mut script: Option<String> = None;
    let mut cleanup: Option<String> = None;
    let mut env_json: Vec<serde_json::Value> = Vec::new();

    for p in params {
        match p.name.as_str() {
            "capability" => capability = p.value.as_str().map(str::to_string).or(capability),
            "script"     => script     = p.value.as_str().map(str::to_string).or(script),
            "cleanup"    => cleanup    = p.value.as_str().map(str::to_string).or(cleanup),
            "env" => {
                if let Some(raw) = p.value.as_str() {
                    let parsed: Vec<serde_json::Value> = serde_yaml::from_str(raw)
                        .map_err(|e| CustomRunError::ParamParse(format!("env: {e}")))?;
                    env_json = parsed;
                }
            }
            other => eprintln!("[customrun] ignoring unrecognized param '{other}'"),
        }
    }

    // Convert JSON env entries to typed k8s EnvVar structs.
    let mut env: Vec<EnvVar> = Vec::new();
    for v in env_json {
        let ev: EnvVar = serde_json::from_value(v)
            .map_err(|e| CustomRunError::ParamParse(format!("env entry: {e}")))?;
        env.push(ev);
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

    // Look up OUR Job by label.
    let our_job = find_our_job(&ctx.client, &ns, &name).await?;

    match our_job {
        Some(job) => {
            // Job exists — sync its status onto the CustomRun.
            let terminal = sync_status_from_job(&customruns, &name, &job).await?;
            if terminal {
                return Ok(Action::await_change());
            }
            Ok(Action::requeue(std::time::Duration::from_secs(
                REQUEUE_WHILE_RUNNING_SECS,
            )))
        }
        None => {
            // No Job yet — parse params and create one.
            let parsed = match parse_params(&spec.params) {
                Ok(p) => p,
                Err(e) => {
                    set_failed(&customruns, &name, &format!("invalid params: {e}")).await?;
                    return Ok(Action::await_change());
                }
            };

            let job_name = format!("{name}-exec");
            let owner_uid = run.uid().unwrap_or_default();

            if let Err(e) =
                create_job(&ctx, &ns, &job_name, &name, &owner_uid, parsed).await
            {
                set_failed(&customruns, &name, &format!("failed to create Job: {e}")).await?;
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

// ── Job creation ──────────────────────────────────────────────────────────────

async fn create_job(
    ctx: &CustomRunContext,
    ns: &str,
    job_name: &str,
    customrun_name: &str,
    owner_uid: &str,
    parsed: ParsedSpec,
) -> Result<(), CustomRunError> {
    let runner_image = std::env::var(RUNNER_IMAGE_ENV)
        .unwrap_or_else(|_| DEFAULT_RUNNER_IMAGE.into());

    // Build env: user-supplied first, then controller-managed vars appended so
    // they cannot be accidentally shadowed.
    let mut env = parsed.env;
    env.extend([
        EnvVar { name: "REMOTE_SCRIPT".into(),         value: Some(parsed.script),           value_from: None },
        EnvVar { name: "REMOTE_CAPABILITY".into(),     value: Some(parsed.capability),       value_from: None },
        EnvVar { name: "EXTERNAL_EXECUTOR_URL".into(), value: Some(ctx.sidekick_url.clone()), value_from: None },
    ]);
    if let Some(cleanup) = parsed.cleanup {
        env.push(EnvVar { name: "REMOTE_CLEANUP".into(), value: Some(cleanup), value_from: None });
    }

    let mut labels = std::collections::BTreeMap::new();
    labels.insert("app.kubernetes.io/managed-by".to_string(), "remote-task-controller".to_string());
    labels.insert(CUSTOMRUN_LABEL.to_string(), customrun_name.to_string());

    let job = Job {
        metadata: ObjectMeta {
            name: Some(job_name.to_string()),
            namespace: Some(ns.to_string()),
            labels: Some(labels.clone()),
            owner_references: Some(vec![OwnerReference {
                api_version: format!("{CUSTOMRUN_GROUP}/{CUSTOMRUN_VERSION}"),
                kind: CUSTOMRUN_KIND.to_string(),
                name: customrun_name.to_string(),
                uid: owner_uid.to_string(),
                controller: Some(true),
                block_owner_deletion: Some(true),
            }]),
            ..Default::default()
        },
        spec: Some(JobSpec {
            backoff_limit: Some(0),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    restart_policy: Some("Never".to_string()),
                    containers: vec![Container {
                        name: "run".to_string(),
                        image: Some(runner_image),
                        env: Some(env),
                        volume_mounts: Some(vec![VolumeMount {
                            name: "ginger-auth".to_string(),
                            mount_path: "/var/run/ginger-society".to_string(),
                            read_only: Some(true),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    }],
                    volumes: Some(vec![Volume {
                        name: "ginger-auth".to_string(),
                        secret: Some(k8s_openapi::api::core::v1::SecretVolumeSource {
                            secret_name: Some(ctx.auth_secret_name.clone()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    };

    let jobs: Api<Job> = Api::namespaced(ctx.client.clone(), ns);
    match jobs.create(&PostParams::default(), &job).await {
        Ok(_) => {
            println!("[customrun] created Job {ns}/{job_name}");
            Ok(())
        }
        Err(kube::Error::Api(ae)) if ae.code == 409 => {
            println!("[customrun] Job {ns}/{job_name} already exists, skipping");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

// ── Job lookup ────────────────────────────────────────────────────────────────

async fn find_our_job(
    client: &Client,
    ns: &str,
    customrun_name: &str,
) -> Result<Option<Job>, CustomRunError> {
    use kube::api::ListParams;
    let jobs: Api<Job> = Api::namespaced(client.clone(), ns);
    let lp = ListParams::default()
        .labels(&format!("{CUSTOMRUN_LABEL}={customrun_name}"));
    let list = jobs.list(&lp).await?;
    Ok(list.items.into_iter().next())
}

// ── status sync ───────────────────────────────────────────────────────────────

/// Mirror the Job's status onto the CustomRun.
/// Returns true if the CustomRun is now in a terminal state.
async fn sync_status_from_job(
    customruns: &Api<DynamicObject>,
    customrun_name: &str,
    job: &Job,
) -> Result<bool, CustomRunError> {
    let status = job.status.as_ref();

    // Job succeeded: .status.succeeded >= 1
    if status.and_then(|s| s.succeeded).unwrap_or(0) >= 1 {
        set_succeeded(customruns, customrun_name).await?;
        return Ok(true);
    }

    // Job failed: .status.failed >= 1 and backoff exhausted (backoffLimit=0)
    if status.and_then(|s| s.failed).unwrap_or(0) >= 1 {
        // Pull message from the job condition if available.
        let msg = job
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .and_then(|conds| conds.iter().find(|c| c.type_ == "Failed"))
            .and_then(|c| c.message.clone())
            .unwrap_or_else(|| "Job failed".to_string());
        set_failed(customruns, customrun_name, &msg).await?;
        return Ok(true);
    }

    // Still running.
    set_running(customruns, customrun_name).await?;
    Ok(false)
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
            message: "Job is running".into(),
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
            reason: "JobSucceeded".into(),
            message: "Job completed successfully".into(),
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
            reason: "JobFailed".into(),
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
    let ar = kube::discovery::ApiResource {
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