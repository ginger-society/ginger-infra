//! Shared helpers used by the CustomRun and RemoteTask reconcilers.
//!
//! This module used to also own the in-process dispatch to the sidekick
//! (HTTP POST + SSE streaming) so the controller itself executed jobs. That
//! path has been removed: execution now happens in a Kubernetes Job running
//! the `external-executor-runner` image (see job.rs), so the controller's
//! job is purely to resolve env vars (so they can be written into the
//! Job's mounted `.envrc`) and to patch RemoteTask status based on what it
//! observes — never to run anything itself. This makes the controller much
//! easier to reason about and debug: it watches and reflects state, it
//! doesn't do work.

use std::collections::HashMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::{
    api::{Api, Patch, PatchParams},
    Client,
};
use serde_json::json;

use ginger_infra::remote_task::{RemoteTask, RemoteTaskStatus};

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("k8s api error: {0}")]
    Kube(#[from] kube::Error),
    #[error("secret '{secret}' key '{key}' not found in namespace '{ns}'")]
    SecretKeyNotFound {
        secret: String,
        key: String,
        ns: String,
    },
}

pub fn now() -> Time {
    Time(k8s_openapi::jiff::Timestamp::now())
}

/// Resolve a RemoteTask's env spec (literal values + secretKeyRef lookups)
/// into a flat name → value map. The caller writes this into the `.envrc`
/// mounted into the execution Job — see job.rs::render_envrc.
pub async fn resolve_env(
    client: &Client,
    ns: &str,
    env_specs: &[ginger_infra::remote_task::RemoteTaskEnvVar],
) -> Result<HashMap<String, String>, DispatchError> {
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
                let data =
                    secret
                        .data
                        .ok_or_else(|| DispatchError::SecretKeyNotFound {
                            secret: secret_ref.name.clone(),
                            key: secret_ref.key.clone(),
                            ns: ns.to_string(),
                        })?;
                let bytes =
                    data.get(&secret_ref.key)
                        .ok_or_else(|| DispatchError::SecretKeyNotFound {
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

pub async fn set_remote_task_status(
    client: &Client,
    ns: &str,
    name: &str,
    status: RemoteTaskStatus,
) -> Result<(), DispatchError> {
    let api: Api<RemoteTask> = Api::namespaced(client.clone(), ns);
    let patch = json!({ "status": status });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}