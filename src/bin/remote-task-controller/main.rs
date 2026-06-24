use std::sync::Arc;

use futures_util::StreamExt;
use kube::{
    api::{Api, DynamicObject},
    discovery::ApiResource,
    runtime::{controller::Action, watcher, Controller},
    Client, ResourceExt,
};

use ginger_infra::remote_task::{RemoteTask, RemoteTaskPhase};

mod customrun;
mod dispatch;
mod events;
mod rpc;

use customrun::{
    customrun_error_policy, reconcile_customrun, CustomRunContext, CUSTOMRUN_GROUP,
    CUSTOMRUN_KIND, CUSTOMRUN_PLURAL, CUSTOMRUN_VERSION,
};

const REQUEUE_AFTER_ERROR_SECS: u64 = 30;

#[derive(Debug, thiserror::Error)]
enum ReconcileError {
    #[error("k8s api error: {0}")]
    Kube(#[from] kube::Error),
}

struct ControllerContext {
    client: Client,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("[tekton-controller] starting...");

    let sidekick_url = std::env::var("SIDEKICK_URL")
        .map_err(|_| anyhow::anyhow!("SIDEKICK_URL env var is required"))?;

    println!("[tekton-controller] sidekick_url={}", sidekick_url);

    let client = Client::try_default().await?;

    let remote_tasks: Api<RemoteTask> = Api::all(client.clone());
    let ctx = Arc::new(ControllerContext { client: client.clone() });

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
        sidekick_url: sidekick_url.clone(),
    });

    println!("[tekton-controller] watching RemoteTask and CustomRun across all namespaces");

    let remote_task_controller = Controller::new(remote_tasks, watcher::Config::default())
        .run(reconcile_remote_task, error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok(o) => println!("[tekton-controller] reconciled remotetask {:?}", o),
                Err(e) => eprintln!("[tekton-controller] remotetask reconcile error: {:?}", e),
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
            let owner = rt.metadata.owner_references.as_ref()?
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
                .within(rt.metadata.namespace.as_deref().unwrap_or("default")),
            )
        },
    )
    .run(reconcile_customrun, customrun_error_policy, customrun_ctx)
    .for_each(|res| async move {
        match res {
            Ok(o) => println!("[tekton-controller] reconciled customrun {:?}", o),
            Err(e) => eprintln!("[tekton-controller] customrun reconcile error: {:?}", e),
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

async fn reconcile_remote_task(
    task: Arc<RemoteTask>,
    _ctx: Arc<ControllerContext>,
) -> Result<Action, ReconcileError> {
    let name = task.name_any();
    let ns = task.namespace().unwrap_or_else(|| "default".to_string());
    let phase = task.status.as_ref().map(|s| s.phase.clone()).unwrap_or_default();

    println!("[tekton-controller] remotetask {}/{} phase={:?}", ns, name, phase);

    Ok(Action::await_change())
}