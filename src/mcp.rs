use crate::*;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use std::{process::Stdio, time::Duration};
use tokio::{
    process::{Child, ChildStdin, ChildStdout},
    sync::Mutex,
};

enum Transport {
    Stdio {
        child: Child,
        input: ChildStdin,
        output: tokio::io::Lines<BufReader<ChildStdout>>,
    },
    Http {
        client: reqwest::Client,
        url: String,
        headers: reqwest::header::HeaderMap,
        session: Option<String>,
    },
}
struct Peer {
    transport: Transport,
    next: u64,
    failed: bool,
    timeout: Duration,
}
impl Peer {
    async fn open(config: &Value, cwd: &std::path::Path) -> Result<Self> {
        let transport = if let Some(command) = config["command"].as_str() {
            let mut child = tokio::process::Command::new(command);
            child.env_clear();
            for key in ["PATH", "HOME", "TMPDIR", "LANG", "SYSTEMROOT"] {
                if let Ok(value) = std::env::var(key) {
                    child.env(key, value);
                }
            }
            if let Some(names) = config["env_vars"].as_array() {
                for name in names {
                    let name = name.as_str().context("MCP env_vars must contain strings")?;
                    child.env(
                        name,
                        std::env::var(name)
                            .context("missing configured MCP environment variable")?,
                    );
                }
            }
            child
                .current_dir(cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            if let Some(args) = config["args"].as_array() {
                for arg in args {
                    child.arg(arg.as_str().context("MCP argument must be string")?);
                }
            }
            if let Some(env) = config["env"].as_object() {
                for (key, value) in env {
                    child.env(key, value.as_str().context("MCP env value must be string")?);
                }
            }
            let mut child = child.spawn().context("MCP process failed to start")?;
            Transport::Stdio {
                input: child.stdin.take().unwrap(),
                output: BufReader::new(child.stdout.take().unwrap()).lines(),
                child,
            }
        } else {
            let url = text(config, "url")?.to_owned();
            let mut headers = reqwest::header::HeaderMap::new();
            for (field, environment) in [("http_headers", false), ("env_http_headers", true)] {
                if let Some(values) = config[field].as_object() {
                    for (key, value) in values {
                        let value = text(&json!({"value":value}), "value")?.to_owned();
                        let value = if environment {
                            std::env::var(value)
                                .context("missing MCP header environment variable")?
                        } else {
                            value
                        };
                        headers.insert(
                            reqwest::header::HeaderName::from_bytes(key.as_bytes())?,
                            value.parse()?,
                        );
                    }
                }
            }
            if let Some(name) = config["bearer_token_env_var"].as_str() {
                headers.insert(
                    reqwest::header::AUTHORIZATION,
                    format!(
                        "Bearer {}",
                        std::env::var(name).context("missing MCP token environment variable")?
                    )
                    .parse()?,
                );
            }
            headers.insert(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream".parse()?,
            );
            headers.insert("mcp-protocol-version", "2024-11-05".parse()?);
            Transport::Http {
                client: reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()?,
                url,
                headers,
                session: None,
            }
        };
        let seconds = config["tool_timeout_sec"]
            .as_u64()
            .unwrap_or(60)
            .clamp(1, 300);
        let mut peer = Self {
            transport,
            next: 0,
            failed: false,
            timeout: Duration::from_secs(seconds),
        };
        peer.call("initialize",json!({"protocolVersion":"2024-11-05","clientInfo":{"name":"antigravity-harness","version":"0.1"},"capabilities":{}})).await?;
        peer.exchange(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await?;
        Ok(peer)
    }
    async fn exchange(&mut self, message: Value) -> Result<Value> {
        match &mut self.transport {
            Transport::Stdio {
                input,
                output,
                child,
            } => {
                ensure!(child.try_wait()?.is_none(), "MCP process exited");
                input.write_all(format!("{message}\n").as_bytes()).await?;
                input.flush().await?;
                if message.get("id").is_none() {
                    return Ok(Value::Null);
                }
                loop {
                    let line = output
                        .next_line()
                        .await?
                        .context("MCP process disconnected; outcome unknown")?;
                    ensure!(line.len() <= 8_000_000, "MCP frame exceeds limit");
                    let response: Value = serde_json::from_str(&line)?;
                    if response.get("method").is_some() {
                        if response.get("id").is_some() {
                            // No elicitation capability was advertised: fail closed, never synthesize approval.
                            input.write_all(format!("{}\n",json!({"jsonrpc":"2.0","id":response["id"],"error":{"code":-32601,"message":"Client request capability unavailable"}})).as_bytes()).await?;
                            input.flush().await?;
                        }
                        continue;
                    }
                    ensure!(
                        response["id"] == message["id"],
                        "unexpected MCP response ID"
                    );
                    return Ok(response);
                }
            }
            Transport::Http {
                client,
                url,
                headers,
                session,
            } => {
                let mut headers = headers.clone();
                if let Some(session) = session {
                    headers.insert("mcp-session-id", session.parse()?);
                }
                let response = client
                    .post(url.as_str())
                    .headers(headers)
                    .json(&message)
                    .send()
                    .await
                    .context("MCP HTTP request failed; outcome unknown")?;
                ensure!(
                    response.status().is_success(),
                    "MCP HTTP status {}",
                    response.status()
                );
                if let Some(id) = response.headers().get("mcp-session-id") {
                    *session = Some(id.to_str()?.to_owned());
                }
                if message.get("id").is_none() {
                    return Ok(Value::Null);
                }
                let sse = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .is_some_and(|v| v.to_str().unwrap_or("").starts_with("text/event-stream"));
                let bytes = response.bytes().await?;
                ensure!(bytes.len() <= 8_000_000, "MCP response exceeds limit");
                if !sse {
                    return Ok(serde_json::from_slice(&bytes)?);
                }
                let body = std::str::from_utf8(&bytes)?;
                for block in body.replace("\r\n", "\n").split("\n\n") {
                    let data = block
                        .lines()
                        .filter_map(|l| l.strip_prefix("data:").map(str::trim_start))
                        .collect::<Vec<_>>()
                        .join("\n");
                    if let Ok(value) = serde_json::from_str::<Value>(&data)
                        && value["id"] == message["id"]
                        && value.get("method").is_none()
                    {
                        return Ok(value);
                    }
                }
                bail!("MCP stream ended without matching response; outcome unknown")
            }
        }
    }
    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        ensure!(
            !self.failed,
            "MCP connection unavailable; previous request will not be replayed"
        );
        self.next += 1;
        let request = json!({"jsonrpc":"2.0","id":self.next,"method":method,"params":params});
        let result = tokio::time::timeout(self.timeout, self.exchange(request)).await;
        let response = match result {
            Ok(Ok(response)) => response,
            error => {
                self.failed = true;
                return match error {
                    Ok(Err(e)) => Err(e),
                    _ => Err(anyhow::anyhow!(
                        "MCP request timed out; outcome unknown, not retried"
                    )),
                };
            }
        };
        ensure!(response["id"] == self.next, "MCP response ID mismatch");
        if response.get("error").is_some() {
            bail!("MCP returned error code {}", response["error"]["code"]);
        }
        ensure!(response.get("result").is_some(), "MCP result missing");
        Ok(response["result"].clone())
    }
}
struct Entry {
    peer: Mutex<Peer>,
    config: Value,
}
pub struct Hub {
    interrupted: std::sync::atomic::AtomicBool,
    execution_epoch: std::sync::atomic::AtomicU64,
    control: Option<mpsc::UnboundedSender<goal::Action>>,
    thread_id: String,
    token: String,
    peers: BTreeMap<String, Entry>,
    failures: BTreeMap<String, String>,
    address: String,
    task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}
impl Drop for Hub {
    fn drop(&mut self) {
        if let Some(task) = self.task.get_mut().unwrap().take() {
            task.abort();
        }
    }
}
impl Hub {
    pub async fn open(
        thread_id: &str,
        configs: &Value,
        cwd: &std::path::Path,
    ) -> Result<Arc<Self>> {
        Self::with_control(thread_id, configs, cwd, None).await
    }
    pub async fn with_control(
        thread_id: &str,
        configs: &Value,
        cwd: &std::path::Path,
        control: Option<mpsc::UnboundedSender<goal::Action>>,
    ) -> Result<Arc<Self>> {
        let mut peers = BTreeMap::new();
        let mut failures = BTreeMap::new();
        if let Some(configs) = configs.as_object() {
            for (name, config) in configs {
                ensure!(name != "harness_goal", "reserved MCP server name");
                ensure!(
                    name.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                    "invalid MCP server name"
                );
                if config["enabled"] == false {
                    continue;
                }
                match Peer::open(config, cwd).await {
                    Ok(peer) => {
                        peers.insert(
                            name.clone(),
                            Entry {
                                peer: Mutex::new(peer),
                                config: config.clone(),
                            },
                        );
                    }
                    Err(_) => {
                        failures.insert(
                            name.clone(),
                            "MCP startup failed; check server configuration and authentication"
                                .into(),
                        );
                    }
                }
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let hub = Arc::new(Self {
            interrupted: std::sync::atomic::AtomicBool::new(false),
            execution_epoch: std::sync::atomic::AtomicU64::new(0),
            control,
            thread_id: thread_id.into(),
            token: uuid::Uuid::new_v4().to_string(),
            peers,
            failures,
            address: format!("http://{}", listener.local_addr()?),
            task: std::sync::Mutex::new(None),
        });
        // Weak routing ownership lets unloading the session terminate the listener and child servers.
        let app = Router::new()
            .route("/mcp/{server}", post(proxy))
            .with_state(Arc::downgrade(&hub));
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        *hub.task.lock().unwrap() = Some(task);
        Ok(hub)
    }
    pub fn native(&self) -> Value {
        let mut names = self.peers.keys().map(String::as_str).collect::<Vec<_>>();
        if self.control.is_some() {
            names.push("harness_goal");
        }
        json!(names.iter().map(|name|json!({"name":name,"http":{"url":format!("{}/mcp/{name}",self.address),"headers":{"Authorization":format!("Bearer {}",self.token)}}})).collect::<Vec<_>>())
    }
    pub fn set_interrupted(&self, value: bool) {
        if value {
            self.execution_epoch
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        self.interrupted
            .store(value, std::sync::atomic::Ordering::SeqCst);
    }
    fn check_interrupted(&self, method: &str) -> Result<()> {
        ensure!(
            !["tools/call", "resources/read"].contains(&method)
                || !self.interrupted.load(std::sync::atomic::Ordering::SeqCst),
            "turn interrupted; new MCP execution is disabled"
        );
        Ok(())
    }
    fn allowed(entry: &Entry, name: &str) -> bool {
        let enabled = entry.config["enabled_tools"]
            .as_array()
            .is_none_or(|tools| tools.iter().any(|t| t == name));
        enabled
            && !entry.config["disabled_tools"]
                .as_array()
                .is_some_and(|tools| tools.iter().any(|t| t == name))
    }
    pub async fn call(&self, server: &str, method: &str, mut params: Value) -> Result<Value> {
        let execution_epoch = self
            .execution_epoch
            .load(std::sync::atomic::Ordering::SeqCst);
        self.check_interrupted(method)?;
        if server == "harness_goal" {
            let control = self.control.as_ref().context("goal tools unavailable")?;
            return match method {
                "tools/list" => Ok(json!({"tools":[goal::tool()]})),
                "resources/list" => Ok(json!({"resources":[]})),
                "resources/templates/list" => Ok(json!({"resourceTemplates":[]})),
                "ping" => Ok(json!({})),
                "tools/call" => {
                    ensure!(params["name"] == "goal_update", "unknown goal tool");
                    let (reply, wait) = oneshot::channel();
                    control
                        .send(goal::Action {
                            thread_id: self.thread_id.clone(),
                            params: params["arguments"].clone(),
                            reply,
                        })
                        .context("goal controller stopped")?;
                    tokio::time::timeout(Duration::from_secs(30), wait)
                        .await
                        .context("goal update timed out; query state before retry")?
                        .context("goal controller stopped")?
                }
                _ => bail!("unsupported goal MCP method"),
            };
        }
        let entry = self
            .peers
            .get(server)
            .context("MCP server is not available")?;
        if method == "tools/call" {
            ensure!(
                Self::allowed(entry, text(&params, "name")?),
                "MCP tool disabled"
            );
            ensure!(
                !self.thread_id.is_empty(),
                "MCP tool calls require a thread"
            );
        }
        if !params.is_object() {
            params = json!({});
        }
        if !params["_meta"].is_object() {
            params["_meta"] = json!({});
        }
        let prefix = std::env::var("AGY_CLIENT_THREAD_PREFIX").unwrap_or_default();
        params["_meta"]["threadId"] = json!(format!("{prefix}{}", self.thread_id));
        let mut peer = entry.peer.lock().await;
        self.check_interrupted(method)?;
        ensure!(
            !["tools/call", "resources/read"].contains(&method)
                || execution_epoch
                    == self
                        .execution_epoch
                        .load(std::sync::atomic::Ordering::SeqCst),
            "stale MCP call from an interrupted turn"
        );
        let mut result = peer.call(method, params).await?;
        if method == "tools/list"
            && let Some(tools) = result["tools"].as_array_mut()
        {
            tools.retain(|t| Self::allowed(entry, t["name"].as_str().unwrap_or("")));
        }
        Ok(result)
    }
    pub async fn catalog(&self) -> Result<Value> {
        let mut data = Vec::new();
        if self.control.is_some() {
            data.push(json!({"name":"harness_goal","tools":{"goal_update":goal::tool()},"resources":[],"resourceTemplates":[],"authStatus":"unsupported","status":"ready"}));
        }
        for name in self.peers.keys() {
            let mut tools = serde_json::Map::new();
            let mut cursor = Value::Null;
            let mut pages = std::collections::BTreeSet::new();
            let status = loop {
                let result = match self
                    .call(name, "tools/list", json!({"cursor":cursor}))
                    .await
                {
                    Ok(r) => r,
                    Err(_) => break "failed",
                };
                if let Some(list) = result["tools"].as_array() {
                    for tool in list {
                        if let Some(name) = tool["name"].as_str() {
                            tools.insert(name.into(), tool.clone());
                        }
                    }
                }
                cursor = result["nextCursor"].clone();
                if cursor.is_null() {
                    break "ready";
                }
                if !pages.insert(cursor.to_string()) || pages.len() > 100 {
                    break "failed";
                }
            };
            data.push(json!({"name":name,"tools":tools,"resources":[],"resourceTemplates":[],"authStatus":"unsupported","status":status}));
        }
        for (name, error) in &self.failures {
            data.push(json!({"name":name,"tools":{},"resources":[],"resourceTemplates":[],"authStatus":"unsupported","status":"failed","error":error}));
        }
        Ok(json!({"data":data,"nextCursor":null}))
    }
}
async fn proxy(
    State(hub): State<std::sync::Weak<Hub>>,
    Path(server): Path<String>,
    headers: HeaderMap,
    Json(message): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let Some(hub) = hub.upgrade() else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(Value::Null));
    };
    if headers.get("authorization").and_then(|v| v.to_str().ok())
        != Some(&format!("Bearer {}", hub.token))
    {
        return (StatusCode::UNAUTHORIZED, Json(Value::Null));
    }
    if message.get("id").is_none() {
        return (StatusCode::ACCEPTED, Json(Value::Null));
    }
    let method = message["method"].as_str().unwrap_or("");
    let result = if method == "initialize" {
        Ok(
            json!({"protocolVersion":message["params"]["protocolVersion"],"serverInfo":{"name":"antigravity-mcp-proxy","version":"0.1"},"capabilities":{"tools":{},"resources":{}}}),
        )
    } else if [
        "tools/list",
        "tools/call",
        "resources/list",
        "resources/templates/list",
        "resources/read",
        "ping",
    ]
    .contains(&method)
    {
        hub.call(&server, method, message["params"].clone()).await
    } else {
        Err(anyhow::anyhow!("unsupported MCP method"))
    };
    let response = match result {
        Ok(result) => json!({"jsonrpc":"2.0","id":message["id"],"result":result}),
        Err(error) => {
            json!({"jsonrpc":"2.0","id":message["id"],"error":{"code":-32000,"message":error.to_string()}})
        }
    };
    (StatusCode::OK, Json(response))
}

pub fn launch_config() -> Result<Value> {
    let mut config = json!({});
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "app-server" => {}
            "--listen" => ensure!(
                args.next().as_deref() == Some("stdio://"),
                "only stdio listener supported"
            ),
            "-c" | "--config" => {
                let assignment = args.next().context("missing config assignment")?;
                let (key, value) = assignment
                    .split_once('=')
                    .context("config assignment requires =")?;
                ensure!(
                    key.starts_with("mcp_servers.") || key == "features.goals",
                    "unsupported CLI config key"
                );
                let parsed: toml::Value = toml::from_str(&format!("value={value}"))?;
                let value = serde_json::to_value(&parsed["value"])?;
                let mut target = &mut config;
                let mut parts = key.split('.').peekable();
                while let Some(part) = parts.next() {
                    if parts.peek().is_none() {
                        target[part] = value.clone();
                    } else {
                        if !target[part].is_object() {
                            target[part] = json!({});
                        }
                        target = &mut target[part];
                    }
                }
            }
            _ => bail!("unsupported launch argument: {arg}"),
        }
    }
    Ok(config)
}
