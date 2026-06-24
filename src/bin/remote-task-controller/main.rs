use std::sync::Arc;

use futures_util::StreamExt;
use k8s_openapi::api::batch::v1::Job;
use kube::{
    api::{Api, DynamicObject},
    discovery::ApiResource,
    runtime::{controller::Action, watcher, Controller},
    Client, ResourceExt,
};

use ginger_infra::remote_task::{RemoteTask, RemoteTaskPhase, RemoteTaskStatus};

mod customrun;
mod dispatch;
mod events;
mod job;

use customrun::{
    customrun_error_policy, reconcile_customrun, CustomRunContext, CUSTOMRUN_GROUP,
    CUSTOMRUN_KIND, CUSTOMRUN_PLURAL, CUSTOMRUN_VERSION,
};
use dispatch::{now, set_remote_task_status};
use job::{classify_job_status, phase_for_outcome, JobOutcome};

const REQUEUE_AFTER_ERROR_SECS: u64 = 30;
const REQUEUE_WHILE_RUNNING_SECS: u64 = 5;

#[derive(Debug, thiserror::Error)]
enum ReconcileError {
    #[error("k8s api error: {0}")]
    Kube(#[from] kube::Error),
    #[error("dispatch error: {0}")]
    Dispatch(#[from] dispatch::DispatchError),
}

struct ControllerContext {
    client: Client,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("[tekton-controller] starting...");

    // SIDEKICK_URL is still required — it's read by job.rs when building the
    // execution Job's pod spec (EXTERNAL_EXECUTOR_URL env var for the
    // `ginger-infra rpc` runner container), not used by the controller
    // process itself.
    let sidekick_url = std::env::var("SIDEKICK_URL")
        .map_err(|_| anyhow::anyhow!("SIDEKICK_URL env var is required"))?;
    println!("[tekton-controller] sidekick_url={} (passed to execution Jobs)", sidekick_url);

    let client = Client::try_default().await?;

    let remote_tasks: Api<RemoteTask> = Api::all(client.clone());
    let ctx = Arc::new(ControllerContext {
        client: client.clone(),
    });

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

    println!("[tekton-controller] watching RemoteTask + owned Jobs across all namespaces");
    println!(
        "[tekton-controller] watching CustomRun (tekton.dev/v1beta1) across all namespaces"
    );

    // RemoteTask controller — watches the execution Job it owns (created by
    // the CustomRun reconciler) and mirrors that Job's status onto
    // RemoteTask.status. This is the only place RemoteTask status is
    // written; the controller does no execution of its own.
    let remote_task_controller = Controller::new(remote_tasks, watcher::Config::default())
        .owns(Api::<Job>::all(client.clone()), watcher::Config::default())
        .run(reconcile_remote_task, error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok(o) => println!("[tekton-controller] reconciled remotetask {:?}", o),
                Err(e) => {
                    eprintln!("[tekton-controller] remotetask reconcile error: {:?}", e)
                }
            }
        });

    let customrun_controller = Controller::new_with(
        customruns,
        watcher::Config::default(),
        customrun_ar.clone(),
    )
    .watches(
        Api::<RemoteTask>::all(client.clone()),
        watcher::Config::default(),
        move |rt| {
            let owner = rt
                .metadata
                .owner_references
                .as_ref()?
                .iter()
                .find(|o| o.kind == "CustomRun")?;

            Some(
                kube::runtime::reflector::ObjectRef::<DynamicObject>::new_with(
                    &owner.name,
                    ApiResource {
                        group: CUSTOMRUN_GROUP.to_string(),
                        version: CUSTOMRUN_VERSION.to_string(),
                        api_version: format!("{CUSTOMRUN_GROUP}/{CUSTOMRUN_VERSION}"),
                        kind: CUSTOMRUN_KIND.to_string(),
                        plural: CUSTOMRUN_PLURAL.to_string(),
                    },
                )
                .within(
                    rt.metadata
                        .namespace
                        .as_deref()
                        .unwrap_or("default"),
                ),
            )
        },
    )
    .run(reconcile_customrun, customrun_error_policy, customrun_ctx)
    .for_each(|res| async move {
        match res {
            Ok(o) => println!("[tekton-controller] reconciled customrun {:?}", o),
            Err(e) => {
                eprintln!("[tekton-controller] customrun reconcile error: {:?}", e)
            }
        }
    });

    tokio::join!(remote_task_controller, customrun_controller);

    Ok(())
}

fn error_policy(
    _task: Arc<RemoteTask>,
    err: &ReconcileError,
    _ctx: Arc<ControllerContext>,
) -> Action {
    eprintln!("[tekton-controller] error_policy: {:?}", err);
    Action::requeue(std::time::Duration::from_secs(REQUEUE_AFTER_ERROR_SECS))
}

/// RemoteTask reconciler — looks up the execution Job (same name as the
/// RemoteTask, created by the CustomRun reconciler), classifies its status,
/// and patches RemoteTask.status to match. Pure observation: this function
/// never runs a script and never calls the sidekick.
async fn reconcile_remote_task(
    task: Arc<RemoteTask>,
    ctx: Arc<ControllerContext>,
) -> Result<Action, ReconcileError> {
    let name = task.name_any();
    let ns = task.namespace().unwrap_or_else(|| "default".to_string());

    let current_phase = task
        .status
        .as_ref()
        .map(|s| s.phase.clone())
        .unwrap_or_default();

    // Already terminal — nothing more to observe.
    if matches!(
        current_phase,
        RemoteTaskPhase::Succeeded | RemoteTaskPhase::Failed
    ) {
        return Ok(Action::await_change());
    }

    let jobs: Api<Job> = Api::namespaced(ctx.client.clone(), &ns);
    let job = match jobs.get_opt(&name).await? {
        Some(j) => j,
        None => {
            // CustomRun reconciler hasn't created the Job yet (or it's been
            // deleted out from under us). Nothing to do but wait.
            println!(
                "[tekton-controller] {}/{} — no execution Job yet, waiting",
                ns, name
            );
            return Ok(Action::requeue(std::time::Duration::from_secs(
                REQUEUE_WHILE_RUNNING_SECS,
            )));
        }
    };

    let outcome = match classify_job_status(job.status.as_ref()) {
        Some(o) => o,
        None => {
            println!(
                "[tekton-controller] {}/{} — Job exists, no status yet",
                ns, name
            );
            return Ok(Action::requeue(std::time::Duration::from_secs(
                REQUEUE_WHILE_RUNNING_SECS,
            )));
        }
    };

    let new_phase = phase_for_outcome(&outcome);

    println!(
        "[tekton-controller] {}/{} phase={:?} → {:?}",
        ns, name, current_phase, new_phase
    );

    match outcome {
        JobOutcome::Running => {
            if current_phase != RemoteTaskPhase::Running {
                set_remote_task_status(
                    &ctx.client,
                    &ns,
                    &name,
                    RemoteTaskStatus {
                        phase: RemoteTaskPhase::Running,
                        start_time: Some(now()),
                        ..Default::default()
                    },
                )
                .await?;
            }
            Ok(Action::requeue(std::time::Duration::from_secs(
                REQUEUE_WHILE_RUNNING_SECS,
            )))
        }
        JobOutcome::Succeeded => {
            // Job ran to completion successfully. We don't have a per-line
            // exit code from the script itself (the runner pod's own exit
            // code is 0 either way once the rpc subcommand returns cleanly),
            // so exit_code 0 here reflects "the runner completed without
            // error" — check the Job's pod logs for the script's own output.
            set_remote_task_status(
                &ctx.client,
                &ns,
                &name,
                RemoteTaskStatus {
                    phase: RemoteTaskPhase::Succeeded,
                    exit_code: Some(0),
                    completion_time: Some(now()),
                    conditions: vec![ginger_infra::remote_task::RemoteTaskCondition {
                        type_: "Succeeded".to_string(),
                        status: "True".to_string(),
                        reason: Some("JobCompleted".to_string()),
                        message: Some("execution Job completed successfully".to_string()),
                    }],
                    ..Default::default()
                },
            )
            .await?;
            Ok(Action::await_change())
        }
        JobOutcome::Failed { message } => {
            set_remote_task_status(
                &ctx.client,
                &ns,
                &name,
                RemoteTaskStatus {
                    phase: RemoteTaskPhase::Failed,
                    completion_time: Some(now()),
                    message: Some(message.clone()),
                    conditions: vec![ginger_infra::remote_task::RemoteTaskCondition {
                        type_: "Succeeded".to_string(),
                        status: "False".to_string(),
                        reason: Some("JobFailed".to_string()),
                        message: Some(message),
                    }],
                    ..Default::default()
                },
            )
            .await?;
            Ok(Action::await_change())
        }
    }
}