//! Shared TaskRun builder used by both the CustomRun and RemoteTask reconcilers.
//!
//! The runner image's entrypoint.sh:
//!   1. Reads auth.json from /workspace/creds/ginger-society/auth.json
//!      (written by the init-credentials step that runs before this task)
//!   2. Writes REMOTE_SCRIPT / REMOTE_CLEANUP from env to /tmp
//!   3. Calls: ginger-infra rpc --envrc /dev/null --script /tmp/script.sh …
//!
//! NOTE: The previous approach mounted a Kubernetes Secret containing
//! auth.json. That is no longer needed — the init-credentials step
//! (injected by ginger-gitter before pipeline tasks run) writes all
//! credentials to a shared PVC workspace at /workspace/creds, which is
//! the same workspace the runner step receives here.

use kube::{
    api::{Api, DynamicObject, PostParams},
    discovery::ApiResource,
    Client,
};
use serde_json::json;

const RUNNER_IMAGE_ENV: &str = "RUNNER_IMAGE";
const DEFAULT_RUNNER_IMAGE: &str = "gingersociety/external-executor-runner:latest";

/// Name of the shared workspace volume that init-credentials writes to.
/// Must match what ginger-gitter injects into the Pipeline's workspace list.
const CREDS_WORKSPACE_NAME: &str = "creds";

/// Mount path inside the runner container where the workspace is available.
const CREDS_MOUNT_PATH: &str = "/workspace/creds";

#[derive(Debug, thiserror::Error)]
pub enum TaskRunError {
    #[error("kube: {0}")]
    Kube(#[from] kube::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub struct TaskRunSpec<'a> {
    pub name: &'a str,
    pub ns: &'a str,
    pub owner_api_version: &'a str,
    pub owner_kind: &'a str,
    /// The name of the owning resource (may differ from `name` — e.g. for
    /// CustomRun the TaskRun is named `<customrun>-exec` but the owner is the
    /// CustomRun itself).
    pub owner_name: &'a str,
    pub owner_uid: &'a str,
    pub capability: &'a str,
    pub script: &'a str,
    pub cleanup: Option<&'a str>,
    /// Pre-built serde_json env entries, each `{"name": "...", "value": "..."}`
    /// or `{"name": "...", "valueFrom": {...}}`.
    pub env: Vec<serde_json::Value>,
    pub executor_url: &'a str,
    /// Extra labels to merge onto the TaskRun metadata (e.g. the CustomRun
    /// tracking label so we can find the TaskRun by label later).
    pub extra_labels: std::collections::BTreeMap<String, String>,
    /// Name of the PipelineRun's shared workspace PVC claim, so the runner
    /// TaskRun can bind to the same volume that init-credentials wrote to.
    /// When `None` (e.g. standalone RemoteTask not inside a Pipeline), the
    /// runner falls back to expecting auth.json at the default location on the
    /// node — typically only useful in dev/test scenarios.
    pub creds_workspace_claim: Option<String>,
}

/// Create a Tekton TaskRun that runs the runner image with the given spec.
/// Returns Ok(()) if created or already exists (idempotent).
pub async fn create_taskrun(client: &Client, spec: TaskRunSpec<'_>) -> Result<(), TaskRunError> {
    let runner_image = std::env::var(RUNNER_IMAGE_ENV)
        .unwrap_or_else(|_| DEFAULT_RUNNER_IMAGE.into());

    // Start with user-provided env, then append controller-managed vars so
    // they cannot be accidentally shadowed.
    let mut step_env = spec.env;
    step_env.extend([
        json!({ "name": "REMOTE_SCRIPT",         "value": spec.script }),
        json!({ "name": "REMOTE_CAPABILITY",     "value": spec.capability }),
        json!({ "name": "EXTERNAL_EXECUTOR_URL", "value": spec.executor_url }),
        // Tell the runner where to find auth.json. The runner's entrypoint.sh
        // reads this path instead of the old /var/run/ginger-society location.
        json!({ "name": "GINGER_AUTH_PATH", "value": format!("{CREDS_MOUNT_PATH}/ginger-society/auth.json") }),
    ]);
    if let Some(cleanup) = spec.cleanup {
        step_env.push(json!({ "name": "REMOTE_CLEANUP", "value": cleanup }));
    }

    // Build volume + volumeMount based on whether we have a workspace claim.
    // If we do, bind the runner to the same PVC that init-credentials wrote.
    // If not, mount nothing — the node must already have auth.json in place
    // (dev/test only; production pipelines always have the workspace).
    let (volumes, volume_mounts) = match &spec.creds_workspace_claim {
        Some(claim_name) => {
            let volumes = json!([{
                "name": CREDS_WORKSPACE_NAME,
                "persistentVolumeClaim": {
                    "claimName": claim_name,
                }
            }]);
            let volume_mounts = json!([{
                "name": CREDS_WORKSPACE_NAME,
                "mountPath": CREDS_MOUNT_PATH,
                "readOnly": true,
            }]);
            (volumes, volume_mounts)
        }
        None => (json!([]), json!([])),
    };

    // Merge base labels with any caller-supplied extras.
    let mut labels = std::collections::BTreeMap::from([(
        "app.kubernetes.io/managed-by".to_string(),
        "remote-task-controller".to_string(),
    )]);
    for (k, v) in &spec.extra_labels {
        labels.insert(k.clone(), v.clone());
    }

    let taskrun = json!({
        "apiVersion": "tekton.dev/v1",
        "kind": "TaskRun",
        "metadata": {
            "name": spec.name,
            "namespace": spec.ns,
            "ownerReferences": [{
                "apiVersion": spec.owner_api_version,
                "kind": spec.owner_kind,
                "name": spec.owner_name,
                "uid": spec.owner_uid,
                "controller": true,
                "blockOwnerDeletion": true,
            }],
            "labels": labels,
        },
        "spec": {
            "taskSpec": {
                "steps": [{
                    "name": "run",
                    "image": runner_image,
                    "env": step_env,
                    "volumeMounts": volume_mounts,
                }],
                "volumes": volumes,
            }
        }
    });

    let ar = ApiResource {
        group: "tekton.dev".into(),
        version: "v1".into(),
        api_version: "tekton.dev/v1".into(),
        kind: "TaskRun".into(),
        plural: "taskruns".into(),
    };
    let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), spec.ns, &ar);
    let obj: DynamicObject = serde_json::from_value(taskrun)?;

    match api.create(&PostParams::default(), &obj).await {
        Ok(_) => {
            println!("[taskrun] created TaskRun {}/{}", spec.ns, spec.name);
            Ok(())
        }
        Err(kube::Error::Api(ae)) if ae.code == 409 => {
            println!(
                "[taskrun] TaskRun {}/{} already exists, skipping",
                spec.ns, spec.name
            );
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

pub fn taskrun_api(client: &Client, ns: &str) -> Api<DynamicObject> {
    let ar = ApiResource {
        group: "tekton.dev".into(),
        version: "v1".into(),
        api_version: "tekton.dev/v1".into(),
        kind: "TaskRun".into(),
        plural: "taskruns".into(),
    };
    Api::namespaced_with(client.clone(), ns, &ar)
}