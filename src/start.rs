use IAMService::models::ValidateApiTokenResponse;
use serde_json::json;

use crate::wamp_client::WampClient;

pub async fn main(access_token: String, token_response: ValidateApiTokenResponse) {
    let client = WampClient::new(
        "ginger_infra",        // prefix — channel = "ginger_infra_{sub}"
        &access_token,
        &token_response.sub,   // sub comes from the validated token
    );

    client.register("snap_install", |args, _kwargs| async move {
        let package = args
            .as_ref()
            .and_then(|a| a.get(0))
            .and_then(|v| v.get("package"))  // ← get "package" key from the object
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        println!("Installing: {}", package);
        Ok(json!({"status": "installed", "package": package}))
    }).await;

    client.register("apt_update", |args, _kwargs| async move {

        println!("Updating apt...");
        Ok(json!({"status": "done"}))
    }).await;

    client.listen().await;
}