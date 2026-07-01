use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use clap::Parser;
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    Container, EnvVar, PersistentVolumeClaim, PersistentVolumeClaimVolumeSource, Pod, PodSpec,
    PodTemplateSpec, Secret, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, DeleteParams, ListParams, LogParams, PostParams, PropagationPolicy};
use kube::Client;
use tokio_cron_scheduler::{Job as CronJob, JobScheduler};

const ANNOTATION_ENABLED: &str = "snapshot.gingersociety.org/enabled";
const ANNOTATION_SELECTED_NODE: &str = "volume.kubernetes.io/selected-node";
const NAMESPACE_FILE: &str = "/var/run/secrets/kubernetes.io/serviceaccount/namespace";

#[derive(Parser, Debug, Clone)]
#[command(name = "restic-snapshot-controller")]
struct Args {
    /// Base S3 location, e.g. "my-bucket" or "my-bucket/backups"
    #[arg(long, env = "S3_BASE_PATH")]
    s3_base_path: String,

    /// Restic image used for backup Jobs
    #[arg(long, env = "RESTIC_IMAGE", default_value = "restic/restic:0.16.4")]
    restic_image: String,

    /// Secret (in the controller's own namespace) holding
    /// AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, RESTIC_PASSWORD
    #[arg(long, env = "CREDENTIALS_SECRET_NAME", default_value = "s3-credentials")]
    credentials_secret_name: String,

    /// Overrides auto-detected controller namespace
    #[arg(long, env = "CONTROLLER_NAMESPACE")]
    controller_namespace: Option<String>,

    /// Restrict to a single namespace; unset = all namespaces
    #[arg(long, env = "TARGET_NAMESPACE")]
    target_namespace: Option<String>,

    /// Cron schedule (seconds field included, per tokio-cron-scheduler)
    #[arg(long, env = "CRON_SCHEDULE", default_value = "0 0 * * * *")]
    schedule: String,

    #[arg(long, env = "KEEP_HOURLY", default_value_t = 5)]
    keep_hourly: u32,
    #[arg(long, env = "KEEP_WEEKLY", default_value_t = 4)]
    keep_weekly: u32,
    #[arg(long, env = "KEEP_MONTHLY", default_value_t = 6)]
    keep_monthly: u32,

    /// Run a single sweep immediately and exit (for manual testing)
    #[arg(long, default_value_t = false)]
    run_once: bool,
}

#[derive(Clone)]
struct Credentials {
    access_key: String,
    secret_key: String,
    restic_password: String,
}

fn resolve_controller_namespace(args: &Args) -> String {
    if let Some(ns) = &args.controller_namespace {
        return ns.clone();
    }
    std::fs::read_to_string(NAMESPACE_FILE)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "default".to_string())
}

async fn fetch_credentials(client: &Client, ns: &str, secret_name: &str) -> Result<Credentials> {
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    let secret = secrets
        .get(secret_name)
        .await
        .with_context(|| format!("fetching secret {ns}/{secret_name}"))?;
    let data = secret
        .data
        .ok_or_else(|| anyhow!("secret {ns}/{secret_name} has no data"))?;

    let get = |key: &str| -> Result<String> {
        let bytes = data
            .get(key)
            .ok_or_else(|| anyhow!("secret {ns}/{secret_name} missing key {key}"))?;
        Ok(String::from_utf8(bytes.0.clone())?)
    };

    Ok(Credentials {
        access_key: get("AWS_ACCESS_KEY_ID")?,
        secret_key: get("AWS_SECRET_ACCESS_KEY")?,
        restic_password: get("RESTIC_PASSWORD")?,
    })
}

fn is_rwo(pvc: &PersistentVolumeClaim) -> bool {
    pvc.spec
        .as_ref()
        .and_then(|s| s.access_modes.as_ref())
        .map(|modes| modes.iter().any(|m| m == "ReadWriteOnce"))
        .unwrap_or(false)
}

fn selected_node(pvc: &PersistentVolumeClaim) -> Option<String> {
    pvc.metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(ANNOTATION_SELECTED_NODE))
        .cloned()
}

async fn list_target_pvcs(
    client: &Client,
    ns_filter: Option<&str>,
) -> Result<Vec<PersistentVolumeClaim>> {
    let api: Api<PersistentVolumeClaim> = match ns_filter {
        Some(ns) => Api::namespaced(client.clone(), ns),
        None => Api::all(client.clone()),
    };
    let list = api.list(&ListParams::default()).await?;
    Ok(list
        .items
        .into_iter()
        .filter(|p| {
            p.metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(ANNOTATION_ENABLED))
                .map(|v| v == "true")
                .unwrap_or(false)
        })
        .collect())
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '-' })
        .collect()
}

fn build_backup_job(
    pvc_name: &str,
    namespace: &str,
    run_id: &str,
    args: &Args,
    repo: &str,
    creds: &Credentials,
    node: Option<&str>,
) -> Job {
    let job_name = format!("restic-backup-{}-{}", sanitize(pvc_name), run_id);

    let script = format!(
        "set -eu\n\
         if ! restic snapshots >/dev/null 2>&1; then restic init; fi\n\
         restic backup /volumes-to-backup/{pvc} --tag {pvc}\n\
         restic forget --keep-hourly {kh} --keep-weekly {kw} --keep-monthly {km} --prune\n",
        pvc = pvc_name,
        kh = args.keep_hourly,
        kw = args.keep_weekly,
        km = args.keep_monthly,
    );

    let mut labels = BTreeMap::new();
    labels.insert(
        "app.kubernetes.io/managed-by".to_string(),
        "restic-snapshot-controller".to_string(),
    );
    labels.insert("snapshot.gingersociety.org/pvc".to_string(), pvc_name.to_string());
    labels.insert("snapshot.gingersociety.org/run-id".to_string(), run_id.to_string());

    let env = vec![
        EnvVar { name: "RESTIC_REPOSITORY".into(), value: Some(repo.into()), ..Default::default() },
        EnvVar { name: "RESTIC_PASSWORD".into(), value: Some(creds.restic_password.clone()), ..Default::default() },
        EnvVar { name: "AWS_ACCESS_KEY_ID".into(), value: Some(creds.access_key.clone()), ..Default::default() },
        EnvVar { name: "AWS_SECRET_ACCESS_KEY".into(), value: Some(creds.secret_key.clone()), ..Default::default() },
    ];

    let node_selector = node.map(|n| {
        let mut m = BTreeMap::new();
        m.insert("kubernetes.io/hostname".to_string(), n.to_string());
        m
    });

    Job {
        metadata: ObjectMeta {
            name: Some(job_name),
            namespace: Some(namespace.to_string()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(JobSpec {
            backoff_limit: Some(0),
            ttl_seconds_after_finished: Some(300),
            template: PodTemplateSpec {
                metadata: None,
                spec: Some(PodSpec {
                    restart_policy: Some("Never".into()),
                    node_selector,
                    containers: vec![Container {
                        name: "restic-backup".into(),
                        image: Some(args.restic_image.clone()),
                        command: Some(vec!["/bin/sh".into(), "-c".into(), script]),
                        env: Some(env),
                        volume_mounts: Some(vec![VolumeMount {
                            name: "data".into(),
                            mount_path: format!("/volumes-to-backup/{pvc_name}"),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    }],
                    volumes: Some(vec![Volume {
                        name: "data".into(),
                        persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                            claim_name: pvc_name.to_string(),
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
    }
}

async fn wait_for_job(jobs: &Api<Job>, name: &str) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30 * 60);
    loop {
        let j = jobs.get(name).await?;
        if let Some(status) = j.status {
            if status.succeeded.unwrap_or(0) > 0 {
                return Ok(true);
            }
            if status.failed.unwrap_or(0) > 0 {
                return Ok(false);
            }
        }
        if tokio::time::Instant::now() > deadline {
            return Err(anyhow!("timed out waiting for job {name}"));
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

async fn print_job_logs(client: &Client, ns: &str, job_name: &str) {
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    let lp = ListParams::default().labels(&format!("job-name={job_name}"));
    match pods.list(&lp).await {
        Ok(list) => {
            for pod in list.items {
                let pod_name = pod.metadata.name.clone().unwrap_or_default();
                println!("── logs: job={job_name} pod={pod_name} ──");
                match pods.logs(&pod_name, &LogParams::default()).await {
                    Ok(logs) => println!("{logs}"),
                    Err(e) => eprintln!("[restic-controller] could not fetch logs for {pod_name}: {e}"),
                }
            }
        }
        Err(e) => eprintln!("[restic-controller] could not list pods for job {job_name}: {e}"),
    }
}

async fn run_backup_job(client: &Client, job: Job) -> Result<bool> {
    let ns = job.metadata.namespace.clone().unwrap();
    let name = job.metadata.name.clone().unwrap();
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);

    jobs.create(&PostParams::default(), &job).await?;
    let result = wait_for_job(&jobs, &name).await;

    // Print logs regardless of outcome, before cleanup — this is our audit trail.
    print_job_logs(client, &ns, &name).await;

    let dp = DeleteParams {
        propagation_policy: Some(PropagationPolicy::Background),
        ..Default::default()
    };
    if let Err(e) = jobs.delete(&name, &dp).await {
        eprintln!("[restic-controller] warning: failed to delete job {ns}/{name}: {e}");
    }

    result
}

async fn run_backup_sweep(client: Client, args: Arc<Args>) {
    let run_id = Utc::now().format("%Y%m%d%H%M%S").to_string();
    println!("[restic-controller] === sweep start run_id={run_id} ===");

    let controller_ns = resolve_controller_namespace(&args);
    let creds = match fetch_credentials(&client, &controller_ns, &args.credentials_secret_name).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[restic-controller] failed to load credentials from {controller_ns}/{}: {e}", args.credentials_secret_name);
            return;
        }
    };

    let pvcs = match list_target_pvcs(&client, args.target_namespace.as_deref()).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[restic-controller] failed to list PVCs: {e}");
            return;
        }
    };

    if pvcs.is_empty() {
        println!("[restic-controller] no PVCs annotated {ANNOTATION_ENABLED}=true found; nothing to do");
        return;
    }

    let mut succeeded = Vec::new();
    let mut failed = Vec::new();

    for pvc in pvcs {
        let name = pvc.metadata.name.clone().unwrap_or_default();
        let ns = pvc.metadata.namespace.clone().unwrap_or_default();
        let label = format!("{ns}/{name}");

        let node = if is_rwo(&pvc) {
            match selected_node(&pvc) {
                Some(n) => Some(n),
                None => {
                    eprintln!(
                        "[restic-controller] skipping {label}: RWO with no {ANNOTATION_SELECTED_NODE} annotation, cannot safely place backup pod"
                    );
                    failed.push(label);
                    continue;
                }
            }
        } else {
            None
        };

        let repo = format!("s3:https://s3.amazonaws.com/{}/{}/{}", args.s3_base_path, ns, name);
        let job = build_backup_job(&name, &ns, &run_id, &args, &repo, &creds, node.as_deref());

        println!("[restic-controller] dispatching backup job for {label}");
        match run_backup_job(&client, job).await {
            Ok(true) => {
                println!("[restic-controller] {label}: SUCCESS");
                succeeded.push(label);
            }
            Ok(false) => {
                eprintln!("[restic-controller] {label}: FAILED (job reported failure)");
                failed.push(label);
            }
            Err(e) => {
                eprintln!("[restic-controller] {label}: ERROR dispatching job: {e}");
                failed.push(label);
            }
        }
    }

    println!(
        "[restic-controller] === sweep complete run_id={run_id}: {} succeeded, {} failed{} ===",
        succeeded.len(),
        failed.len(),
        if failed.is_empty() { String::new() } else { format!(" -> {}", failed.join(", ")) }
    );
}

async fn setup_scheduler(client: Client, args: Arc<Args>) -> Result<JobScheduler> {
    let sched = JobScheduler::new().await?;
    let client_c = client.clone();
    let args_c = args.clone();

    let cron_job = CronJob::new_async(args.schedule.as_str(), move |_uuid, _l| {
        let client = client_c.clone();
        let args = args_c.clone();
        Box::pin(async move {
            run_backup_sweep(client, args).await;
        })
    })?;

    sched.add(cron_job).await?;
    sched.start().await?;
    Ok(sched)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Arc::new(Args::parse());
    let client = Client::try_default().await?;

    if args.run_once {
        run_backup_sweep(client, args).await;
        return Ok(());
    }

    println!("[restic-controller] scheduling backup sweep with cron '{}'", args.schedule);
    let _sched = setup_scheduler(client, args.clone()).await?;

    tokio::signal::ctrl_c().await?;
    println!("[restic-controller] shutting down");
    Ok(())
}