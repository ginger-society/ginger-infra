// src/wamp_client.rs
//
// Declarative WAMP-like client for the GingerSociety notification broker.
//
// Usage (callee / service):
// ─────────────────────────────────
//   let mut client = WampClient::new("ginger_infra", access_token, realm).await;
//
//   client.register("snap_install", |args, kwargs| async move {
//       let package = args[0].as_str().unwrap_or("").to_string();
//       Ok(serde_json::json!({"status": "installed", "package": package}))
//   });
//
//   client.listen().await;
//
// Usage (caller / any other service):
// ─────────────────────────────────────
//   let client = WampClient::new("my_service", access_token, realm).await;
//
//   let result = client.call(
//       "ginger_infra",          // target service prefix
//       workspace_id,            // target workspace realm
//       "snap_install",          // function name
//       json!(["certbot"]),      // args
//       json!({"classic": true}),// kwargs
//   ).await?;
//
// Usage (fire-and-forget publish):
// ─────────────────────────────────
//   // listen() must be running (in a spawned task) before calling publish()
//   client.publish("some.topic", json!({"key": "value"})).await?;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{oneshot, Mutex};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use std::sync::atomic::{AtomicBool, Ordering};


// ── WAMP-style message types (mirrors broker's requests.rs) ──────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WampPublish {
    pub message_type: u8,
    pub request_id: u64,
    pub options: WampPublishOptions,
    pub topic: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kwargs: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WampPublishOptions {
    #[serde(default)]
    pub acknowledge: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WampEvent {
    pub message_type: u8,
    pub subscription_id: String,
    pub publication_id: u64,
    pub details: WampEventDetails,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kwargs: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WampEventDetails {
    pub timestamp: String,
    pub topic: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokerError {
    pub message_type: u8,
    pub error: String,
    pub correlation_id: Option<String>,
    pub topic: Option<String>,
}

// ── handler type ──────────────────────────────────────────────────────────────

pub type HandlerResult = Result<Value, String>;
pub type Handler = Arc<
    dyn Fn(Option<Value>, Option<Value>) -> Pin<Box<dyn Future<Output = HandlerResult> + Send>>
        + Send
        + Sync,
>;
// ── pending call tracker (for caller side) ───────────────────────────────────

type PendingCalls = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>>;

// ── outbound publish channel ──────────────────────────────────────────────────

type PublishTx = Arc<Mutex<Option<tokio::sync::mpsc::Sender<WampPublish>>>>;

// ── WampClient ────────────────────────────────────────────────────────

pub struct WampClient {
    /// Service prefix e.g. "ginger_infra"
    prefix: String,
    /// Token sub — workspace/user id
    realm: String,
    /// Full channel name = "{prefix}_{sub}"
    channel: String,
    /// WebSocket URL
    url: String,
    /// Registered RPC handlers (callee side)
    handlers: Arc<Mutex<HashMap<String, Handler>>>,
    /// Pending outbound calls waiting for a reply (caller side)
    pending: PendingCalls,
    /// Sender half of the outbound publish channel; None until listen() starts
    publish_tx: PublishTx,
    /// True while the WebSocket connection is alive
    pub is_connected: Arc<AtomicBool>,
}

impl WampClient {
    /// Create a new client.
    /// `prefix`       — service name e.g. "ginger_infra"
    /// `access_token` — JWT used to authenticate the WebSocket
    /// `realm`        — token subject (workspace id); channel = "{prefix}_{sub}"
    pub fn new(prefix: &str, access_token: &str, realm: &str) -> Self {
        let broker_url = std::env::var("NOTIFICATION_BROKER_URL")
            .unwrap_or_else(|_| "wss://api.gingersociety.org".to_string());

        let channel = format!("{}_{}", prefix, realm);
        let url = format!(
            "{}/notification/ws/{}?token={}",
            broker_url, channel, access_token
        );

        Self {
            prefix: prefix.to_string(),
            realm: realm.to_string(),
            channel,
            url,
            handlers: Arc::new(Mutex::new(HashMap::new())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            publish_tx: Arc::new(Mutex::new(None)),
            is_connected: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.is_connected.load(Ordering::Relaxed)
    }

    /// Register an RPC handler for a function name.
    /// The handler receives (args, kwargs) and returns a JSON result.
    pub async fn register<F, Fut>(&self, function: &str, handler: F)
    where
        F: Fn(Option<Value>, Option<Value>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HandlerResult> + Send + 'static,
    {
        let boxed: Handler = Arc::new(move |args, kwargs| Box::pin(handler(args, kwargs)));
        self.handlers
            .lock()
            .await
            .insert(function.to_string(), boxed);
        println!("[client] registered handler for '{}'", function);
    }

    /// Fire-and-forget publish to any topic.
    /// `listen()` must be running (e.g. in a spawned task) before calling this.
    pub async fn publish(&self, topic: &str, kwargs: Value) -> Result<(), String> {
        if !self.is_connected() {
            return Err("not connected".to_string());
        }
        let guard = self.publish_tx.lock().await;
        let tx = guard
            .as_ref()
            .ok_or_else(|| "not connected — call listen() first".to_string())?;

        let msg = WampPublish {
            message_type: 16,
            request_id: rand_u64(),
            options: WampPublishOptions {
                acknowledge: false,
                correlation_id: None,
                reply_to: None,
            },
            topic: topic.to_string(),
            args: None,
            kwargs: Some(kwargs),
        };

        tx.send(msg).await.map_err(|e| e.to_string())
    }

    /// Start listening — blocks until shutdown signal.
    /// Dispatches incoming RPC calls to registered handlers and sends results back.
    /// Also drains the outbound publish channel onto the websocket.
    pub async fn listen(&self) {
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

        // signal handler
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{signal, SignalKind};
                let mut sigterm =
                    signal(SignalKind::terminate()).expect("Failed to bind SIGTERM");
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = sigterm.recv() => {}
                }
            }
            #[cfg(not(unix))]
            {
                tokio::signal::ctrl_c()
                    .await
                    .expect("Failed to listen for ctrl_c");
            }
            println!("\n[client] shutdown signal received.");
            let _ = shutdown_tx.send(true);
        });

        // create the outbound publish channel and store the sender so publish() can use it
        let (pub_tx, mut pub_rx) = tokio::sync::mpsc::channel::<WampPublish>(32);
        *self.publish_tx.lock().await = Some(pub_tx);

        let heartbeat_topic = format!("heartbeat.{}_{}", self.prefix, self.realm);
        let mut attempt: u32 = 0;

        loop {
            if *shutdown_rx.borrow() {
                println!("[client] exiting.");
                return;
            }

            if attempt > 0 {
                let backoff = std::cmp::max(1, std::cmp::min(60, 2_u64.pow(attempt)));
                println!(
                    "[client] reconnecting in {}s... (attempt #{})",
                    backoff,
                    attempt + 1
                );
                tokio::select! {
                    _ = tokio::time::sleep(tokio::time::Duration::from_secs(backoff)) => {}
                    _ = shutdown_rx.changed() => {
                        println!("[client] exiting during backoff.");
                        return;
                    }
                }
            }

            println!("[client] connecting to {}...", self.channel);

            let conn = tokio::time::timeout(
                tokio::time::Duration::from_secs(15),
                connect_async(&self.url),
            )
            .await;

            let mut ws_stream = match conn {
                Ok(Ok((stream, _))) => {
                    println!("✅ [client] connected on channel '{}'", self.channel);
                    self.is_connected.store(true, Ordering::Relaxed);
                    attempt = 0;
                    stream
                }
                Ok(Err(e)) => {
                    eprintln!("[client] connection failed: {:?}", e);
                    attempt += 1;
                    continue;
                }
                Err(_) => {
                    eprintln!("[client] connection timed out after 15s.");
                    attempt += 1;
                    continue;
                }
            };

            let mut ping_interval = tokio::time::interval(
                tokio::time::Duration::from_secs(PING_INTERVAL_SECS),
            );
            ping_interval.tick().await; // discard immediate tick

            let mut heartbeat_interval = tokio::time::interval(
                tokio::time::Duration::from_secs(5),
            );
            heartbeat_interval.tick().await; // discard immediate tick

            let mut waiting_for_pong = false;
            let mut pong_timeout: Option<Pin<Box<tokio::time::Sleep>>> = None;

            loop {
                tokio::select! {
                    biased;

                    _ = shutdown_rx.changed() => {
                        println!("[client] shutting down, closing connection...");
                        self.is_connected.store(false, Ordering::Relaxed);
                        let _ = ws_stream.close(None).await;
                        while let Some(Ok(msg)) = ws_stream.next().await {
                            if msg.is_close() { break; }
                        }
                        return;
                    }

                    _ = ping_interval.tick() => {
                        if waiting_for_pong {
                            eprintln!("[client] pong not received — reconnecting...");
                            break;
                        }
                        if let Err(e) = ws_stream.send(Message::Ping(vec![].into())).await {
                            eprintln!("[client] ping failed: {:?} — reconnecting...", e);
                            break;
                        }
                        waiting_for_pong = true;
                        pong_timeout = Some(Box::pin(tokio::time::sleep(
                            tokio::time::Duration::from_secs(PING_TIMEOUT_SECS),
                        )));
                    }

                    _ = async {
                        if let Some(ref mut t) = pong_timeout { t.await }
                        else { std::future::pending::<()>().await }
                    } => {
                        eprintln!("[client] pong timeout — reconnecting...");
                        break;
                    }

                    _ = heartbeat_interval.tick() => {
                        let stats = crate::heartbeat::collect_stats();
                        let publish = WampPublish {
                            message_type: 16,
                            request_id: rand_u64(),
                            options: WampPublishOptions {
                                acknowledge: false,
                                correlation_id: None,
                                reply_to: None,
                            },
                            topic: heartbeat_topic.clone(),
                            args: None,
                            kwargs: Some(stats),
                        };
                        let msg = serde_json::to_string(&publish).unwrap();
                        if let Err(e) = ws_stream.send(Message::Text(msg.into())).await {
                            eprintln!("[client] heartbeat publish failed: {:?} — reconnecting...", e);
                            break;
                        }
                        println!("[client] ♥ heartbeat → {}", heartbeat_topic);
                    }

                    Some(outbound) = pub_rx.recv() => {
                        let msg = serde_json::to_string(&outbound).unwrap();
                        if let Err(e) = ws_stream.send(Message::Text(msg.into())).await {
                            eprintln!("[client] outbound publish failed: {:?} — reconnecting...", e);
                            break;
                        }
                        println!("[client] → PUBLISH topic='{}'", outbound.topic);
                    }

                    msg_result = ws_stream.next() => {
                        match msg_result {
                            Some(Ok(msg)) => {
                                if msg.is_pong() {
                                    waiting_for_pong = false;
                                    pong_timeout = None;
                                    continue;
                                }
                                if msg.is_close() {
                                    println!("[client] server closed connection — reconnecting...");
                                    break;
                                }
                                if msg.is_ping() { continue; }

                                let text = msg.into_text().unwrap_or_default();
                                if text.is_empty() { continue; }

                                self.handle_message(&text, &mut ws_stream).await;
                            }
                            Some(Err(e)) => {
                                eprintln!("[client] ws error: {:?} — reconnecting...", e);
                                break;
                            }
                            None => {
                                println!("[client] connection dropped — reconnecting...");
                                break;
                            }
                        }
                    }
                }
            }
            self.is_connected.store(false, Ordering::Relaxed);
            attempt += 1;
        }
    }

    /// Call a remote function on another service.
    /// Returns the result value or an error string.
    pub async fn call(
        &self,
        target_prefix: &str,
        target_realm: &str,
        function: &str,
        args: Value,
        kwargs: Value,
    ) -> Result<Value, String> {
        let target_channel = format!("{}_{}", target_prefix, target_realm);
        let correlation_id = format!("corr-{}", uuid());
        let reply_to = self.channel.clone();

        let mut kw = kwargs.as_object().cloned().unwrap_or_default();
        kw.insert("function".to_string(), Value::String(function.to_string()));
        kw.insert("correlation_id".to_string(), Value::String(correlation_id.clone()));
        kw.insert("reply_to".to_string(), Value::String(reply_to.clone()));

        let publish = WampPublish {
            message_type: 16,
            request_id: rand_u64(),
            options: WampPublishOptions {
                acknowledge: false,
                correlation_id: Some(correlation_id.clone()),
                reply_to: Some(reply_to),
            },
            topic: target_channel,
            args: Some(args),
            kwargs: Some(Value::Object(kw)),
        };

        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .await
            .insert(correlation_id.clone(), tx);

        println!(
            "[client] → CALL fn='{}' corr={} target='{}'",
            function, correlation_id, publish.topic
        );

        let _ = serde_json::to_string(&publish).unwrap();

        match tokio::time::timeout(tokio::time::Duration::from_secs(30), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("reply channel dropped".to_string()),
            Err(_) => {
                self.pending.lock().await.remove(&correlation_id);
                Err("call timed out".to_string())
            }
        }
    }

    // ── internal message dispatcher ───────────────────────────────────────────

    async fn handle_message(
        &self,
        text: &str,
        ws_stream: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) {
        if let Ok(err) = serde_json::from_str::<BrokerError>(text) {
            if err.message_type == 0 {
                println!(
                    "[client] ← BROKER ERROR: {} corr={:?}",
                    err.error, err.correlation_id
                );
                if let Some(corr_id) = &err.correlation_id {
                    if let Some(tx) = self.pending.lock().await.remove(corr_id) {
                        let _ = tx.send(Err(err.error.clone()));
                    }
                }
                return;
            }
        }

        if let Ok(event) = serde_json::from_str::<WampEvent>(text) {
            let corr_id = event.details.correlation_id.clone();
            let reply_to = event.details.reply_to.clone();
            let function = event.kwargs
                .as_ref()
                .and_then(|kw| kw.get("function"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            match function {
                Some(fn_name) => {
                    self.dispatch_rpc(
                        fn_name,
                        event.args.clone(),
                        event.kwargs.clone(),
                        corr_id,
                        reply_to,
                        ws_stream,
                    )
                    .await;
                }
                None => {
                    if let Some(ref cid) = corr_id {
                        if let Some(tx) = self.pending.lock().await.remove(cid) {
                            let result = event.kwargs
                                .or(event.args)
                                .unwrap_or(Value::Null);
                            let _ = tx.send(Ok(result));
                        }
                    } else {
                        println!("[client] ← EVENT {}", text);
                    }
                }
            }
            return;
        }

        println!("[client] ← UNKNOWN {}", text);
    }

    async fn dispatch_rpc(
        &self,
        function: String,
        args: Option<Value>,
        kwargs: Option<Value>,
        correlation_id: Option<String>,
        reply_to: Option<String>,
        ws_stream: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) {
        let handler = self.handlers.lock().await.get(&function).cloned();

        match handler {
            None => {
                eprintln!("[client] no handler registered for '{}'", function);
                if let (Some(corr_id), Some(rt)) = (correlation_id, reply_to) {
                    let reply = make_reply(
                        &self.channel,
                        &rt,
                        &corr_id,
                        serde_json::json!({"error": format!("no handler for '{}'", function)}),
                        true,
                    );
                    let _ = ws_stream
                        .send(Message::Text(serde_json::to_string(&reply).unwrap().into()))
                        .await;
                }
            }
            Some(h) => {
                println!("[client] ← CALL fn='{}' corr={:?}", function, correlation_id);
                let result = h(args, kwargs).await;

                if let (Some(corr_id), Some(rt)) = (correlation_id, reply_to) {
                    let payload = match result {
                        Ok(v) => v,
                        Err(e) => serde_json::json!({"error": e}),
                    };
                    let reply = make_reply(&self.channel, &rt, &corr_id, payload, false);
                    let msg = serde_json::to_string(&reply).unwrap();
                    println!("[client] → REPLY fn='{}' corr={}", function, corr_id);
                    let _ = ws_stream.send(Message::Text(msg.into())).await;
                }
            }
        }
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

const PING_INTERVAL_SECS: u64 = 15;
const PING_TIMEOUT_SECS: u64 = 10;

fn make_reply(
    my_channel: &str,
    reply_to: &str,
    correlation_id: &str,
    payload: Value,
    is_error: bool,
) -> WampPublish {
    let mut kw = serde_json::Map::new();
    kw.insert("is_result".to_string(), Value::Bool(true));
    kw.insert(
        "correlation_id".to_string(),
        Value::String(correlation_id.to_string()),
    );
    if is_error {
        kw.insert("is_error".to_string(), Value::Bool(true));
    }

    WampPublish {
        message_type: 16,
        request_id: rand_u64(),
        options: WampPublishOptions {
            acknowledge: false,
            correlation_id: Some(correlation_id.to_string()),
            reply_to: None,
        },
        topic: reply_to.to_string(),
        args: None,
        kwargs: Some(Value::Object(
            kw.into_iter()
                .chain(
                    payload
                        .as_object()
                        .cloned()
                        .unwrap_or_default()
                        .into_iter(),
                )
                .collect(),
        )),
    }
}

fn rand_u64() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u64
}

fn uuid() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

#[macro_export]
macro_rules! wamp_args {
    ($args:expr) => {
        $args
            .as_ref()
            .and_then(|a| a.get(0))
            .ok_or_else(|| "missing args".to_string())
            .and_then(|v| {
                serde_json::from_value(v.clone())
                    .map_err(|e| format!("invalid args: {}", e))
            })
    };
}