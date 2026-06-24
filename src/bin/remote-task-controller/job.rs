//! Builds the ConfigMap + Secret + Job trio that actually executes a
//! RemoteTask, and maps the resulting Job's status back onto
//! RemoteTaskPhase.
//!
//! This replaces the old in-process dispatch (controller → HTTP → sidekick
//! → SSE) with: controller → Job → pod running `ginger-infra rpc` → HTTP →
//! sidekick → SSE, where the pod's own stdout *is* the job's log output.
//! That gives Tekton (and plain `kubectl logs`) a real pod to attach to,
//! with no Events polling and no log-mirror hack required.

use std::collections::BTreeMap;

use k8s_openapi::api::batch::v1::{Job, JobSpec, JobStatus};
use k8s_openapi::api::core::v1::{
    ConfigMap, Container, EnvVar as K8sEnvVar, PodSpec, PodTemplateSpec, Secret, Volume,
    VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::{
    api::{Api, ObjectMeta, Patch, PatchParams, PostParams},
    Client, ResourceExt,
};
use std::collections::HashMap;

use ginger_infra::remote_task::{RemoteTask, RemoteTaskPhase};

const RUNNER_IMAGE_ENV: &str = "RUNNER_IMAGE";
const DEFAULT_RUNNER_IMAGE: &str = "gingersociety/external-executor-runner:latest";

#[derive(Debug, thiserror::Error)]
pub enum JobError {
    #[error("k8s api error: {0}")]
    Kube(#[from] kube::Error),
}

/// Render a resolved env map into `.envrc`-style `export NAME=VALUE` lines.
/// Values are double-quoted; embedded `"` and `\` are escaped so the file
/// round-trips through parse_envrc on the runner side.
fn render_envrc(env: &HashMap<String, String>) -> String {
    let mut out = String::new();
    let mut keys: Vec<&String> = env.keys().collect();
    keys.sort();
    for k in keys {
        let v = &env[k];
        let escaped = v.replace('\\', "\\\\").replace('"', "\\\"");
        out.push_str(&format!("export {}=\"{}\"\n", k, escaped));
    }
    out
}

fn owner_ref_for(task_name: &str, task_uid: &str) -> OwnerReference {
    OwnerReference {
        api_version: "gingersociety.org/v1alpha1".to_string(),
        kind: "RemoteTask".to_string(),
        name: task_name.to_string(),
        uid: task_uid.to_string(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    }
}

/// Create the Secret holding the rendered `.envrc` for this RemoteTask.
/// A Secret (not a ConfigMap) is used because the env values are typically
/// resolved from other Secrets and we don't want them sitting in plaintext
/// in a ConfigMap.
pub async fn create_envrc_secret(
    client: &Client,
    ns: &str,
    task_name: &str,
    task_uid: &str,
    env: &HashMap<String, String>,
) -> Result<String, JobError> {
    let secret_name = format!("{task_name}-envrc");
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);

    let envrc_contents = render_envrc(env);

    let mut string_data = BTreeMap::new();
    string_data.insert(".envrc".to_string(), envrc_contents);

    let secret = Secret {
        metadata: ObjectMeta {
            name: Some(secret_name.clone()),
            namespace: Some(ns.to_string()),
            owner_references: Some(vec![owner_ref_for(task_name, task_uid)]),
            labels: Some(BTreeMap::from([(
                "remotetask".to_string(),
                task_name.to_string(),
            )])),
            ..Default::default()
        },
        string_data: Some(string_data),
        ..Default::default()
    };

    match secrets.create(&PostParams::default(), &secret).await {
        Ok(_) => {}
        Err(kube::Error::Api(ae)) if ae.code == 409 => {
            // already exists (reconcile retry) — replace contents
            secrets
                .replace(&secret_name, &PostParams::default(), &secret)
                .await?;
        }
        Err(e) => return Err(e.into()),
    }

    Ok(secret_name)
}

/// Create the ConfigMap holding `script.sh` and (optionally) `cleanup.sh`.
pub async fn create_scripts_configmap(
    client: &Client,
    ns: &str,
    task_name: &str,
    task_uid: &str,
    script: &str,
    cleanup: Option<&str>,
) -> Result<String, JobError> {
    let cm_name = format!("{task_name}-scripts");
    let configmaps: Api<ConfigMap> = Api::namespaced(client.clone(), ns);

    let mut data = BTreeMap::new();
    data.insert("script.sh".to_string(), script.to_string());
    if let Some(cleanup) = cleanup {
        data.insert("cleanup.sh".to_string(), cleanup.to_string());
    }

    let cm = ConfigMap {
        metadata: ObjectMeta {
            name: Some(cm_name.clone()),
            namespace: Some(ns.to_string()),
            owner_references: Some(vec![owner_ref_for(task_name, task_uid)]),
            labels: Some(BTreeMap::from([(
                "remotetask".to_string(),
                task_name.to_string(),
            )])),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    };

    match configmaps.create(&PostParams::default(), &cm).await {
        Ok(_) => {}
        Err(kube::Error::Api(ae)) if ae.code == 409 => {
            configmaps
                .replace(&cm_name, &PostParams::default(), &cm)
                .await?;
        }
        Err(e) => return Err(e.into()),
    }

    Ok(cm_name)
}

/// Create the Job that actually runs `ginger-infra rpc` against the mounted
/// script/cleanup/envrc. The pod's stdout is the job's real log output —
/// viewable with `kubectl logs job/<name>` or via Tekton's normal pod-log
/// plumbing, no Events/log-mirror required.
pub async fn create_execution_job(
    client: &Client,
    ns: &str,
    task: &RemoteTask,
    task_uid: &str,
    configmap_name: &str,
    secret_name: &str,
    has_cleanup: bool,
) -> Result<(), JobError> {
    let task_name = task.name_any();
    let jobs: Api<Job> = Api::namespaced(client.clone(), ns);

    if jobs.get_opt(&task_name).await?.is_some() {
        println!("[customrun-controller] execution Job {task_name} already exists, skipping");
        return Ok(());
    }

    let runner_image = std::env::var(RUNNER_IMAGE_ENV).unwrap_or_else(|_| DEFAULT_RUNNER_IMAGE.to_string());

    let mut args = vec![
        "--envrc".to_string(),
        "/config/envrc/.envrc".to_string(),
        "--script".to_string(),
        "/config/scripts/script.sh".to_string(),
        "--capability".to_string(),
        task.spec.capability.clone(),
    ];
    if has_cleanup {
        args.push("--cleanup".to_string());
        args.push("/config/scripts/cleanup.sh".to_string());
    }

    let job = Job {
        metadata: ObjectMeta {
            name: Some(task_name.clone()),
            namespace: Some(ns.to_string()),
            owner_references: Some(vec![owner_ref_for(&task_name, task_uid)]),
            labels: Some(BTreeMap::from([
                ("app".to_string(), "remote-task-runner".to_string()),
                ("remotetask".to_string(), task_name.clone()),
            ])),
            ..Default::default()
        },
        spec: Some(JobSpec {
            // We want a single, observable attempt. If the script fails,
            // the RemoteTask should go Failed — not have Kubernetes silently
            // retry it on our behalf.
            backoff_limit: Some(0),
            ttl_seconds_after_finished: Some(3600),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(BTreeMap::from([
                        ("app".to_string(), "remote-task-runner".to_string()),
                        ("remotetask".to_string(), task_name.clone()),
                    ])),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    restart_policy: Some("Never".to_string()),
                    containers: vec![Container {
                        name: "runner".to_string(),
                        image: Some(runner_image),
                        args: Some(args),
                        env: Some(vec![K8sEnvVar {
                            name: "EXTERNAL_EXECUTOR_URL".to_string(),
                            value: std::env::var("SIDEKICK_URL").ok(),
                            ..Default::default()
                        }]),
                        volume_mounts: Some(vec![
                            VolumeMount {
                                name: "scripts".to_string(),
                                mount_path: "/config/scripts".to_string(),
                                read_only: Some(true),
                                ..Default::default()
                            },
                            VolumeMount {
                                name: "envrc".to_string(),
                                mount_path: "/config/envrc".to_string(),
                                read_only: Some(true),
                                ..Default::default()
                            },
                        ]),
                        ..Default::default()
                    }],
                    volumes: Some(vec![
                        Volume {
                            name: "scripts".to_string(),
                            config_map: Some(k8s_openapi::api::core::v1::ConfigMapVolumeSource {
                                name: configmap_name.to_string(),
                                default_mode: Some(0o555),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                        Volume {
                            name: "envrc".to_string(),
                            secret: Some(k8s_openapi::api::core::v1::SecretVolumeSource {
                                secret_name: Some(secret_name.to_string()),
                                default_mode: Some(0o440),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        status: None,
    };

    jobs.create(&PostParams::default(), &job).await?;
    println!("[customrun-controller] created execution Job {ns}/{task_name}");
    Ok(())
}

/// Outcome of inspecting a Job's status, in terms the RemoteTask
/// reconciler cares about.
pub enum JobOutcome {
    /// Job exists but hasn't reached a terminal state yet.
    Running,
    /// Job's pod(s) completed successfully.
    Succeeded,
    /// Job failed (backoff_limit exhausted or pod errored).
    Failed { message: String },
}

/// Inspect a Job's status and classify it. Returns `None` if the Job has no
/// status yet (just created, not yet picked up by the job controller).
pub fn classify_job_status(status: Option<&JobStatus>) -> Option<JobOutcome> {
    let status = status?;

    if status.succeeded.unwrap_or(0) > 0 {
        return Some(JobOutcome::Succeeded);
    }

    if status.failed.unwrap_or(0) > 0 {
        let message = status
            .conditions
            .as_ref()
            .and_then(|conds| conds.iter().find(|c| c.type_ == "Failed"))
            .and_then(|c| c.message.clone())
            .unwrap_or_else(|| "execution Job failed".to_string());
        return Some(JobOutcome::Failed { message });
    }

    Some(JobOutcome::Running)
}

/// Best-effort mapping from JobOutcome to RemoteTaskPhase, for callers that
/// just want the phase without the failure message.
pub fn phase_for_outcome(outcome: &JobOutcome) -> RemoteTaskPhase {
    match outcome {
        JobOutcome::Running => RemoteTaskPhase::Running,
        JobOutcome::Succeeded => RemoteTaskPhase::Succeeded,
        JobOutcome::Failed { .. } => RemoteTaskPhase::Failed,
    }
}