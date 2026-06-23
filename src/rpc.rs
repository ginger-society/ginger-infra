// src/rpc.rs
//
// `ginger-infra rpc` — reads a .envrc-style file plus a script and an
// optional cleanup script from disk, POSTs them to the external-executor's
// `/run-job` SSE endpoint, and streams the resulting log/done/error events
// to stdout as they arrive.
//
// .envrc parsing is intentionally simple: it only understands lines of the
// form `export NAME=VALUE` (the common direnv convention) and ignores
// blank lines / comments. Quoted values have their surrounding quotes
// stripped. Anything more exotic (command substitution, `source_up`, etc.)
// is not evaluated — this is a plain text scan, not a shell.

use std::fs;
use std::path::Path;
use std::process::exit;

use futures_util::StreamExt;
use reqwest::Client;
use serde::Serialize;
use serde_json::Value;

const DEFAULT_EXECUTOR_URL: &str = "https://api.gingersociety.org/external-executor/run-job";

#[derive(Debug, Serialize)]
struct EnvVar {
    name: String,
    value: String,
}

#[derive(Debug, Serialize)]
struct RunJobRequest {
    capability: String,
    script: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cleanup_script: Option<String>,
    env: Vec<EnvVar>,
}

/// Entry point for the `rpc` subcommand.
///
/// `envrc_path`    — path to a .envrc-style file (export NAME=VALUE lines)
/// `script_path`   — path to the script to run
/// `cleanup_path`  — optional path to a cleanup script
/// `capability`    — device capability to target (e.g. "unix")
pub async fn run_rpc(
    envrc_path: &str,
    script_path: &str,
    cleanup_path: Option<&str>,
    capability: &str,
) {
    let env = match parse_envrc(Path::new(envrc_path)) {
        Ok(env) => env,
        Err(e) => {
            eprintln!("rpc: failed to read envrc '{}': {}", envrc_path, e);
            exit(1);
        }
    };

    let script = match fs::read_to_string(script_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("rpc: failed to read script '{}': {}", script_path, e);
            exit(1);
        }
    };

    let cleanup_script = match cleanup_path {
        Some(p) => match fs::read_to_string(p) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("rpc: failed to read cleanup script '{}': {}", p, e);
                exit(1);
            }
        },
        None => None,
    };

    let executor_url = std::env::var("EXTERNAL_EXECUTOR_URL")
        .unwrap_or_else(|_| DEFAULT_EXECUTOR_URL.to_string());

    let request = RunJobRequest {
        capability: capability.to_string(),
        script,
        cleanup_script,
        env,
    };

    if let Err(e) = stream_job(&executor_url, &request).await {
        eprintln!("rpc: {}", e);
        exit(1);
    }
}

/// POST the job and stream SSE `data: {...}` frames to stdout until a
/// `done` or `error` event closes the job.
async fn stream_job(url: &str, request: &RunJobRequest) -> Result<(), String> {
    // Note: we deliberately do NOT call .timeout() here. reqwest's
    // Client::builder().timeout() applies to the whole request including
    // the time spent reading the body — and this is a long-lived SSE
    // stream, so any finite timeout would eventually kill it mid-job.
    // (Duration::from_secs(0) is NOT "no timeout" — it's an instant
    // timeout, which is what caused every request to fail immediately.)
    let client = Client::builder()
        .build()
        .map_err(|e| format!("failed to build http client: {e}"))?;

    let response = client
        .post(url)
        .header("Content-Type", "application/json")
        .json(request)
        .send()
        .await
        .map_err(|e| format!("request to '{url}' failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("executor returned {status}: {body}"));
    }

    let mut stream = response.bytes_stream();
    let mut buf = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("stream read error: {e}"))?;
        buf.push_str(&String::from_utf8_lossy(&chunk));

        // SSE frames are separated by blank lines; drain complete lines
        // as they arrive so we don't wait for the whole buffer.
        while let Some(idx) = buf.find('\n') {
            let line = buf[..idx].trim_end_matches('\r').to_string();
            buf.drain(..=idx);

            if line.is_empty() {
                continue; // blank line between SSE frames
            }
            let Some(data) = line.strip_prefix("data:") else {
                continue; // ignore SSE comment lines (e.g. the leading ":")
            };
            let data = data.trim_start();

            let event: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("rpc: could not parse event '{data}': {e}");
                    continue;
                }
            };

            let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

            match event_type {
                "log" => {
                    let stream_name = event.get("stream").and_then(|v| v.as_str()).unwrap_or("stdout");
                    let line_text = event.get("line").and_then(|v| v.as_str()).unwrap_or("");
                    if stream_name == "stderr" {
                        eprintln!("{}", line_text);
                    } else {
                        println!("{}", line_text);
                    }
                }
                "done" => {
                    let exit_code = event.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(0);
                    if exit_code != 0 {
                        return Err(format!("job exited with code {exit_code}"));
                    }
                    return Ok(());
                }
                "error" => {
                    let message = event
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    return Err(format!("job error: {message}"));
                }
                _ => {
                    // Unrecognized event shape — print raw for visibility.
                    println!("rpc: {data}");
                }
            }
        }
    }

    Err("stream ended before a 'done' or 'error' event was received".to_string())
}

/// Parse a .envrc-style file. Only handles `export NAME=VALUE` lines
/// (optionally with surrounding single/double quotes on VALUE);
/// blank lines and `#` comments are skipped. Anything else is ignored.
fn parse_envrc(path: &Path) -> Result<Vec<EnvVar>, String> {
    let contents = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut vars = Vec::new();

    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let rest = match line.strip_prefix("export ") {
            Some(r) => r.trim(),
            None => continue, // not an export line; skip rather than guess
        };

        let Some((name, value)) = rest.split_once('=') else {
            continue;
        };

        let name = name.trim();
        if name.is_empty() {
            continue;
        }

        let mut value = value.trim();
        if (value.starts_with('"') && value.ends_with('"') && value.len() >= 2)
            || (value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2)
        {
            value = &value[1..value.len() - 1];
        }

        vars.push(EnvVar {
            name: name.to_string(),
            value: value.to_string(),
        });
    }

    Ok(vars)
}