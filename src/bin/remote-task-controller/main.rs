//! remote-task-controller
//!
//! Runs two reconcilers:
//!
//! 1. CustomRun reconciler — Tekton calls this when a Pipeline task uses
//!    `taskRef: apiVersion: gingersociety.org/v1alpha1, kind: RemoteTask`.
//!    It reads the spec fields (capability, script, cleanup, env) off the
//!    CustomRun and creates a TaskRun that runs the runner image.
//!
//! 2. RemoteTask reconciler — watches standalone RemoteTask CRD objects
//!    (applied directly, not via a pipeline) and likewise creates a TaskRun.
//!
//! In both cases the controller's only job is: parse spec → create TaskRun.
//! Tekton owns pod lifecycle, logs, dashboard, and pipeline view from there.
//!
//! ## Credentials
//!
//! The runner no longer needs a Kubernetes Secret for auth.json. Credentials
//! are written to the shared `creds` workspace by the init-credentials step
//! injected by ginger-gitter, and the runner reads them from there via the
//! GINGER_AUTH_PATH env var set by taskrun.rs. The AUTH_SECRET_NAME env var
//! has been removed.

use std::sync::Arc;

use futures_util::StreamExt;
use kube::{
    api::{Api, DynamicObject},
    discovery::ApiResource,
    runtime::{controller::Action, watcher, Controller},
    Client, ResourceExt,
};

use ginger_infra::remote_task::RemoteTask;

mod customrun;
mod remotetask;
mod taskrun;

use customrun::{
    customrun_error_policy, reconcile_customrun, CustomRunContext,
    CUSTOMRUN_GROUP, CUSTOMRUN_KIND, CUSTOMRUN_PLURAL, CUSTOMRUN_VERSION,
};
use remotetask::{error_policy, reconcile_remote_task, RemoteTaskContext};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("[remote-task-controller] starting...");

    let sidekick_url = std::env::var("SIDEKICK_URL")
        .map_err(|_| anyhow::anyhow!("SIDEKICK_URL env var is required"))?;

    println!("[remote-task-controller] sidekick_url={sidekick_url}");

    let client = Client::try_default().await?;

    // ── CustomRun controller ───────────────────────────────────────────────
    let customrun_ar = ApiResource {
        group: CUSTOMRUN_GROUP.to_string(),
        version: CUSTOMRUN_VERSION.to_string(),
        api_version: format!("{CUSTOMRUN_GROUP}/{CUSTOMRUN_VERSION}"),
        kind: CUSTOMRUN_KIND.to_string(),
        plural: CUSTOMRUN_PLURAL.to_string(),
    };
    let customruns: Api<DynamicObject> =
        Api::all_with(client.clone(), &customrun_ar);

    let customrun_ctx = Arc::new(CustomRunContext {
        client: client.clone(),
        sidekick_url: sidekick_url.clone(),
    });

    let customrun_controller = Controller::new_with(
        customruns,
        watcher::Config::default(),
        customrun_ar,
    )
    .run(reconcile_customrun, customrun_error_policy, customrun_ctx)
    .for_each(|res| async move {
        match res {
            Ok(o)  => println!("[customrun-controller] reconciled {:?}", o),
            Err(e) => eprintln!("[customrun-controller] error: {:?}", e),
        }
    });

    // ── RemoteTask controller ──────────────────────────────────────────────
    let remotetask_ctx = Arc::new(RemoteTaskContext {
        client: client.clone(),
        sidekick_url,
    });

    let remotetask_controller = Controller::new(
        Api::<RemoteTask>::all(client),
        watcher::Config::default(),
    )
    .run(reconcile_remote_task, error_policy, remotetask_ctx)
    .for_each(|res| async move {
        match res {
            Ok(o)  => println!("[remotetask-controller] reconciled {:?}", o),
            Err(e) => eprintln!("[remotetask-controller] error: {:?}", e),
        }
    });

    println!("[remote-task-controller] watching CustomRun + RemoteTask across all namespaces");

    tokio::join!(customrun_controller, remotetask_controller);

    Ok(())
}