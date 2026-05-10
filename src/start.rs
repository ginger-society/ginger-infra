use IAMService::models::ValidateApiTokenResponse;
use MetadataService::apis::{configuration::Configuration as MetadataConfiguration, default_api::{MetadataGetPackageVersionPlainTextParams, metadata_get_package_version_plain_text}};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use crate::{wamp_args, wamp_client::WampClient};

#[derive(Deserialize)]
struct SnapInstallArgs {
    package: String,
}

#[derive(Deserialize)]
struct AptArgs {
    port: u16,
}

pub async fn main(access_token: String, token_response: ValidateApiTokenResponse, metadata_config: &MetadataConfiguration) {
    let client = Arc::new(WampClient::new(
        "ginger_infra",
        &access_token,
        &token_response.sub,
    ));

    client.register("snap_install", |args, _kwargs| async move {
        let parsed: SnapInstallArgs = wamp_args!(args)?;
        println!("Installing: {}", parsed.package);
        Ok(json!({"status": "installed", "package": parsed.package}))
    }).await;

    client.register("apt_update", |args, _kwargs| async move {
        let parsed: AptArgs = wamp_args!(args)?;
        println!("Updating apt on port: {}", parsed.port);
        println!("Updating apt...");
        Ok(json!({"status": "done"}))
    }).await;


    let metadata_config_ref = metadata_config.clone();

    client.register("self_update", move |_args, _kwargs| {
        let metadata_config = metadata_config_ref.clone(); // clone per-call, outside async
        async move {
            let current_version = env!("CARGO_PKG_VERSION");

            let latest_version = match metadata_get_package_version_plain_text(
                &metadata_config,
                MetadataGetPackageVersionPlainTextParams {
                    org_id: "ginger-society".to_string(),
                    package_name: "ginger-infra".to_string(),
                },
            ).await {
                Ok(v) => v.trim().to_string(),
                Err(e) => return Err(format!("failed to fetch version: {}", e)),
            };

            println!("[self_update] current={} latest={}", current_version, latest_version);

            if current_version == latest_version {
                println!("[self_update] already up to date");
                return Ok(json!({"status": "up_to_date", "version": current_version}));
            }

            let status = tokio::process::Command::new("bash")
                .arg("-c")
                .arg(r#"bash -c "$(curl -fsSL https://raw.githubusercontent.com/ginger-society/infra-as-code-repo/main/rust-helpers/installer.sh)" -- ginger-society/ginger-infra:latest"#)
                .status()
                .await
                .map_err(|e| format!("failed to run installer: {}", e))?;

            if !status.success() {
                return Err(format!("installer failed: {:?}", status.code()));
            }

            tokio::spawn(async {
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                std::process::exit(0);
            });

            Ok(json!({"status": "updated", "from": current_version, "to": latest_version}))
        }
    }).await;

    // spawn heartbeat as a separate task — nothing to do with WampClient internals
    let heartbeat_client = Arc::clone(&client);
    let heartbeat_topic = format!("heartbeat.ginger_infra_{}", token_response.sub);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
        interval.tick().await; // discard immediate tick
        loop {
            interval.tick().await;
            if heartbeat_client.is_connected() {
                let stats = crate::heartbeat::collect_stats();
                if let Err(e) = heartbeat_client.publish(&heartbeat_topic, stats).await {
                    eprintln!("[heartbeat] publish failed: {}", e);
                }
            }
        }
    });

    client.listen().await;
}