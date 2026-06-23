use std::collections::HashMap;
use std::sync::Arc;

use futures_util::StreamExt;
use k8s_openapi::{api::core::v1::ObjectReference, apimachinery::pkg::apis::meta::v1::OwnerReference};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::{
    Client, ResourceExt, api::{Api, DynamicObject, ObjectMeta, Patch, PatchParams, PostParams}, discovery::ApiResource, runtime::{Controller, controller::Action, watcher},
};
use serde::Serialize;
use serde_json::{json, Value};

use ginger_infra::remote_task::{RemoteTask, RemoteTaskCondition, RemoteTaskPhase, RemoteTaskStatus};

mod customrun;
mod events;

use customrun::{
    customrun_error_policy, reconcile_customrun, CustomRunContext, CUSTOMRUN_GROUP,
    CUSTOMRUN_KIND, CUSTOMRUN_PLURAL, CUSTOMRUN_VERSION,
};
use events::emit_event;

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
    sidekick_url: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("[tekton-controller] starting...");

    let sidekick_url = std::env::var("SIDEKICK_URL")
        .map_err(|_| anyhow::anyhow!("SIDEKICK_URL env var is required"))?;

    println!("[tekton-controller] sidekick_url={}", sidekick_url);

    let client = Client::try_default().await?;

    let remote_tasks: Api<RemoteTask> = Api::all(client.clone());
    let ctx = Arc::new(ControllerContext {
        client: client.clone(),
        http: reqwest::Client::new(),
        sidekick_url,
    });

    // Second watch: Tekton CustomRun objects (tekton.dev/v1beta1), filtered
    // inside reconcile_customrun to only act on ones whose customRef targets
    // our RemoteTask GVK. DynamicObject since we don't own this CRD and no
    // Rust crate ships its types.
    let customrun_ar = ApiResource {
        group: CUSTOMRUN_GROUP.to_string(),
        version: CUSTOMRUN_VERSION.to_string(),
        api_version: format!("{CUSTOMRUN_GROUP}/{CUSTOMRUN_VERSION}"),
        kind: CUSTOMRUN_KIND.to_string(),
        plural: CUSTOMRUN_PLURAL.to_string(),
    };
    let customruns: Api<DynamicObject> = Api::all_with(client.clone(), &customrun_ar);
    let customrun_ctx = Arc::new(CustomRunContext {
        client: client.clone(),
    });

    println!("[tekton-controller] watching RemoteTask across all namespaces");
    println!("[tekton-controller] watching CustomRun (tekton.dev/v1beta1) across all namespaces");

    let remote_task_controller = Controller::new(remote_tasks, watcher::Config::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok(o) => println!("[tekton-controller] reconciled remotetask {:?}", o),
                Err(e) => eprintln!("[tekton-controller] remotetask reconcile error: {:?}", e),
            }
        });

    let customrun_controller = Controller::new_with(customruns, watcher::Config::default(), customrun_ar)
        .run(reconcile_customrun, customrun_error_policy, customrun_ctx)
        .for_each(|res| async move {
            match res {
                Ok(o) => println!("[tekton-controller] reconciled customrun {:?}", o),
                Err(e) => eprintln!("[tekton-controller] customrun reconcile error: {:?}", e),
            }
        });

    // Run both controllers concurrently in the same process.
    tokio::join!(remote_task_controller, customrun_controller);

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

    match phase {
        RemoteTaskPhase::Pending => {
            println!("[tekton-controller] dispatching {}/{}", ns, name);
            dispatch(&task, &ctx, &ns, &name).await?;
        }
        RemoteTaskPhase::Running => {
            println!("[tekton-controller] {}/{} already Running — no resume logic, leaving as-is", ns, name);
        }
        RemoteTaskPhase::Succeeded | RemoteTaskPhase::Failed => {}
    }

    Ok(Action::await_change())
}

async fn dispatch(
    task: &RemoteTask,
    ctx: &ControllerContext,
    ns: &str,
    name: &str,
) -> Result<(), ReconcileError> {
    let api: Api<RemoteTask> = Api::namespaced(ctx.client.clone(), ns);

    let env_map = resolve_env(&ctx.client, ns, &task.spec.env).await?;

    // ── find owning CustomRun (if this RemoteTask was created by our customrun controller)
    let customrun_ref: Option<ObjectReference> = task
        .metadata
        .owner_references
        .as_ref()
        .and_then(|owners| owners.iter().find(|o| o.kind == "CustomRun"))
        .map(|owner| ObjectReference {
            api_version: Some(owner.api_version.clone()),
            kind: Some(owner.kind.clone()),
            name: Some(owner.name.clone()),
            namespace: Some(ns.to_string()),
            uid: Some(owner.uid.clone()),
            ..Default::default()
        });

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

    if let Err(e) = create_log_mirror_pod(&ctx.client, ns, name, task).await {
        // non-fatal — job still runs, just no dashboard logs
        eprintln!("[tekton-controller] failed to create log-mirror pod: {e}");
    }

    let request = RunJobRequest {
        capability: task.spec.capability.clone(),
        script: task.spec.script.clone(),
        cleanup_script: task.spec.cleanup.clone(),
        env: env_map.into_iter().map(|(name, value)| EnvVar { name, value }).collect(),
    };

    let task_ref = object_ref_for_task(task, ns, name);
    let result = stream_job(
        &ctx.client,
        &ctx.http,
        &ctx.sidekick_url,
        &request,
        ns,
        &task_ref,
        customrun_ref.as_ref(), // ← new
    ).await;

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

async fn stream_job(
    client: &Client,
    http: &reqwest::Client,
    url: &str,
    request: &RunJobRequest,
    ns: &str,
    task_ref: &ObjectReference,
    customrun_ref: Option<&ObjectReference>, // ← new
) -> Result<i32, ReconcileError> {
    let response = http
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
                    println!("[remote-task:{stream_name}] {line_text}");

                    let message = format!("[{stream_name}] {line_text}");

                    // emit on RemoteTask (existing behaviour)
                    if let Err(e) = emit_event(
                        client, ns, task_ref, "Normal", "RemoteTaskLog", &message,
                    ).await {
                        eprintln!("[tekton-controller] failed to emit log event on remotetask: {e}");
                    }

                    // ── also emit on the owning CustomRun so the Tekton dashboard
                    //    Events tab and `tkn pr logs` can surface the output
                    if let Some(cr_ref) = customrun_ref {
                        if let Err(e) = emit_event(
                            client, ns, cr_ref, "Normal", "RemoteTaskLog", &message,
                        ).await {
                            eprintln!("[tekton-controller] failed to emit log event on customrun: {e}");
                        }
                    }
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
            "[tekton-controller] env '{}' has no supported source — skipping",
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
    Time(k8s_openapi::jiff::Timestamp::now())
}

fn object_ref_for_task(task: &RemoteTask, ns: &str, name: &str) -> ObjectReference {
    ObjectReference {
        api_version: Some("gingersociety.org/v1alpha1".to_string()),
        kind: Some("RemoteTask".to_string()),
        name: Some(name.to_string()),
        namespace: Some(ns.to_string()),
        uid: task.uid(),
        ..Default::default()
    }
}

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
    env: Vec<EnvVar>,
}

use k8s_openapi::api::core::v1::{
    Container, EnvVar as K8sEnvVar, Pod, PodSpec,
};

async fn create_log_mirror_pod(
    client: &Client,
    ns: &str,
    remote_task_name: &str,
    owner: &RemoteTask,
) -> Result<(), ReconcileError> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);

    // Idempotent — if the pod already exists (e.g. controller restarted), skip
    if pods.get_opt(remote_task_name).await?.is_some() {
        return Ok(());
    }

    let uid = owner.uid().unwrap_or_default();
    let api_version = "gingersociety.org/v1alpha1".to_string();

    let pod = Pod {
        metadata: ObjectMeta {
            name: Some(remote_task_name.to_string()),
            namespace: Some(ns.to_string()),
            // owned by the RemoteTask so it gets GC'd automatically
            owner_references: Some(vec![OwnerReference {
                api_version: api_version.clone(),
                kind: "RemoteTask".to_string(),
                name: remote_task_name.to_string(),
                uid,
                controller: Some(true),
                block_owner_deletion: Some(false),
            }]),
            labels: Some(std::collections::BTreeMap::from([
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
                // kubectl is all we need — this image is tiny (~13MB)
                image: Some("bitnami/kubectl:latest".to_string()),
                command: Some(vec!["/bin/sh".to_string()]),
                args: Some(vec![
                    "-c".to_string(),
                    // Poll events every 2s and print new ones.
                    // Exit when the RemoteTask reaches a terminal phase.
                    format!(r#"
SEEN=""
echo "[log-mirror] Waiting for RemoteTask {name} to start..."
while true; do
  PHASE=$(kubectl get remotetask {name} -n {ns} \
    -o jsonpath='{{.status.phase}}' 2>/dev/null || echo "pending")

  # Fetch all RemoteTaskLog events, print only ones we haven't seen
  EVENTS=$(kubectl get events -n {ns} \
    --field-selector involvedObject.name={name},reason=RemoteTaskLog \
    --sort-by='.metadata.creationTimestamp' \
    -o jsonpath='{{range .items[*]}}{{.metadata.uid}} {{.message}}{{"\n"}}{{end}}' \
    2>/dev/null || true)

  while IFS= read -r line; do
    [ -z "$line" ] && continue
    UID=$(echo "$line" | cut -d' ' -f1)
    MSG=$(echo "$line" | cut -d' ' -f2-)
    case "$SEEN" in
      *"$UID"*) ;;  # already printed
      *) echo "$MSG"; SEEN="$SEEN $UID" ;;
    esac
  done <<EOF
$EVENTS
EOF

  if [ "$PHASE" = "succeeded" ]; then
    echo "[log-mirror] RemoteTask completed successfully (exit 0)"
    exit 0
  elif [ "$PHASE" = "failed" ]; then
    MSG=$(kubectl get remotetask {name} -n {ns} \
      -o jsonpath='{{.status.message}}' 2>/dev/null || echo "unknown error")
    echo "[log-mirror] RemoteTask failed: $MSG"
    exit 1
  fi

  sleep 2
done
"#,
                    name = remote_task_name,
                    ns = ns,
                    ),
                ]),
                env: Some(vec![
                    // Prevent kubectl from complaining about missing home dir
                    K8sEnvVar {
                        name: "HOME".to_string(),
                        value: Some("/tmp".to_string()),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    };

    pods.create(&PostParams::default(), &pod).await?;
    println!("[tekton-controller] created log-mirror pod for RemoteTask {remote_task_name}");
    Ok(())
}