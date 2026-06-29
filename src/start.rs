use GingerPresence::{apis::default_api::routes_index, get_configuration};
use IAMService::models::ValidateApiTokenResponse;
use MetadataService::apis::{configuration::Configuration as MetadataConfiguration, default_api::{MetadataGetPackageVersionPlainTextParams, metadata_get_package_version_plain_text}};
use ginger_shared_rs::utils::get_token_from_file_storage;
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
    #[serde(default = "default_cpus")]
    cpus: u16,
    #[serde(default = "default_memory")]
    memory: String,
    #[serde(default = "default_disk")]
    disk: String,
}

fn default_cpus() -> u16 { 2 }
fn default_memory() -> String { "4g".to_string() }
fn default_disk() -> String { "30g".to_string() }

#[derive(Deserialize)]
struct SetupGatewayArgs {
    domain: String,
    port: u16,
    #[serde(default)]
    websocket: bool,
}

#[derive(Deserialize)]
struct DeleteClusterArgs {
    name: String,
}

#[derive(Deserialize)]
struct DeleteGatewayArgs {
    domain: String,
}

#[derive(Deserialize)]
struct ExecuteArgs {
    job_id: String,
    script: String,
    cleanup_script: Option<String>,
    /// Flat key/value env vars — already resolved by the executor
    #[serde(default)]
    env: std::collections::HashMap<String, String>,
    /// WAMP channel of the executor to publish log events back to
    reply_channel: String,
}


pub async fn main(
    access_token: String,
    token_response: ValidateApiTokenResponse,
    metadata_config: &MetadataConfiguration,
    device_id: String,
    capabilities: Vec<String>,
) {
    let client = Arc::new(WampClient::new(
        &format!("{}.ginger_infra", device_id),
        &access_token,
        &token_response.sub,
    ));


    // ── execute ───────────────────────────────────────────────────────────────
    //
    // Called by the executor service to run a shell script on this device.
    //
    // Flow:
    //   1. Write script to /tmp/{job_id}/run.sh and chmod +x
    //   2. Spawn bash, stream stdout/stderr lines back to reply_channel as
    //      WAMP publish events with type="log"
    //   3. Wait for exit
    //   4. Run cleanup_script (if any) unconditionally — failures logged, not propagated
    //   5. Publish a type="done" or type="error" event so the executor closes the SSE stream
    //   6. Return Ok({exit_code}) or Err({exit_code, error}) — this unblocks the
    //      executor's call() which then triggers its own cleanup path
    {
        let execute_client = Arc::clone(&client);
        client.register("execute", move |_args, kwargs| {
            let wamp = Arc::clone(&execute_client);
            async move {
                // ── parse kwargs ──────────────────────────────────────────────
                let raw = kwargs.ok_or_else(|| json!({"error": "missing kwargs"}))?;
                let parsed: ExecuteArgs = serde_json::from_value(raw)
                    .map_err(|e| json!({"error": format!("invalid kwargs: {}", e)}))?;

                let job_dir = format!("/tmp/{}", parsed.job_id);
                let script_path = format!("{}/run.sh", job_dir);
                let cleanup_path = format!("{}/cleanup.sh", job_dir);

                // ── 1. write scripts to disk ──────────────────────────────────
                tokio::fs::create_dir_all(&job_dir)
                    .await
                    .map_err(|e| json!({"error": format!("failed to create job dir: {}", e)}))?;

                tokio::fs::write(&script_path, &parsed.script)
                    .await
                    .map_err(|e| json!({"error": format!("failed to write run.sh: {}", e)}))?;

                tokio::process::Command::new("chmod")
                    .args(["+x", &script_path])
                    .status()
                    .await
                    .map_err(|e| json!({"error": format!("chmod failed: {}", e)}))?;

                if let Some(ref cleanup) = parsed.cleanup_script {
                    tokio::fs::write(&cleanup_path, cleanup)
                        .await
                        .map_err(|e| json!({"error": format!("failed to write cleanup.sh: {}", e)}))?;

                    tokio::process::Command::new("chmod")
                        .args(["+x", &cleanup_path])
                        .status()
                        .await
                        .map_err(|e| json!({"error": format!("chmod failed: {}", e)}))?;
                }

                // ── 2. spawn run.sh ───────────────────────────────────────────
                let mut cmd = tokio::process::Command::new("bash");
                cmd.arg(&script_path)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());

                for (k, v) in &parsed.env {
                    cmd.env(k, v);
                }

                cmd.env("RPC_JOB_ID", &parsed.job_id);           
                cmd.env("RPC_CREDS_DIR", format!("/tmp/rpc/{}", &parsed.job_id));

                let mut child = cmd.spawn().map_err(|e| {
                    json!({"error": format!("failed to spawn run.sh: {}", e)})
                })?;

                // ── 3. stream stdout/stderr back as WAMP publish events ────────
                //
                // Each line is published to reply_channel with kwargs:
                //   { type: "log", stream: "stdout"|"stderr", line: "...", correlation_id: "..." }
                //
                // The executor's event_subs listener picks these up and
                // forwards them to the caller's SSE stream.

                let stdout = child.stdout.take();
                let stderr = child.stderr.take();

                let wamp_stdout = Arc::clone(&wamp);
                let reply_stdout = parsed.reply_channel.clone();
                let job_id_stdout = parsed.job_id.clone();

                let stdout_task = tokio::spawn(async move {
                    if let Some(stdout) = stdout {
                        use tokio::io::AsyncBufReadExt;
                        let mut reader = tokio::io::BufReader::new(stdout).lines();
                        while let Ok(Some(line)) = reader.next_line().await {
                            println!("[execute:stdout] {}", line);
                            let _ = wamp_stdout
                                .publish(
                                    &reply_stdout,
                                    json!({
                                        "type": "log",
                                        "stream": "stdout",
                                        "line": line,
                                        "correlation_id": job_id_stdout,
                                    }),
                                )
                                .await;
                        }
                    }
                });

                let wamp_stderr = Arc::clone(&wamp);
                let reply_stderr = parsed.reply_channel.clone();
                let job_id_stderr = parsed.job_id.clone();

                let stderr_task = tokio::spawn(async move {
                    if let Some(stderr) = stderr {
                        use tokio::io::AsyncBufReadExt;
                        let mut reader = tokio::io::BufReader::new(stderr).lines();
                        while let Ok(Some(line)) = reader.next_line().await {
                            eprintln!("[execute:stderr] {}", line);
                            let _ = wamp_stderr
                                .publish(
                                    &reply_stderr,
                                    json!({
                                        "type": "log",
                                        "stream": "stderr",
                                        "line": line,
                                        "correlation_id": job_id_stderr,
                                    }),
                                )
                                .await;
                        }
                    }
                });

                // wait for the process and both stream tasks
                let status = child
                    .wait()
                    .await
                    .map_err(|e| json!({"error": format!("failed to wait for run.sh: {}", e)}))?;

                let _ = tokio::join!(stdout_task, stderr_task);

                let exit_code = status.code().unwrap_or(-1);

                // ── 4. run cleanup unconditionally ────────────────────────────
                if parsed.cleanup_script.is_some() {
                    println!("[execute] running cleanup.sh for job_id={}", parsed.job_id);

                    let mut cleanup_cmd = tokio::process::Command::new("bash");
                    cleanup_cmd
                        .arg(&cleanup_path)
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped());

                    for (k, v) in &parsed.env {
                        cleanup_cmd.env(k, v);
                    }

                    cleanup_cmd.env("RPC_JOB_ID", &parsed.job_id);          
                    cleanup_cmd.env("RPC_CREDS_DIR", format!("/tmp/rpc/{}", &parsed.job_id));

                    match cleanup_cmd.spawn() {
                        Err(e) => {
                            eprintln!("[execute] failed to spawn cleanup.sh: {}", e);
                        }
                        Ok(mut cleanup_child) => {
                            // drain cleanup output to logs — never propagated to caller
                            if let Some(out) = cleanup_child.stdout.take() {
                                tokio::spawn(async move {
                                    use tokio::io::AsyncBufReadExt;
                                    let mut r = tokio::io::BufReader::new(out).lines();
                                    while let Ok(Some(l)) = r.next_line().await {
                                        println!("[execute:cleanup:stdout] {}", l);
                                    }
                                });
                            }
                            if let Some(err) = cleanup_child.stderr.take() {
                                tokio::spawn(async move {
                                    use tokio::io::AsyncBufReadExt;
                                    let mut r = tokio::io::BufReader::new(err).lines();
                                    while let Ok(Some(l)) = r.next_line().await {
                                        eprintln!("[execute:cleanup:stderr] {}", l);
                                    }
                                });
                            }
                            match cleanup_child.wait().await {
                                Ok(s) => println!(
                                    "[execute] cleanup.sh exited with {}",
                                    s.code().unwrap_or(-1)
                                ),
                                Err(e) => eprintln!("[execute] cleanup wait error: {}", e),
                            }
                        }
                    }
                }

                // ── 5. clean up job dir ───────────────────────────────────────
                let _ = tokio::fs::remove_dir_all(&job_dir).await;

                let _ = tokio::fs::remove_dir_all( format!("/tmp/rpc/{}", &parsed.job_id) ).await;  

                // ── 6. publish terminal event to close the SSE stream ─────────
                //
                // This must happen BEFORE returning so the executor's
                // event subscriber receives "done"/"error" and yields the
                // final SSE event before the call() resolves.
                if status.success() {
                    let _ = wamp
                        .publish(
                            &parsed.reply_channel,
                            json!({
                                "type": "done",
                                "exit_code": exit_code,
                                "correlation_id": parsed.job_id,
                            }),
                        )
                        .await;

                    Ok(json!({"exit_code": exit_code}))
                } else {
                    let _ = wamp
                        .publish(
                            &parsed.reply_channel,
                            json!({
                                "type": "error",
                                "message": format!("script exited with code {}", exit_code),
                                "exit_code": exit_code,
                                "correlation_id": parsed.job_id,
                            }),
                        )
                        .await;

                    Err(json!({
                        "exit_code": exit_code,
                        "error": format!("script exited with code {}", exit_code),
                    }))
                }
            }
        }).await;
    }

    client.register("delete_cluster", |args, _kwargs| async move {

        let parsed: DeleteClusterArgs = wamp_args!(args)?;
        let name = &parsed.name;

        println!("[delete_cluster] deleting cluster '{}'", name);

        let mut child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(format!(
                r#"curl -fsSL https://raw.githubusercontent.com/ginger-society/infra-as-code-repo/main/ginger-infra-helpers/delete-kind-cluster.sh | bash -s -- {}"#,
                name
            ))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| json!({"error": format!("failed to spawn script: {}", e)}))?;

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut reader = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    eprintln!("[delete_cluster] stderr: {}", line);
                }
            });
        }

        let status = child.wait().await
            .map_err(|e| json!({"error": format!("failed to wait for script: {}", e)}))?;

        if !status.success() {
            let exit_code = status.code().unwrap_or(-1);
            let error_msg = match exit_code {
                2 => format!("cluster '{}' does not exist", name),
                3 => "kind delete cluster failed".to_string(),
                4 => "nginx reload failed after removing cluster entry".to_string(),
                _ => format!("script failed with exit code {}", exit_code),
            };
            return Err(json!({
                "error": error_msg,
                "exit_code": exit_code,
            }));
        }

        println!("[delete_cluster] cluster '{}' deleted successfully", name);

        Ok(json!({
            "status": "deleted",
            "cluster": name,
        }))
    }).await;

    #[cfg(not(unix))]
    client.register("install_ssl", |_args, _kwargs| async move {
        Err(json!({"error": "install_ssl is not supported on this platform"}))
    }).await;

    #[cfg(unix)]
    client.register("install_ssl", |args, _kwargs| async move {
        let mut parsed: InstallSslArgs = wamp_args!(args)?;
        parsed.domain = parsed.domain.trim().to_string();

        println!("[install_ssl] requesting cert for domain={}", parsed.domain);

        let base_domain = parsed.domain.clone();

        let wildcard_cert_path = format!("/etc/letsencrypt/live/{}/privkey.pem", base_domain);

        if std::path::Path::new(&wildcard_cert_path).exists() {
            println!("[install_ssl] wildcard cert already exists at {}, skipping certbot", wildcard_cert_path);
            return Ok(json!({
                "status": "already_installed",
                "domain": parsed.domain,
                "base_domain": base_domain,
                "cert_path": wildcard_cert_path,
            }));
        }

        let lock_path = "/tmp/certbot.lock";
        let lock_file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(lock_path)
            .await
            .map_err(|e| json!({"error": format!("failed to open lock file: {}", e)}))?;

        use std::os::unix::io::IntoRawFd;
        let fd = lock_file.into_std().await.into_raw_fd();
        unsafe { libc::flock(fd, libc::LOCK_EX); }

        if std::path::Path::new(&wildcard_cert_path).exists() {
            unsafe { libc::flock(fd, libc::LOCK_UN); }
            println!("[install_ssl] wildcard cert installed by concurrent call, skipping");
            return Ok(json!({
                "status": "already_installed",
                "domain": parsed.domain,
                "base_domain": base_domain,
                "cert_path": wildcard_cert_path,
            }));
        }

        println!("[install_ssl] issuing wildcard cert for base_domain={}", base_domain);

        let output = tokio::process::Command::new("certbot")
            .arg("certonly")
            .arg("--authenticator").arg("dns-godaddy")
            .arg("--dns-godaddy-credentials").arg("/etc/letsencrypt/godaddy.ini")
            .arg("--non-interactive")
            .arg("--agree-tos")
            .arg("--register-unsafely-without-email")
            .arg("-d").arg(&base_domain)
            .arg("-d").arg(format!("*.{}", base_domain))
            .output()
            .await
            .map_err(|e| {
                unsafe { libc::flock(fd, libc::LOCK_UN); }
                json!({"error": format!("failed to run certbot: {}", e)})
            })?;

        unsafe { libc::flock(fd, libc::LOCK_UN); }

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        println!("[install_ssl] certbot stdout:\n{}", stdout);
        println!("[install_ssl] certbot stderr:\n{}", stderr);

        if !output.status.success() {
            return Err(json!({
                "error": "certbot failed",
                "exit_code": output.status.code(),
                "domain": parsed.domain,
                "base_domain": base_domain,
                "stdout": stdout,
                "stderr": stderr,
            }));
        }

        Ok(json!({
            "status": "installed",
            "domain": parsed.domain,
            "base_domain": base_domain,
            "cert_path": wildcard_cert_path,
            "stdout": stdout,
        }))
    }).await;

    client.register("delete_gateway", |args, _kwargs| async move {
        let parsed: DeleteGatewayArgs = wamp_args!(args)?;

        println!(
            "[delete_gateway] domain={}",
            parsed.domain
        );

        let mut child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(format!(
                r#"curl -fsSL https://raw.githubusercontent.com/ginger-society/infra-as-code-repo/main/ginger-infra-helpers/delete-gateway.sh | bash -s -- --domain {}"#,
                parsed.domain
            ))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| json!({"error": format!("failed to spawn delete-gateway.sh: {}", e)}))?;

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| json!({"error": format!("failed to wait on delete-gateway.sh: {}", e)}))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !output.status.success() {
            return Err(json!({
                "error": "delete-gateway.sh failed",
                "exit_code": output.status.code(),
                "domain": parsed.domain,
                "stdout": stdout,
                "stderr": stderr,
            }));
        }

        Ok(json!({
            "status": "deleted",
            "domain": parsed.domain,
            "stdout": stdout,
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

        println!(
            "[create_cluster] name={} api_port={} cpus={} memory={} disk={}",
            name, api_port, parsed.cpus, parsed.memory, parsed.disk
        );
        println!("[create_cluster] port_mappings={}", port_mappings);

        let mut child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(format!(
                r#"curl -fsSL https://raw.githubusercontent.com/ginger-society/infra-as-code-repo/main/ginger-infra-helpers/create-kind-cluster.sh | bash -s -- {} {} '{}' --cpus {} --memory {} --disk {}"#,
                name, api_port, port_mappings, parsed.cpus, parsed.memory, parsed.disk
            ))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| json!({"error": format!("failed to spawn script: {}", e)}))?;

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut reader = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    eprintln!("[create_cluster] stderr: {}", line);
                }
            });
        }

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


    let token = get_token_from_file_storage();

    let presence_config = get_configuration(Some(token));

    let device_channel = match routes_index(&presence_config).await {
        Ok(resp) => {
            println!("{:?}", resp.message);
            resp.message
        }
        Err(e) => {
            eprintln!("[start] failed to fetch presence channel: {:?}", e);
            String::new()
        }
    };

    // spawn heartbeat as a separate task
    let heartbeat_client = Arc::clone(&client);
    let heartbeat_capabilities = capabilities.clone();
    let own_channel = client.channel().to_string();

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
        interval.tick().await;

        let mut current_device_channel = device_channel.clone();
        let mut consecutive_failures: u32 = 0;

        loop {
            interval.tick().await;

            if consecutive_failures >= 3 {
                eprintln!("[heartbeat] {} consecutive failures — re-fetching presence channel...", consecutive_failures);
                let token = get_token_from_file_storage();
                let presence_config = get_configuration(Some(token));
                match routes_index(&presence_config).await {
                    Ok(resp) => {
                        let new_channel = resp.message;
                        if new_channel != current_device_channel {
                            println!("[heartbeat] presence channel changed: {} → {}", current_device_channel, new_channel);
                            current_device_channel = new_channel;
                        } else {
                            println!("[heartbeat] presence channel unchanged: {}", current_device_channel);
                        }
                        consecutive_failures = 0;
                    }
                    Err(e) => {
                        eprintln!("[heartbeat] failed to re-fetch presence channel: {:?}", e);
                        continue;
                    }
                }
            }

            if !heartbeat_client.is_connected() || current_device_channel.is_empty() {
                continue;
            }

            let result = heartbeat_client
                .call(
                    "handle_heartbeat",
                    current_device_channel.clone(),
                    json!([]),
                    json!({
                        "device_channel": own_channel,
                        "capabilities": heartbeat_capabilities,
                    }),
                )
                .await;

            match result {
                Ok(_) => {
                    consecutive_failures = 0;
                }
                Err(e) => {
                    consecutive_failures += 1;
                    eprintln!(
                        "[heartbeat] handle_heartbeat call failed ({}/3): {:?}",
                        consecutive_failures, e
                    );
                }
            }
        }
    });

    client.listen().await;
}