use std::collections::{BTreeMap, HashMap};

use futures_util::StreamExt;
use k8s_openapi::api::core::v1::{ConfigMap, ObjectReference, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::{
    api::{Api, ObjectMeta, Patch, PatchParams, PostParams},
    Client,
};
use serde::Serialize;
use serde_json::{json, Value};

use ginger_infra::remote_task::{
    RemoteTask, RemoteTaskCondition, RemoteTaskPhase, RemoteTaskStatus,
};

use crate::events::emit_event;

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
    #[error("sidekick request failed: {0}")]
    SidekickRequest(String),
    #[error("job error: {0}")]
    JobError(String),
}

#[derive(Debug, Serialize)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Serialize)]
pub struct RunJobRequest {
    pub capability: String,
    pub script: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleanup_script: Option<String>,
    pub env: Vec<EnvVar>,
}

pub fn now() -> Time {
    Time(k8s_openapi::jiff::Timestamp::now())
}

pub async fn resolve_env(
    client: &Client,
    ns: &str,
    env_specs: &[ginger_infra::remote_task::RemoteTaskEnvVar],
) -> Result<HashMap<String, String>, DispatchError> {
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
                resolved.insert(env.name.clone(), String::from_utf8_lossy(&bytes.0).to_string());
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

pub async fn append_log_to_configmap(
    client: &Client,
    ns: &str,
    name: &str,
    line: &str,
) -> Result<(), kube::Error> {
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), ns);

    let existing = cms.get_opt(name).await?;
    let mut existing_logs = existing
        .as_ref()
        .and_then(|cm| cm.data.as_ref())
        .and_then(|d| d.get("logs"))
        .cloned()
        .unwrap_or_default();

    existing_logs.push_str(line);
    existing_logs.push('\n');

    if existing.is_none() {
        let cm = ConfigMap {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some(ns.to_string()),
                labels: Some(BTreeMap::from([
                    ("app".to_string(), "remote-task-logs".to_string()),
                    ("remotetask".to_string(), name.to_string()),
                ])),
                ..Default::default()
            },
            data: Some(BTreeMap::from([("logs".to_string(), existing_logs)])),
            ..Default::default()
        };
        cms.create(&PostParams::default(), &cm).await?;
    } else {
        let patch = json!({ "data": { "logs": existing_logs } });
        cms.patch(name, &PatchParams::default(), &Patch::Merge(&patch)).await?;
    }

    Ok(())
}

pub async fn delete_log_configmap(client: &Client, ns: &str, name: &str) {
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), ns);
    if let Err(e) = cms.delete(name, &Default::default()).await {
        eprintln!("[tekton-controller] failed to delete log configmap {ns}/{name}: {e}");
    }
}

pub async fn stream_job(
    client: &Client,
    http: &reqwest::Client,
    url: &str,
    request: &RunJobRequest,
    ns: &str,
    task_ref: &ObjectReference,
    customrun_ref: Option<&ObjectReference>,
) -> Result<i32, DispatchError> {
    let response = http
        .post(url)
        .header("Content-Type", "application/json")
        .json(request)
        .send()
        .await
        .map_err(|e| DispatchError::SidekickRequest(format!("request to '{url}' failed: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(DispatchError::SidekickRequest(format!(
            "sidekick returned {status}: {body}"
        )));
    }

    let task_name = task_ref.name.as_deref().unwrap_or("");
    let mut stream = response.bytes_stream();
    let mut buf = String::new();
    let mut milestone_event_sent = false;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| DispatchError::SidekickRequest(format!("stream read error: {e}")))?;
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
                    let stream_name = event
                        .get("stream").and_then(|v| v.as_str()).unwrap_or("stdout");
                    let line_text = event.get("line").and_then(|v| v.as_str()).unwrap_or("");

                    println!("[remote-task:{stream_name}] {line_text}");

                    let log_line = format!("[{stream_name}] {line_text}");

                    if let Err(e) = append_log_to_configmap(client, ns, task_name, &log_line).await {
                        eprintln!("[tekton-controller] failed to append log to configmap: {e}");
                    }

                    if !milestone_event_sent {
                        if let Some(cr_ref) = customrun_ref {
                            if let Err(e) = emit_event(
                                client, ns, cr_ref, "Normal", "RemoteTaskLog",
                                &format!("streaming — kubectl logs {task_name} -n {ns}"),
                            ).await {
                                eprintln!("[tekton-controller] failed to emit milestone event: {e}");
                            }
                            milestone_event_sent = true;
                        }
                    }
                }
                "done" => {
                    let exit_code = event
                        .get("exit_code").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                    return Ok(exit_code);
                }
                "error" => {
                    let message = event
                        .get("message").and_then(|v| v.as_str()).unwrap_or("unknown error");
                    return Err(DispatchError::JobError(message.to_string()));
                }
                _ => {
                    println!("[tekton-controller] unrecognized event: {data}");
                }
            }
        }
    }

    Err(DispatchError::JobError(
        "stream ended before a 'done' or 'error' event was received".to_string(),
    ))
}

pub async fn run_dispatch(
    client: Client,
    http: reqwest::Client,
    sidekick_url: String,
    ns: String,
    name: String,
    task: RemoteTask,
    task_ref: ObjectReference,
    customrun_ref: Option<ObjectReference>,
) {
    let env_map = match resolve_env(&client, &ns, &task.spec.env).await {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[tekton-controller] failed to resolve env for {ns}/{name}: {e}");
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
        env: env_map
            .into_iter()
            .map(|(name, value)| EnvVar { name, value })
            .collect(),
    };

    let result = stream_job(
        &client, &http, &sidekick_url, &request, &ns, &task_ref, customrun_ref.as_ref(),
    ).await;

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
                    message: Some(format!("Script completed successfully (exit {exit_code})")),
                }],
                ..Default::default()
            }
        }
        Err(e) => {
            eprintln!("[tekton-controller] {ns}/{name} failed: {e}");
            RemoteTaskStatus {
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
            }
        }
    };

    if let Err(e) = set_remote_task_status(&client, &ns, &name, final_status).await {
        eprintln!("[tekton-controller] failed to set final status for {ns}/{name}: {e}");
    }

    delete_log_configmap(&client, &ns, &name).await;
}