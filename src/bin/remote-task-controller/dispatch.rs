use std::collections::HashMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use k8s_openapi::api::core::v1::Secret;
use kube::{
    api::{Api, Patch, PatchParams},
    Client,
};
use serde_json::json;

use ginger_infra::remote_task::{
    RemoteTask, RemoteTaskCondition, RemoteTaskPhase, RemoteTaskStatus,
};

use crate::rpc::{ stream_rpc_job, EnvVar, RunJobRequest};

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
    #[error("rpc error: {0}")]
    Rpc(String),
}

pub fn now() -> Time {
    Time(k8s_openapi::jiff::Timestamp::now())
}

pub async fn resolve_env(
    client: &Client,
    ns: &str,
    env_specs: &[ginger_infra::remote_task::RemoteTaskEnvVar],
) -> Result<Vec<EnvVar>, DispatchError> {
    let secrets_api: Api<Secret> = Api::namespaced(client.clone(), ns);
    let mut resolved = Vec::new();

    for env in env_specs {
        if let Some(value) = &env.value {
            resolved.push(EnvVar { name: env.name.clone(), value: value.clone() });
            continue;
        }

        if let Some(from) = &env.value_from {
            if let Some(secret_ref) = &from.secret_key_ref {
                let secret = secrets_api.get(&secret_ref.name).await?;
                let data = secret.data.ok_or_else(|| DispatchError::SecretKeyNotFound {
                    secret: secret_ref.name.clone(),
                    key: secret_ref.key.clone(),
                    ns: ns.to_string(),
                })?;
                let bytes = data.get(&secret_ref.key).ok_or_else(|| DispatchError::SecretKeyNotFound {
                    secret: secret_ref.name.clone(),
                    key: secret_ref.key.clone(),
                    ns: ns.to_string(),
                })?;
                resolved.push(EnvVar {
                    name: env.name.clone(),
                    value: String::from_utf8_lossy(&bytes.0).to_string(),
                });
                continue;
            }
        }

        eprintln!("[tekton-controller] env '{}' has no supported source — skipping", env.name);
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
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch)).await?;
    Ok(())
}

pub async fn run_dispatch(
    client: Client,
    sidekick_url: String,
    ns: String,
    name: String,
    task: RemoteTask,
) {
    let env = match resolve_env(&client, &ns, &task.spec.env).await {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[tekton-controller] env resolution failed for {ns}/{name}: {e}");
            let _ = set_remote_task_status(
                &client, &ns, &name,
                RemoteTaskStatus {
                    phase: RemoteTaskPhase::Failed,
                    completion_time: Some(now()),
                    message: Some(format!("env resolution failed: {e}")),
                    ..Default::default()
                },
            ).await;
            return;
        }
    };

    if let Err(e) = set_remote_task_status(
        &client, &ns, &name,
        RemoteTaskStatus {
            phase: RemoteTaskPhase::Running,
            start_time: Some(now()),
            ..Default::default()
        },
    ).await {
        eprintln!("[tekton-controller] failed to set Running for {ns}/{name}: {e}");
        return;
    }

    let request = RunJobRequest {
        capability: task.spec.capability.clone(),
        script: task.spec.script.clone(),
        cleanup_script: task.spec.cleanup.clone(),
        env,
    };

    println!("[tekton-controller] {ns}/{name} dispatching via rpc to {sidekick_url}");

    let result = stream_rpc_job(&sidekick_url, &request).await;

    let final_status = match result {
        Ok(exit_code) => {
            println!("[tekton-controller] {ns}/{name} succeeded (exit {exit_code})");
            RemoteTaskStatus {
                phase: RemoteTaskPhase::Succeeded,
                exit_code: Some(exit_code),
                completion_time: Some(now()),
                conditions: vec![RemoteTaskCondition {
                    type_: "Succeeded".to_string(),
                    status: "True".to_string(),
                    reason: Some("ExitCodeZero".to_string()),
                    message: Some(format!("script completed (exit {exit_code})")),
                }],
                ..Default::default()
            }
        }
        Err(e) => {
            eprintln!("[tekton-controller] {ns}/{name} failed: {e}");
            RemoteTaskStatus {
                phase: RemoteTaskPhase::Failed,
                completion_time: Some(now()),
                message: Some(e.clone()),
                conditions: vec![RemoteTaskCondition {
                    type_: "Succeeded".to_string(),
                    status: "False".to_string(),
                    reason: Some("Error".to_string()),
                    message: Some(e),
                }],
                ..Default::default()
            }
        }
    };

    if let Err(e) = set_remote_task_status(&client, &ns, &name, final_status).await {
        eprintln!("[tekton-controller] failed to set final status for {ns}/{name}: {e}");
    }
}