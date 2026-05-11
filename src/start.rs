use IAMService::models::ValidateApiTokenResponse;
use MetadataService::apis::{configuration::Configuration as MetadataConfiguration, default_api::{MetadataGetPackageVersionPlainTextParams, metadata_get_package_version_plain_text}};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use crate::{wamp_args, wamp_client::WampClient};


#[derive(Deserialize)]
struct InstallSslArgs {
    domain: String,
}

#[derive(Deserialize)]
struct PortMapping {
    container_port: u16,
    host_port: u16,
    protocol: Option<String>,
}

#[derive(Deserialize)]
struct CreateClusterArgs {
    name: String,
    api_port: u16,
    port_mappings: Vec<PortMapping>,
}

#[derive(Deserialize)]
struct SetupGatewayArgs {
    domain: String,
    port: u16,
    #[serde(default)]
    websocket: bool,
}
 


pub async fn main(access_token: String, token_response: ValidateApiTokenResponse, metadata_config: &MetadataConfiguration, device_id: String) {
    let client = Arc::new(WampClient::new(
        &format!("{}.ginger_infra", device_id),
        &access_token,
        &token_response.sub,
    ));


    client.register("install_ssl", |args, _kwargs| async move {
        let parsed: InstallSslArgs = wamp_args!(args)?;

        println!("[install_ssl] requesting cert for domain={}", parsed.domain);

        let status = tokio::process::Command::new("certbot")
            .arg("certonly")
            .arg("--apache")
            .arg("--non-interactive")
            .arg("--agree-tos")
            .arg("--no-eff-email")
            .arg("--register-unsafely-without-email")
            .arg("-d")
            .arg(&parsed.domain)
            .status()
            .await
            .map_err(|e| json!({"error": format!("failed to run certbot: {}", e)}))?;

        if !status.success() {
            return Err(json!({
                "error": "certbot failed",
                "exit_code": status.code(),
                "domain": parsed.domain,
            }));
        }

        Ok(json!({
            "status": "installed",
            "domain": parsed.domain,
        }))
    }).await;

    
    client.register("setup_gateway", |args, _kwargs| async move {
        let parsed: SetupGatewayArgs = wamp_args!(args)?;
    
        println!(
            "[setup_gateway] domain={} port={} websocket={}",
            parsed.domain, parsed.port, parsed.websocket
        );
    
        let ws_flag = if parsed.websocket { "--websocket" } else { "" };
    
        let mut child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(format!(
                r#"curl -fsSL https://raw.githubusercontent.com/ginger-society/infra-as-code-repo/main/ginger-infra-helpers/setup-gateway.sh | bash -s -- --domain {} --port {} {}"#,
                parsed.domain, parsed.port, ws_flag
            ))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| json!({"error": format!("failed to spawn setup-gateway.sh: {}", e)}))?;
    
        let output = child
            .wait_with_output()
            .await
            .map_err(|e| json!({"error": format!("failed to wait on setup-gateway.sh: {}", e)}))?;
    
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    
        if !output.status.success() {
            return Err(json!({
                "error": "setup-gateway.sh failed",
                "exit_code": output.status.code(),
                "domain": parsed.domain,
                "stdout": stdout,
                "stderr": stderr,
            }));
        }
    
        Ok(json!({
            "status": "configured",
            "domain": parsed.domain,
            "port": parsed.port,
            "websocket": parsed.websocket,
            "stdout": stdout,
        }))
    }).await;

    client.register("create_cluster", |args, _kwargs| async move {
        let parsed: CreateClusterArgs = wamp_args!(args)?;

        let name = &parsed.name;
        let api_port = parsed.api_port.to_string();

        let port_mappings = serde_json::to_string(&json!(parsed.port_mappings
            .iter()
            .map(|p| json!({
                "container_port": p.container_port,
                "host_port": p.host_port,
                "protocol": p.protocol.as_deref().unwrap_or("TCP"),
            }))
            .collect::<Vec<_>>()
        )).map_err(|e| json!({"error": format!("failed to serialize port mappings: {}", e)}))?;

        println!("[create_cluster] name={} api_port={}", name, api_port);
        println!("[create_cluster] port_mappings={}", port_mappings);

        let mut child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(format!(
                r#"curl -fsSL https://raw.githubusercontent.com/ginger-society/infra-as-code-repo/main/ginger-infra-helpers/create-kind-cluster.sh | bash -s -- {} {} '{}'"#,
                name, api_port, port_mappings
            ))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| json!({"error": format!("failed to spawn script: {}", e)}))?;

        // stream stderr to logs in real time
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut reader = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    eprintln!("[create_cluster] stderr: {}", line);
                }
            });
        }

        // stream stdout to logs and capture it
        let mut stdout_lines = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            use tokio::io::AsyncBufReadExt;
            let mut reader = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                println!("[create_cluster] stdout: {}", line);
                stdout_lines.push(line);
            }
        }

        let status = child.wait().await
            .map_err(|e| json!({"error": format!("failed to wait for script: {}", e)}))?;

        let stdout = stdout_lines.join("\n");

        if !status.success() {
            let exit_code = status.code().unwrap_or(-1);
            let error_msg = match exit_code {
                2 => format!("cluster '{}' already exists", name),
                3 => "kind create cluster failed".to_string(),
                4 => "nginx reload failed after updating stream config".to_string(),
                5 => "failed to get kubeconfig".to_string(),
                _ => format!("script failed with exit code {}", exit_code),
            };
            return Err(json!({
                "error": error_msg,
                "exit_code": exit_code,
            }));
        }

        let kubeconfig = stdout
            .split("KUBECONFIG_START")
            .nth(1)
            .and_then(|s| s.split("KUBECONFIG_END").next())
            .map(|s| s.trim().to_string())
            .ok_or_else(|| json!({"error": "failed to extract kubeconfig from output"}))?;

        println!("[create_cluster] cluster '{}' created successfully", name);

        Ok(json!({
            "status": "created",
            "cluster": name,
            "fqdn": format!("{}.test-clusters.rackmint.com", name),
            "api_port": parsed.api_port,
            "port_mappings": parsed.port_mappings.iter().map(|p| json!({
                "container_port": p.container_port,
                "host_port": p.host_port,
                "protocol": p.protocol.as_deref().unwrap_or("TCP"),
            })).collect::<Vec<_>>(),
            "kubeconfig": kubeconfig,
        }))
    }).await;


    let metadata_config_ref = metadata_config.clone();

    client.register("self_update", move |_args, _kwargs| {
        let metadata_config = metadata_config_ref.clone();
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
                Err(e) => return Err(json!({"error": format!("failed to fetch version: {}", e)})),
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
                .map_err(|e| json!({"error": format!("failed to run installer: {}", e)}))?;

            if !status.success() {
                return Err(json!({
                    "error": "installer failed",
                    "exit_code": status.code()
                }));
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
    let heartbeat_topic = format!("heartbeat.{}.ginger_infra_{}", device_id, token_response.sub);
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