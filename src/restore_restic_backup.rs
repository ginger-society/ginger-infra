use anyhow::{bail, Context, Result};
use ginger_infra::resticrestore::{
    ResticRestore, ResticRestoreSpec, PHASE_FAILED, PHASE_SUCCEEDED,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, PostParams};
use kube::Client;
use std::time::Duration;

pub async fn run_restore(pvc_name: &str, namespace: &str, clean: bool, snapshot: Option<&str>) -> Result<()> {
    let client = Client::try_default().await.context("connecting to cluster")?;
    let restores: Api<ResticRestore> = Api::namespaced(client.clone(), namespace);

    let run_id = chrono::Utc::now().format("%Y%m%d%H%M%S").to_string();
    let cr_name = format!("restore-{}-{}", sanitize(pvc_name), run_id);

    let cr = ResticRestore {
        metadata: ObjectMeta {
            name: Some(cr_name.clone()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        spec: ResticRestoreSpec {
            pvc_name: pvc_name.to_string(),
            snapshot_id: snapshot.map(str::to_string),
            clean_existing_data: clean,
        },
        status: None,
    };

    restores.create(&PostParams::default(), &cr).await
        .context("creating ResticRestore CR")?;

    println!("created ResticRestore '{namespace}/{cr_name}' for pvc '{pvc_name}'; watching status...");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30 * 60);
    loop {
        let current = restores.get(&cr_name).await?;
        if let Some(status) = &current.status {
            if !status.phase.is_empty() {
                println!("[{}] {}", status.phase, status.message);
            }
            match status.phase.as_str() {
                PHASE_SUCCEEDED => {
                    println!("✅ restore of '{pvc_name}' succeeded.");
                    return Ok(());
                }
                PHASE_FAILED => {
                    bail!("❌ restore of '{pvc_name}' failed: {}", status.message);
                }
                _ => {}
            }
        }
        if tokio::time::Instant::now() > deadline {
            bail!("timed out waiting for restore '{namespace}/{cr_name}' to complete");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

fn sanitize(name: &str) -> String {
    name.chars().map(|c| if c.is_alphanumeric() || c == '-' { c } else { '-' }).collect()
}