//! Shared TaskRun builder used by both the CustomRun and RemoteTask reconcilers.
//!
//! The runner image's entrypoint.sh:
//!   1. Copies auth.json from the mounted Secret to ~/.ginger-society/auth.json
//!   2. Writes REMOTE_SCRIPT / REMOTE_CLEANUP from env to /tmp
//!   3. Calls: ginger-infra rpc --envrc /dev/null --script /tmp/script.sh …

use kube::{
    api::{Api, DynamicObject, PostParams},
    discovery::ApiResource,
    Client, ResourceExt,
};
use serde_json::json;

const RUNNER_IMAGE_ENV: &str = "RUNNER_IMAGE";
const DEFAULT_RUNNER_IMAGE: &str = "gingersociety/external-executor-runner:latest";

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
    pub owner_uid: &'a str,
    pub capability: &'a str,
    pub script: &'a str,
    pub cleanup: Option<&'a str>,
    /// Pre-built serde_json env entries, each `{"name": "...", "value": "..."}`
    /// or `{"name": "...", "valueFrom": {...}}`.
    pub env: Vec<serde_json::Value>,
    pub sidekick_url: &'a str,
    pub auth_secret_name: &'a str,
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
        json!({ "name": "EXTERNAL_EXECUTOR_URL", "value": spec.sidekick_url }),
    ]);
    if let Some(cleanup) = spec.cleanup {
        step_env.push(json!({ "name": "REMOTE_CLEANUP", "value": cleanup }));
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
                "name": spec.name,
                "uid": spec.owner_uid,
                "controller": true,
                "blockOwnerDeletion": true,
            }],
            "labels": {
                "app.kubernetes.io/managed-by": "remote-task-controller",
            }
        },
        "spec": {
            "taskSpec": {
                "steps": [{
                    "name": "run",
                    "image": runner_image,
                    "env": step_env,
                    // Mount the auth secret so entrypoint.sh can copy it to
                    // ~/.ginger-society/auth.json before calling ginger-infra.
                    "volumeMounts": [{
                        "name": "ginger-auth",
                        "mountPath": "/var/run/ginger-society",
                        "readOnly": true,
                    }]
                }],
                "volumes": [{
                    "name": "ginger-auth",
                    "secret": {
                        "secretName": spec.auth_secret_name,
                    }
                }]
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
            println!(
                "[taskrun] created TaskRun {}/{}",
                spec.ns, spec.name
            );
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