//! remote-task-controller
//!
//! Watches RemoteTask CRD objects and creates a Tekton TaskRun for each one.
//! That is the *entire* job of this controller. Tekton owns the pod lifecycle,
//! log streaming, dashboard integration, and pipeline view from that point on.
//!
//! Deleted vs the old controller:
//!   - customrun.rs  (CustomRun bridging — gone, not needed)
//!   - dispatch.rs   (in-process env resolution + HTTP to sidekick — gone)
//!   - events.rs     (manual k8s Event emission — Tekton does this)
//!   - job.rs        (Secret + ConfigMap + Job creation — gone)
//!
//! The runner pod (gingersociety/external-executor-runner) receives the script,
//! cleanup script, capability, and env vars as environment variables injected
//! directly into the TaskRun step spec. The entrypoint.sh in that image writes
//! them to /tmp and delegates to `ginger-infra rpc`.

use std::sync::Arc;

use futures_util::StreamExt;
use kube::{
    api::{Api, DynamicObject, Patch, PatchParams, PostParams},
    core::ObjectMeta,
    discovery::ApiResource,
    runtime::{controller::Action, watcher, Controller},
    Client, ResourceExt,
};
use serde_json::json;

use ginger_infra::remote_task::{RemoteTask, RemoteTaskPhase, RemoteTaskStatus};

const REQUEUE_AFTER_ERROR_SECS: u64 = 30;
const RUNNER_IMAGE_ENV: &str = "RUNNER_IMAGE";
const DEFAULT_RUNNER_IMAGE: &str = "gingersociety/external-executor-runner:latest";

// ── error type ────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("kube: {0}")]
    Kube(#[from] kube::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

// ── shared context ────────────────────────────────────────────────────────────

struct Ctx {
    client: Client,
    sidekick_url: String,
}

// ── entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("[remote-task-controller] starting...");

    let sidekick_url = std::env::var("SIDEKICK_URL")
        .map_err(|_| anyhow::anyhow!("SIDEKICK_URL env var is required"))?;

    println!(
        "[remote-task-controller] sidekick_url={} (injected into TaskRun pods as EXTERNAL_EXECUTOR_URL)",
        sidekick_url
    );

    let client = Client::try_default().await?;
    let ctx = Arc::new(Ctx {
        client: client.clone(),
        sidekick_url,
    });

    println!("[remote-task-controller] watching RemoteTask across all namespaces");

    Controller::new(
        Api::<RemoteTask>::all(client),
        watcher::Config::default(),
    )
    .run(reconcile, error_policy, ctx)
    .for_each(|result| async move {
        match result {
            Ok(obj) => println!("[remote-task-controller] reconciled {:?}", obj),
            Err(e) => eprintln!("[remote-task-controller] reconcile error: {:?}", e),
        }
    })
    .await;

    Ok(())
}

// ── error policy ──────────────────────────────────────────────────────────────

fn error_policy(_task: Arc<RemoteTask>, err: &Error, _ctx: Arc<Ctx>) -> Action {
    eprintln!("[remote-task-controller] error_policy: {:?}", err);
    Action::requeue(std::time::Duration::from_secs(REQUEUE_AFTER_ERROR_SECS))
}

// ── reconciler ────────────────────────────────────────────────────────────────

async fn reconcile(task: Arc<RemoteTask>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let name = task.name_any();
    let ns = task.namespace().unwrap_or_else(|| "default".into());

    let current_phase = task
        .status
        .as_ref()
        .map(|s| s.phase.clone())
        .unwrap_or_default();

    // Already handed off — Tekton owns it from here.
    if matches!(
        current_phase,
        RemoteTaskPhase::Running | RemoteTaskPhase::Succeeded | RemoteTaskPhase::Failed
    ) {
        return Ok(Action::await_change());
    }

    println!("[remote-task-controller] {ns}/{name} — creating TaskRun");

    let taskrun_api = taskrun_api(&ctx.client, &ns);

    // Idempotent: if the TaskRun already exists we just mark ourselves running
    // and stop watching — Tekton will finish it.
    if taskrun_api.get_opt(&name).await?.is_none() {
        create_taskrun(&ctx.client, &ns, &name, &task, &ctx.sidekick_url).await?;
    } else {
        println!(
            "[remote-task-controller] {ns}/{name} — TaskRun already exists, skipping create"
        );
    }

    mark_running(&ctx.client, &ns, &name).await?;

    Ok(Action::await_change())
}

// ── TaskRun creation ─────────────────────────────────────────────────────────

/// Build and POST a Tekton TaskRun that runs the script inside the runner image.
///
/// The script, cleanup script, and capability are passed as environment
/// variables. The runner image's entrypoint.sh writes them to /tmp and
/// delegates to `ginger-infra rpc`. All env vars declared in RemoteTaskSpec
/// (literal values or secretKeyRef) are forwarded directly into the step so
/// Kubernetes handles secret injection — the controller never reads secret
/// values itself.
async fn create_taskrun(
    client: &Client,
    ns: &str,
    name: &str,
    task: &RemoteTask,
    sidekick_url: &str,
) -> Result<(), Error> {
    let runner_image = std::env::var(RUNNER_IMAGE_ENV)
        .unwrap_or_else(|_| DEFAULT_RUNNER_IMAGE.into());

    // Forward user-declared env vars. secretKeyRef entries are passed through
    // verbatim so Kubernetes injects the value at pod start — the controller
    // never decrypts them.
    let mut step_env: Vec<serde_json::Value> = task.spec.env.iter().map(|e| {
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
    }).collect();

    // Controller-managed vars — appended after user vars so they can't be
    // accidentally shadowed by a user-declared env entry with the same name.
    step_env.extend([
        json!({ "name": "REMOTE_SCRIPT",        "value": task.spec.script }),
        json!({ "name": "REMOTE_CAPABILITY",    "value": task.spec.capability }),
        json!({ "name": "EXTERNAL_EXECUTOR_URL","value": sidekick_url }),
    ]);
    if let Some(cleanup) = &task.spec.cleanup {
        step_env.push(json!({ "name": "REMOTE_CLEANUP", "value": cleanup }));
    }

    let taskrun = json!({
        "apiVersion": "tekton.dev/v1",
        "kind": "TaskRun",
        "metadata": {
            "name": name,
            "namespace": ns,
            // Garbage-collect the TaskRun when the RemoteTask is deleted.
            "ownerReferences": [{
                "apiVersion": "gingersociety.org/v1alpha1",
                "kind": "RemoteTask",
                "name": name,
                "uid": task.uid().unwrap_or_default(),
                "controller": true,
                "blockOwnerDeletion": true,
            }],
            "labels": {
                "app.kubernetes.io/managed-by": "remote-task-controller",
                "remotetask": name,
            }
        },
        "spec": {
            // Inline task spec — no Task CR needed in the cluster.
            "taskSpec": {
                "steps": [{
                    "name": "run",
                    "image": runner_image,
                    // The runner's entrypoint.sh reads these env vars, writes
                    // REMOTE_SCRIPT → /tmp/script.sh (+ REMOTE_CLEANUP if set),
                    // then calls: ginger-infra rpc --envrc /dev/null --script …
                    "env": step_env,
                }]
            }
        }
    });

    let api = taskrun_api(client, ns);
    let obj: DynamicObject = serde_json::from_value(taskrun)?;

    match api.create(&PostParams::default(), &obj).await {
        Ok(_) => {
            println!("[remote-task-controller] created TaskRun {ns}/{name}");
            Ok(())
        }
        Err(kube::Error::Api(ae)) if ae.code == 409 => {
            println!("[remote-task-controller] TaskRun {ns}/{name} already exists");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

// ── status helpers ────────────────────────────────────────────────────────────

/// Mark the RemoteTask as Running so we don't re-create the TaskRun on the
/// next reconcile loop.
async fn mark_running(client: &Client, ns: &str, name: &str) -> Result<(), Error> {
    let api: Api<RemoteTask> = Api::namespaced(client.clone(), ns);
    let status = RemoteTaskStatus {
        phase: RemoteTaskPhase::Running,
        start_time: Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
            k8s_openapi::jiff::Timestamp::now(),
        )),
        ..Default::default()
    };
    let patch = json!({ "status": status });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

// ── API helpers ───────────────────────────────────────────────────────────────

fn taskrun_api(client: &Client, ns: &str) -> Api<DynamicObject> {
    let ar = ApiResource {
        group: "tekton.dev".into(),
        version: "v1".into(),
        api_version: "tekton.dev/v1".into(),
        kind: "TaskRun".into(),
        plural: "taskruns".into(),
    };
    Api::namespaced_with(client.clone(), ns, &ar)
}