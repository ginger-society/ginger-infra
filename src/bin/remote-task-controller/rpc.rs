use futures_util::StreamExt;
use reqwest::Client;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize, Clone)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Serialize)]
pub struct RunJobRequest {
    pub capability: String,
    pub script: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleanup_script: Option<String>,
    pub env: Vec<EnvVar>,
}

pub async fn stream_rpc_job(
    executor_url: &str,
    request: &RunJobRequest,
) -> Result<i32, String> {
    let client = Client::builder()
        .build()
        .map_err(|e| format!("failed to build http client: {e}"))?;

    let response = client
        .post(executor_url)
        .header("Content-Type", "application/json")
        .json(request)
        .send()
        .await
        .map_err(|e| format!("request to '{executor_url}' failed: {e}"))?;

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

        while let Some(idx) = buf.find('\n') {
            let line = buf[..idx].trim_end_matches('\r').to_string();
            buf.drain(..=idx);

            if line.is_empty() {
                continue;
            }
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim_start();

            let event: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[rpc] could not parse event '{data}': {e}");
                    continue;
                }
            };

            let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

            match event_type {
                "log" => {
                    let stream_name = event
                        .get("stream").and_then(|v| v.as_str()).unwrap_or("stdout");
                    let line_text = event.get("line").and_then(|v| v.as_str()).unwrap_or("");
                    if stream_name == "stderr" {
                        eprintln!("{}", line_text);
                    } else {
                        println!("{}", line_text);
                    }
                }
                "done" => {
                    let exit_code = event
                        .get("exit_code").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                    return Ok(exit_code);
                }
                "error" => {
                    let message = event
                        .get("message").and_then(|v| v.as_str()).unwrap_or("unknown error");
                    return Err(format!("job error: {message}"));
                }
                _ => {
                    println!("[rpc] {data}");
                }
            }
        }
    }

    Err("stream ended before a 'done' or 'error' event was received".to_string())
}