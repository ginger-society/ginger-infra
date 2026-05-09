use IAMService::models::ValidateApiTokenResponse;
use serde::Deserialize;
use serde_json::json;

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
    let client = WampClient::new(
        "ginger_infra",        // prefix — channel = "ginger_infra_{realm}"
        &access_token,
        &token_response.sub,   // realm comes from the validated token
    );

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

    client.listen().await;
}