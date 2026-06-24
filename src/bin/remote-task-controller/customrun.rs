use std::collections::BTreeMap;
use std::sync::Arc;

use k8s_openapi::api::core::v1::{Container, EnvVar as K8sEnvVar, ObjectReference, Pod, PodSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{OwnerReference, Time};
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

use crate::dispatch::{now, run_dispatch};
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
    pub http: reqwest::Client,
    pub sidekick_url: String,
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

fn spec_from_params(params: &[Param]) -> Result<RemoteTaskSpec, CustomRunError> {
    let mut capability: Option<String> = None;
    let mut script: Option<String> = None;
    let mut cleanup: Option<String> = None;
    let mut env: Vec<RemoteTaskEnvVar> = Vec::new();

    for p in params {
        match p.name.as_str() {
            "capability" => capability = p.value.as_str().map(str::to_string).or(capability),
            "script"     => script     = p.value.as_str().map(str::to_string).or(script),
            "cleanup"    => cleanup    = p.value.as_str().map(str::to_string).or(cleanup),
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

    Ok(RemoteTaskSpec { capability, env, script, cleanup })
}

pub async fn create_log_mirror_pod_by_name(
    client: &Client,
    ns: &str,
    remote_task_name: &str,
    remote_task_uid: String,
) -> Result<(), kube::Error> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);

    if pods.get_opt(remote_task_name).await?.is_some() {
        println!("[customrun-controller] log-mirror pod {remote_task_name} already exists, skipping");
        return Ok(());
    }

    let pod = Pod {
        metadata: ObjectMeta {
            name: Some(remote_task_name.to_string()),
            namespace: Some(ns.to_string()),
            owner_references: Some(vec![OwnerReference {
                api_version: "gingersociety.org/v1alpha1".to_string(),
                kind: "RemoteTask".to_string(),
                name: remote_task_name.to_string(),
                uid: remote_task_uid,
                controller: Some(true),
                block_owner_deletion: Some(false),
            }]),
            labels: Some(BTreeMap::from([
                ("app".to_string(), "remote-task-log-mirror".to_string()),
                ("remotetask".to_string(), remote_task_name.to_string()),
            ])),
            ..Default::default()
        },
        spec: Some(PodSpec {
            restart_policy: Some("Never".to_string()),
            service_account_name: Some("remote-task-controller".to_string()),
            containers: vec![Container {
                name: "log-mirror".to_string(),
                image: Some("bitnami/kubectl:latest".to_string()),
                command: Some(vec!["/bin/sh".to_string()]),
                args: Some(vec![
                    "-c".to_string(),
                    format!(
                        r#"
SEEN_LINES=0
echo "[log-mirror] Watching RemoteTask {name} logs in {ns}..."
while true; do
  PHASE=$(kubectl get remotetask {name} -n {ns} \
    -o jsonpath='{{.status.phase}}' 2>/dev/null || echo "pending")

  LOGS=$(kubectl get configmap {name} -n {ns} \
    -o jsonpath='{{.data.logs}}' 2>/dev/null || true)

  if [ -n "$LOGS" ]; then
    TOTAL=$(printf '%s\n' "$LOGS" | wc -l)
    if [ "$TOTAL" -gt "$SEEN_LINES" ]; then
      printf '%s\n' "$LOGS" | tail -n +"$((SEEN_LINES + 1))"
      SEEN_LINES=$TOTAL
    fi
  fi

  if [ "$PHASE" = "succeeded" ]; then
    echo "[log-mirror] RemoteTask completed successfully"
    exit 0
  elif [ "$PHASE" = "failed" ]; then
    FAIL_MSG=$(kubectl get remotetask {name} -n {ns} \
      -o jsonpath='{{.status.message}}' 2>/dev/null || echo "unknown error")
    echo "[log-mirror] RemoteTask failed: $FAIL_MSG"
    exit 1
  fi

  sleep 1
done
"#,
                        name = remote_task_name,
                        ns = ns,
                    ),
                ]),
                env: Some(vec![K8sEnvVar {
                    name: "HOME".to_string(),
                    value: Some("/tmp".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    };

    pods.create(&PostParams::default(), &pod).await?;
    println!("[customrun-controller] created log-mirror pod {remote_task_name}");
    Ok(())
}

pub async fn reconcile_customrun(
    run: Arc<DynamicObject>,
    ctx: Arc<CustomRunContext>,
) -> Result<Action, CustomRunError> {
    let name = run.name_any();
    let ns = run
        .metadata.namespace.clone()
        .unwrap_or_else(|| "default".to_string());

    let spec_value = run.data.get("spec").cloned()
        .unwrap_or(Value::Object(Default::default()));
    let spec: CustomRunSpec = serde_json::from_value(spec_value)?;

    let custom_ref = spec.custom_ref
        .ok_or_else(|| CustomRunError::MissingCustomRef(name.clone()))?;

    let targets_us =
        custom_ref.api_version.as_deref() == Some(&format!("{OUR_GROUP}/{OUR_VERSION}"))
            && custom_ref.kind.as_deref() == Some(OUR_KIND);

    if !targets_us {
        return Ok(Action::await_change());
    }

    let already_terminal = run.data
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
            sync_status_from_remote_task(&customruns, &name, &existing).await?;
        }
        None => {
            let task_spec = match spec_from_params(&spec.params) {
                Ok(s) => s,
                Err(e) => {
                    set_customrun_failed(&customruns, &name, &format!("invalid params: {e}")).await?;
                    return Ok(Action::await_change());
                }
            };

            let task = build_owned_remote_task(&run, &name, &ns, task_spec);
            let created = remote_tasks.create(&PostParams::default(), &task).await?;
            let remote_task_uid = created.uid().unwrap_or_default();

            println!("[customrun-controller] created RemoteTask {ns}/{name} uid={remote_task_uid}");

            if let Err(e) = create_log_mirror_pod_by_name(
                &ctx.client, &ns, &name, remote_task_uid.clone(),
            ).await {
                eprintln!("[customrun-controller] failed to create log-mirror pod: {e}");
            }

            emit_event(
                &ctx.client, &ns, &object_ref_for(&run),
                "Normal", "RemoteTaskCreated",
                &format!("Created RemoteTask {name} from CustomRun params"),
            ).await?;

            set_customrun_running(&customruns, &name).await?;

            let task_ref = ObjectReference {
                api_version: Some("gingersociety.org/v1alpha1".to_string()),
                kind: Some("RemoteTask".to_string()),
                name: Some(name.clone()),
                namespace: Some(ns.clone()),
                uid: Some(remote_task_uid),
                ..Default::default()
            };
            let customrun_ref = object_ref_for(&run);

            tokio::spawn(run_dispatch(
                ctx.client.clone(),
                ctx.http.clone(),
                ctx.sidekick_url.clone(),
                ns.clone(),
                name.clone(),
                created,
                task_ref,
                Some(customrun_ref),
            ));
        }
    }

    Ok(Action::requeue(std::time::Duration::from_secs(REQUEUE_WHILE_RUNNING_SECS)))
}

pub fn customrun_error_policy(
    _run: Arc<DynamicObject>,
    err: &CustomRunError,
    _ctx: Arc<CustomRunContext>,
) -> Action {
    eprintln!("[customrun-controller] error_policy: {:?}", err);
    Action::requeue(std::time::Duration::from_secs(REQUEUE_AFTER_ERROR_SECS))
}

async fn sync_status_from_remote_task(
    customruns: &Api<DynamicObject>,
    name: &str,
    task: &RemoteTask,
) -> Result<(), CustomRunError> {
    let phase = task.status.as_ref().map(|s| s.phase.clone()).unwrap_or_default();

    match phase {
        RemoteTaskPhase::Pending | RemoteTaskPhase::Running => {
            set_customrun_running(customruns, name).await?;
        }
        RemoteTaskPhase::Succeeded => {
            let exit_code = task.status.as_ref().and_then(|s| s.exit_code).unwrap_or(0);
            set_customrun_succeeded(customruns, name, exit_code).await?;
        }
        RemoteTaskPhase::Failed => {
            let msg = task.status.as_ref()
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
        customruns, name,
        vec![CustomRunCondition {
            type_: "Succeeded".to_string(),
            status: "Unknown".to_string(),
            reason: "Running".to_string(),
            message: "RemoteTask is running".to_string(),
            last_transition_time: Some(now()),
        }],
        None, false,
    ).await
}

async fn set_customrun_succeeded(
    customruns: &Api<DynamicObject>,
    name: &str,
    exit_code: i32,
) -> Result<(), CustomRunError> {
    patch_status(
        customruns, name,
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
    ).await
}

async fn set_customrun_failed(
    customruns: &Api<DynamicObject>,
    name: &str,
    message: &str,
) -> Result<(), CustomRunError> {
    patch_status(
        customruns, name,
        vec![CustomRunCondition {
            type_: "Succeeded".to_string(),
            status: "False".to_string(),
            reason: "RemoteTaskFailed".to_string(),
            message: message.to_string(),
            last_transition_time: Some(now()),
        }],
        None, true,
    ).await
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

fn rfc3339_now() -> String {
    k8s_openapi::jiff::Timestamp::now()
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}