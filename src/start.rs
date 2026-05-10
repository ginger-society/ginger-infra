use IAMService::models::ValidateApiTokenResponse;
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

pub async fn main(access_token: String, token_response: ValidateApiTokenResponse) {
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