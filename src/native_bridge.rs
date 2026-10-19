//! Authenticated local callbacks for native hooks and wrapper-owned MCP tools.
use crate::*;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use std::sync::Mutex;

type Replies = Mutex<BTreeMap<String, oneshot::Sender<Value>>>;
struct Shared {
    id: String,
    token: String,
    tools: Vec<Value>,
    events: mpsc::UnboundedSender<Result<Value>>,
    replies: Replies,
    commands: Mutex<Vec<Value>>,
}
pub struct Bridge {
    shared: Arc<Shared>,
    pub address: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Bridge {
    fn drop(&mut self) {
        self.task.abort();
        self.shared.replies.lock().unwrap().clear();
    }
}
impl Bridge {
    pub async fn open(
        id: &str,
        tools: Vec<Value>,
        events: mpsc::UnboundedSender<Result<Value>>,
    ) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = format!("http://{}", listener.local_addr()?);
        let shared = Arc::new(Shared {
            id: id.into(),
            token: uuid::Uuid::new_v4().to_string(),
            tools,
            events,
            replies: Default::default(),
            commands: Default::default(),
        });
        let app = Router::new()
            .route("/hook", post(hook))
            .route("/mcp", post(mcp))
            .route(&format!("/budget/{}", shared.token), post(budget_hook))
            .layer(axum::extract::DefaultBodyLimit::max(20 * 1024 * 1024))
            .with_state(shared.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self {
            shared,
            address,
            task,
        })
    }
    pub fn endpoint(&self) -> Value {
        json!({"url":self.address,"token":self.shared.token})
    }
    pub fn mcp_config(&self) -> Value {
        json!({"serverName":"harness_tools","serverUrl":format!("{}/mcp",self.address),"headers":{"Authorization":format!("Bearer {}",self.shared.token)},"disableStandaloneSse":true,"forceAllToolsEager":true})
    }
    pub fn budget_provider(&self) -> Value {
        json!({"name":"app_server_budget","url":format!("{}/budget/{}",self.address,self.shared.token),
            "protocol":1,"functionCallTimeoutSeconds":30,"functions":[{"name":"budget_gate","hook":{}}]})
    }
    pub fn reply(&self, key: &str, value: Value) -> Result<()> {
        self.shared
            .replies
            .lock()
            .unwrap()
            .remove(key)
            .context("expired native callback")?
            .send(value)
            .map_err(|_| anyhow::anyhow!("native callback was canceled"))
    }
    pub fn command(&self, modified: &Value) -> Option<Value> {
        self.shared
            .commands
            .lock()
            .unwrap()
            .iter()
            .find(|c| c["modified"] == *modified)
            .map(|c| c["call"].clone())
    }
    pub fn cancel(&self) {
        self.shared.replies.lock().unwrap().clear();
    }
}
async fn budget_hook(
    State(shared): State<Arc<Shared>>,
    Json(message): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let Some(id) = message.get("id") else {
        return (StatusCode::ACCEPTED, Json(Value::Null));
    };
    let result = match message["method"].as_str().unwrap_or("") {
        "initialize" => {
            json!({"protocolVersion":message["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"budget-provider","version":"1"}})
        }
        "tools/list" => {
            json!({"tools":[{"name":"budget_gate","description":"Check invocation budget","inputSchema":{"type":"object","additionalProperties":true}}]})
        }
        "tools/call" => {
            let key = uuid::Uuid::new_v4().to_string();
            let valid = message["params"]["name"] == "budget_gate"
                && message["params"]["arguments"]["common"]["conversationId"] == shared.id;
            let response = if valid {
                tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    callback(
                        &shared,
                        &key,
                        json!({"budgetHookRequest":{"requestId":key}}),
                    ),
                )
                .await
                .ok()
                .and_then(Result::ok)
            } else {
                None
            };
            shared.replies.lock().unwrap().remove(&key);
            json!({"content":[{"type":"text","text":response.unwrap_or(json!({"terminationBehavior":"terminate"})).to_string()}]})
        }
        _ => json!({}),
    };
    (
        StatusCode::OK,
        Json(json!({"jsonrpc":"2.0","id":id,"result":result})),
    )
}
fn authorized(shared: &Shared, headers: &HeaderMap) -> bool {
    headers.get("authorization").and_then(|v| v.to_str().ok())
        == Some(&format!("Bearer {}", shared.token))
}
async fn callback(shared: &Shared, key: &str, event: Value) -> Result<Value> {
    let (tx, rx) = oneshot::channel();
    shared.replies.lock().unwrap().insert(key.into(), tx);
    if shared.events.send(Ok(event)).is_err() {
        shared.replies.lock().unwrap().remove(key);
        bail!("wrapper disconnected");
    }
    let result = tokio::time::timeout(std::time::Duration::from_secs(3500), rx).await;
    shared.replies.lock().unwrap().remove(key);
    result
        .context("approval callback timed out")?
        .context("callback canceled")
}
async fn hook(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if !authorized(&shared, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"decision":"deny","reason":"Unauthorized hook callback"})),
        );
    }
    let result=async {
        ensure!(body["conversationId"]==shared.id,"hook conversation mismatch");
        let key=uuid::Uuid::new_v4().to_string();
        let name=text(&body["toolCall"],"name")?;
        let call=json!({"callId":uuid::Uuid::new_v4().to_string(),"trajectoryId":shared.id,"stepIndex":body["stepIdx"],"toolName":name,"serverName":"","argumentsJson":body["toolCall"]["args"].to_string()});
        let response=callback(&shared,&key,json!({"callHookRequest":{"requestId":key,"preToolArgs":call}})).await?;
        let result=&response["preToolResult"];
        ensure!(result["decision"]=="ALLOW","{}",result["reason"].as_str().unwrap_or("Hook denied command"));
        if name!="run_command" { return Ok(json!({"decision":"allow"})); }
        let mut modified=json!({});
        for field in result["modifiedArgs"]["fields"].as_array().context("command hook did not bind execution arguments")? {
            let value=&field["value"];
            let value=value.get("stringValue").or(value.get("boolValue")).or(value.get("numberValue")).context("invalid hook argument")?;
            modified[text(field,"name")?]=value.clone();
        }
        ensure!(modified["CommandLine"].is_string(),"missing bound command");
        let mut bound=call;
        bound["argumentsJson"]=json!(modified.to_string());
        shared.commands.lock().unwrap().push(json!({"modified":modified,"call":bound}));
        Ok::<_,anyhow::Error>(json!({"decision":"allow","overwrite":modified}))
    }.await;
    (
        StatusCode::OK,
        Json(result.unwrap_or_else(|e| json!({"decision":"deny","reason":e.to_string()}))),
    )
}
async fn mcp(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    Json(message): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if !authorized(&shared, &headers) {
        return (StatusCode::UNAUTHORIZED, Json(Value::Null));
    }
    let Some(id) = message.get("id") else {
        return (StatusCode::ACCEPTED, Json(Value::Null));
    };
    let result=async {
        match message["method"].as_str().unwrap_or("") {
            "initialize"=>Ok(json!({"protocolVersion":message["params"]["protocolVersion"],"serverInfo":{"name":"harness-tools","version":"1"},"capabilities":{"tools":{}}})),
            "ping"=>Ok(json!({})),
            "tools/list"=>Ok(json!({"tools":shared.tools.iter().map(|t|json!({"name":t["name"],"description":t["description"],"inputSchema":serde_json::from_str::<Value>(t["parametersJsonSchema"].as_str().unwrap_or("{}")).unwrap_or(json!({}))})).collect::<Vec<_>>()})),
            "tools/call"=>{
                let name=text(&message["params"],"name")?;
                ensure!(shared.tools.iter().any(|t|t["name"]==name),"unknown wrapper tool");
                let key=uuid::Uuid::new_v4().to_string();
                let response=callback(&shared,&key,json!({"toolCall":{"id":key,"name":name,"trajectoryId":shared.id,"argumentsJson":message["params"]["arguments"].to_string()}})).await?;
                let error=response["errorMessage"].as_str();
                let text=error.or(response["responseJson"].as_str()).unwrap_or("{}");
                Ok(json!({"content":[{"type":"text","text":text}],"isError":error.is_some()}))
            }
            _=>bail!("unsupported MCP method"),
        }
    }.await;
    (
        StatusCode::OK,
        Json(match result {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err(e) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":e.to_string()}})
            }
        }),
    )
}

/// Invoked by the native PreToolUse hook. Failure always produces a deny result.
pub async fn helper_main() -> Result<bool> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().map(String::as_str) != Some("--app-core-hook") {
        return Ok(false);
    }
    let result = async {
        ensure!(args.len() == 2, "invalid hook invocation");
        let endpoint: Value = serde_json::from_slice(&std::fs::read(&args[1])?)?;
        let mut body = Vec::new();
        use tokio::io::AsyncReadExt;
        tokio::io::stdin()
            .take(20 * 1024 * 1024 + 1)
            .read_to_end(&mut body)
            .await?;
        ensure!(body.len() <= 20 * 1024 * 1024, "hook payload too large");
        let body: Value = serde_json::from_slice(&body)?;
        let url = reqwest::Url::parse(text(&endpoint, "url")?)?;
        ensure!(
            url.scheme() == "http" && url.host_str() == Some("127.0.0.1"),
            "invalid local callback endpoint"
        );
        Ok::<_, anyhow::Error>(
            reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(3510))
                .build()?
                .post(format!("{url}hook"))
                .bearer_auth(text(&endpoint, "token")?)
                .json(&body)
                .send()
                .await?
                .error_for_status()?
                .json::<Value>()
                .await?,
        )
    }
    .await;
    println!(
        "{}",
        result.unwrap_or_else(
            |e| json!({"decision":"deny","reason":format!("Wrapper hook unavailable: {e}")})
        )
    );
    Ok(true)
}
