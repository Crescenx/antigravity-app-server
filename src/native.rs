//! App 2.24.0 language server transport. Public app-server semantics stay in Server.
use crate::*;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, VecDeque},
    path::Path,
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::AsyncBufReadExt,
    process::{Child, Command},
};

pub const APP_SHA256: &str = "4a3854e31d77200ccb1cd4c44cb5c01611c07d81006586b40c1ab1f2c9e80713";
const SERVICE: &str = "/exa.language_server_pb.LanguageServerService/";
const MAX_FRAME: usize = 64 * 1024 * 1024;
pub fn verify_binary(path: &Path) -> Result<()> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash)?;
    ensure!(
        format!("{:x}", hash.finalize()) == APP_SHA256,
        "unsupported App core binary; expected verified Linux App 2.24.0 language_server"
    );
    Ok(())
}
pub fn binary() -> Result<PathBuf> {
    std::env::var_os("AGY_APP_CORE_BINARY").map(PathBuf::from).context("set AGY_APP_CORE_BINARY to the verified language_server; AGY_EXTERNAL_BINARY is no longer an execution backend")?.canonicalize().map_err(Into::into)
}
pub fn auth_home(root: &Path) -> PathBuf {
    let path = std::env::var_os("AGY_APP_AUTH_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("app-auth"));
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir().unwrap().join(path)
    }
}
fn client() -> Result<reqwest::Client> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .build()?;
    let _ = CLIENT.set(client.clone());
    Ok(client)
}
pub async fn rpc(endpoint: &Value, method: &str, body: Value) -> Result<Value> {
    let base = text(endpoint, "baseUrl")?;
    let url = reqwest::Url::parse(base)?;
    ensure!(
        url.scheme() == "http" && url.host_str() == Some("127.0.0.1"),
        "App core endpoint must be local HTTP"
    );
    let response = client()?
        .post(format!("{base}{SERVICE}{method}"))
        .header("x-codeium-csrf-token", text(endpoint, "csrf")?)
        .header("Connect-Protocol-Version", "1")
        .timeout(Duration::from_secs(45))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("App core {method} transport failed"))?;
    let status = response.status();
    let value: Value = response
        .json()
        .await
        .with_context(|| format!("invalid App core {method} response"))?;
    ensure!(
        status.is_success(),
        "App core {method}: {}",
        value["message"].as_str().unwrap_or("RPC rejected")
    );
    Ok(value)
}

pub struct Process {
    child: Child,
    pub endpoint: Value,
    log_task: tokio::task::JoinHandle<()>,
}
impl Drop for Process {
    fn drop(&mut self) {
        self.log_task.abort();
    }
}
impl Process {
    pub async fn open(binary: &Path, data: &Path, auth_home: &Path) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(data)?;
        std::fs::set_permissions(data, std::fs::Permissions::from_mode(0o700))?;
        std::fs::create_dir_all(auth_home.join(".gemini"))?;
        std::fs::set_permissions(auth_home, std::fs::Permissions::from_mode(0o700))?;
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let csrf = uuid::Uuid::new_v4().to_string();
        let endpoint =
            json!({"baseUrl":format!("http://127.0.0.1:{port}"),"csrf":csrf,"authHome":auth_home});
        let mut command = Command::new(binary);
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if name.starts_with("BASH_FUNC_")
                || [
                    "BASH_ENV",
                    "ENV",
                    "SHELLOPTS",
                    "BASHOPTS",
                    "CDPATH",
                    "GLOBIGNORE",
                    "LD_PRELOAD",
                    "LD_LIBRARY_PATH",
                    "GOOGLE_APPLICATION_CREDENTIALS",
                    "GEMINI_API_KEY",
                    "GOOGLE_API_KEY",
                ]
                .contains(&name.as_ref())
            {
                command.env_remove(key);
            }
        }
        command
            .current_dir(data)
            .env("HOME", auth_home)
            .env("XDG_CONFIG_HOME", auth_home.join("config"))
            .env("XDG_DATA_HOME", auth_home.join("data"))
            .env("XDG_CACHE_HOME", auth_home.join("cache"));
        command
            .args([
                "--standalone",
                "--subclient_type=hub",
                "--override_ide_name=antigravity",
                "--override_ide_version=2.24.0",
                "--override_user_agent_name=antigravity",
                "--exit_on_stdin_close",
                "--disable_telemetry",
                "--use_ls_chrome_devtools_mcp=false",
                "--cloud_code_endpoint=https://daily-cloudcode-pa.googleapis.com",
                "--api_server_url=https://generativelanguage.googleapis.com",
            ])
            .arg(format!("--http_server_port={port}"))
            .arg(format!("--csrf_token={csrf}"))
            .arg(format!("--gemini_dir={}", data.join(".gemini").display()))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().context("starting App core")?;
        let stderr = child.stderr.take().unwrap();
        // Drain private diagnostics; they may contain authentication URLs and must not leak into JSON-RPC stdout.
        let log_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(_)) = lines.next_line().await {}
        });
        let mut process = Self {
            child,
            endpoint,
            log_task,
        };
        for _ in 0..150 {
            if process.child.try_wait()?.is_some() {
                bail!("App core exited during startup");
            }
            if tokio::time::timeout(
                Duration::from_millis(200),
                rpc(&process.endpoint, "GetCapabilities", json!({})),
            )
            .await
            .is_ok_and(|r| r.is_ok())
            {
                return Ok(process);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!("App core startup timed out")
    }
    pub async fn close(mut self) {
        drop(self.child.stdin.take());
        if tokio::time::timeout(Duration::from_secs(3), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
    }
}

pub struct Native {
    process: Process,
    config: Value,
    budget: crate::native_budget::Budget,
    budget_stop: Option<String>,
    state: crate::native_state::NativeState,
    events: mpsc::UnboundedReceiver<Result<Value>>,
    stream_task: tokio::task::JoinHandle<()>,
    bridge: crate::native_bridge::Bridge,
    queued: VecDeque<Value>,
    min_step: Option<usize>,
    saw_input: bool,
    pending_inputs: usize,
    seen_inputs: BTreeSet<usize>,
    pending: BTreeMap<String, Value>,
    interactions: BTreeSet<String>,
}
impl Drop for Native {
    fn drop(&mut self) {
        self.stream_task.abort();
    }
}
impl Native {
    pub fn pid(&self) -> Option<u32> {
        self.process.child.id()
    }
    pub async fn open(binary: &Path, root: &Path, mut config: Value) -> Result<(Self, Value)> {
        crate::native_budget::validate(config.get("budget"))?;
        let id = text(&config, "cascadeId")?.to_owned();
        let data = root.join("app-core").join(&id);
        let auth_home = PathBuf::from(text(&config, "authHome")?);
        let process = Process::open(binary, &data, &auth_home).await?;
        let resumed = config["resume"] == true;
        let snapshot = if resumed {
            let state = rpc(
                &process.endpoint,
                "GetCascadeTrajectory",
                json!({"cascadeId":id}),
            )
            .await?;
            ensure!(
                state["trajectory"]["cascadeId"] == id,
                "App core session identity mismatch"
            );
            state
        } else {
            let created=rpc(&process.endpoint,"StartCascade",json!({"cascadeId":id,"workspaceUris":config["workspaceUris"],"source":1,"trajectoryType":4,"backend":1})).await?;
            ensure!(
                created["cascadeId"] == id,
                "App core session identity mismatch"
            );
            rpc(
                &process.endpoint,
                "GetCascadeTrajectory",
                json!({"cascadeId":id}),
            )
            .await?
        };
        let (events_tx, events) = mpsc::unbounded_channel();
        let tools = config["externalTools"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let bridge =
            crate::native_bridge::Bridge::open(&id, tools.clone(), events_tx.clone()).await?;
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let endpoint_path = data.join("callback.json");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&endpoint_path)?;
        file.write_all(&serde_json::to_vec(&bridge.endpoint())?)?;
        let hook_command = format!(
            "{} --app-core-hook {}",
            permissions::shell_quote(
                std::env::current_exe()?
                    .to_str()
                    .context("executable path")?
            )?,
            permissions::shell_quote(endpoint_path.to_str().context("hook config path")?)?
        );
        let hook_path = data.join("hooks.json");
        std::fs::write(
            &hook_path,
            serde_json::to_vec(
                &json!({"app_server":{"enabled":true,"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":hook_command,"timeout":3600}]}]}}),
            )?,
        )?;
        let has_token_budget = ["maxInputTokens", "maxOutputTokens", "maxTotalTokens"]
            .iter()
            .any(|k| config["budget"].get(k).is_some());
        let planner = &mut config["cascadeConfig"]["plannerConfig"];
        if has_token_budget {
            planner["declarativeMixinConfig"]["providers"] = json!([bridge.budget_provider()]);
            planner["declarativeMixinConfig"]["postInvocationHooks"]
                .as_array_mut()
                .context("native post hooks")?
                .insert(
                    0,
                    json!({"name":"budget_gate","providerName":"app_server_budget"}),
                );
        }
        planner["customizationDiscoveryConfig"]["hooks"] =
            json!({"inheritUser":false,"hooksPaths":[{"absolute":hook_path}]});
        if !tools.is_empty() {
            planner["customizationDiscoveryConfig"]["mcp"]["servers"]
                .as_array_mut()
                .context("MCP discovery config")?
                .push(bridge.mcp_config());
        }
        // Native approval remains mandatory even if a missing/empty hook were interpreted as allow.
        planner["toolConfig"]["permissionConfig"] = json!({"permissionsV2":true,"effectivePermissionPreset":2,"effectiveGrants":{"allow":["read_file(*)"],"ask":["command(*)","write_file(*)","read_url(*)","mcp(*)"]},"defaultGrants":{"ask":["command(*)","write_file(*)","read_url(*)","mcp(*)"]}});
        let catalog = rpc(&process.endpoint, "GetAvailableModels", json!({})).await?;
        let model = text(&config, "modelName")?;
        let model_id = catalog["response"]["models"][model]["model"].clone();
        ensure!(
            !model_id.is_null(),
            "App core account does not advertise model {model}; log in with --login or select an available model"
        );
        config["cascadeConfig"]["plannerConfig"]["planModel"] = model_id;
        let mut state = crate::native_state::NativeState::new(&id);
        let initial_events = state.snapshot(&snapshot);
        let usage = initial_events
            .iter()
            .find_map(|e| e.get("usageUpdate"))
            .cloned()
            .unwrap_or(json!({"modelCallCount":0}));
        let historical_tools = snapshot["trajectory"]["steps"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s["metadata"]["toolCall"].is_object())
            .count() as u64;
        let budget = crate::native_budget::Budget::open(
            &data,
            config.get("budget"),
            &usage,
            historical_tools,
        )?;
        let endpoint = process.endpoint.clone();
        let stream_id = id.clone();
        let stream_task = tokio::spawn(async move {
            if let Err(e) = stream(&endpoint, &stream_id, events_tx.clone()).await {
                let _ = events_tx.send(Err(e));
            }
        });
        Ok((
            Self {
                process,
                config,
                budget,
                budget_stop: None,
                state,
                events,
                stream_task,
                bridge,
                queued: VecDeque::new(),
                min_step: None,
                saw_input: false,
                pending_inputs: 0,
                seen_inputs: Default::default(),
                pending: Default::default(),
                interactions: Default::default(),
            },
            json!({"initializeConversationResponse":{"cascadeId":id,"runtime":"app-core-v1"}}),
        ))
    }
    pub async fn send(&mut self, event: Value) -> Result<()> {
        let id = text(&self.config, "cascadeId")?.to_owned();
        if let Some(input) = event.get("userInput") {
            let starting = self.min_step.is_none();
            if starting {
                if let Some(cap) = self.budget.prepare_turn()? {
                    self.config["cascadeConfig"]["executorConfig"]["maxGeneratorInvocations"] =
                        json!(cap);
                }
                self.budget_stop = None;
                let snapshot = rpc(
                    &self.process.endpoint,
                    "GetCascadeTrajectory",
                    json!({"cascadeId":id}),
                )
                .await?;
                self.state.begin_turn(&snapshot);
                self.min_step = Some(snapshot["numTotalSteps"].as_u64().unwrap_or_else(|| {
                    snapshot["trajectory"]["steps"]
                        .as_array()
                        .map_or(0, |s| s.len() as u64)
                }) as usize);
                self.saw_input = false;
                self.seen_inputs.clear();
            }
            let mut items = Vec::new();
            let mut media = Vec::new();
            for part in input["parts"].as_array().context("missing input parts")? {
                if let Some(text) = part["text"].as_str() {
                    items.push(json!({"text":text}));
                } else if let Some(data) = part.get("media") {
                    media.push(json!({"mimeType":data["mimeType"],"inlineData":data["data"],"description":data["description"].as_str().unwrap_or("")}));
                } else {
                    bail!("unsupported native input part");
                }
            }
            let mut body = json!({"cascadeId":id,"items":items,"media":media,"cascadeConfig":self.config["cascadeConfig"],"deliveryStrategy":if starting {0}else{1}});
            if let Some(tags) = event.get("tags") {
                body["tags"] = tags.clone();
            }
            rpc(&self.process.endpoint, "SendUserCascadeMessage", body).await?;
            self.pending_inputs += 1;
            self.queued
                .retain(|event| event["trajectoryStateUpdate"]["state"] != "STATE_FULLY_IDLE");
        } else if event.get("haltRequest").is_some() {
            self.bridge.cancel();
            self.pending.clear();
            rpc(
                &self.process.endpoint,
                "CancelCascadeInvocation",
                json!({"cascadeId":id,"killBackgroundTasks":true}),
            )
            .await?;
        } else if let Some(response) = event.get("callHookResponse") {
            self.bridge
                .reply(text(response, "requestId")?, response.clone())?;
        } else if let Some(response) = event.get("toolResponse") {
            self.bridge.reply(text(response, "id")?, response.clone())?;
        } else if let Some(response) = event.get("policyDecisionResponse") {
            let pending = self
                .pending
                .remove(text(response, "requestId")?)
                .context("expired native permission request")?;
            rpc(&self.process.endpoint,"HandleCascadeUserInteraction",json!({"cascadeId":id,"interaction":{"trajectoryId":pending["trajectoryId"],"stepIndex":pending["stepIndex"],"permission":{"allow":response["outcome"]=="POLICY_EVALUATION_OUTCOME_ALLOW","scope":1,"userDenyInstruction":response["denyReason"].as_str().unwrap_or("")}}})).await?;
        } else if let Some(response) = event.get("questionResponse") {
            let pending = self
                .pending
                .remove(&format!("question:{}", response["stepIndex"]))
                .context("expired native question")?;
            let mut questions = pending["questions"].as_array().cloned().unwrap_or_default();
            for (index, question) in questions.iter_mut().enumerate() {
                let answer = &response["response"]["answers"][index];
                let selected = answer["multipleChoiceAnswer"]["selectedChoiceIndices"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|i| i.as_u64())
                    .filter_map(|i| question["options"][i as usize].get("id").cloned())
                    .collect::<Vec<_>>();
                question["selectedOptionIds"] = json!(selected);
                question["writeInResponse"] = json!(
                    answer["multipleChoiceAnswer"]["freeformResponse"]
                        .as_str()
                        .unwrap_or("")
                );
                question["skipped"] = json!(answer["unanswered"] == true);
            }
            rpc(&self.process.endpoint,"HandleCascadeUserInteraction",json!({"cascadeId":id,"interaction":{"trajectoryId":pending["trajectoryId"],"stepIndex":pending["stepIndex"],"askQuestion":{"responses":questions,"cancelled":false}}})).await?;
        } else {
            bail!("unsupported App core operation");
        }
        Ok(())
    }
    fn permission(&mut self, step: &Value) -> Option<Value> {
        let interaction = &step["requestedInteraction"];
        interaction.get("permission")?;
        let native_id = step["nativeTrajectoryId"].as_str()?;
        let index = step["stepIndex"].as_u64()?;
        let address = format!("{native_id}:{index}");
        if !self.interactions.insert(address.clone()) {
            return None;
        }
        let raw = &step["rawNativeStep"];
        let call = raw["metadata"]
            .get("modifiedToolCall")
            .unwrap_or(&raw["metadata"]["toolCall"]);
        let args: Value = serde_json::from_str(call["argumentsJson"].as_str().unwrap_or("{}"))
            .unwrap_or(json!({}));
        let name = raw["metadata"]["toolCall"]["name"].as_str().unwrap_or("");
        let tool_args = if name == "run_command" {
            self.bridge.command(&args).unwrap_or_else(||json!({"toolName":"run_command","callId":"unbound","trajectoryId":self.config["cascadeId"],"stepIndex":index,"argumentsJson":args.to_string()}))
        } else {
            let name = name.strip_prefix("mcp_harness_tools_").unwrap_or(name);
            json!({"toolName":name,"serverName":"","callId":call["id"],"trajectoryId":self.config["cascadeId"],"stepIndex":index,"argumentsJson":args.to_string()})
        };
        let key = format!("permission:{address}");
        self.pending.insert(
            key.clone(),
            json!({"trajectoryId":native_id,"stepIndex":index}),
        );
        Some(
            json!({"policyDecisionRequest":{"requestId":key,"toolArgs":tool_args,"nativeInteraction":interaction}}),
        )
    }
    pub async fn next(&mut self) -> Result<Value> {
        loop {
            if let Some(event) = self.queued.pop_front() {
                if ["STATE_FULLY_IDLE", "STATE_CANCELLED"].contains(
                    &event["trajectoryStateUpdate"]["state"]
                        .as_str()
                        .unwrap_or(""),
                ) {
                    self.min_step = None;
                }
                return Ok(event);
            }
            let event = self
                .events
                .recv()
                .await
                .context("App core stream closed")??;
            if event.get("streamReset").is_some() {
                continue;
            }
            if let Some(update) = event.get("coreUpdate") {
                for mut event in self.state.apply(update) {
                    if let Some(usage) = event.get("usageUpdate") {
                        self.budget.observe_usage(usage)?;
                    }
                    if let Some(step) = event.get_mut("stepUpdate") {
                        let call = &step["metadata"]["toolCall"];
                        if let Some(name) = call["name"].as_str() {
                            let server=self.config["cascadeConfig"]["plannerConfig"]["customizationDiscoveryConfig"]["mcp"]["servers"].as_array().into_iter().flatten()
                                .filter_map(|s|s["serverName"].as_str()).filter(|s|*s!="harness_tools" && name.starts_with(&format!("mcp_{s}_"))).max_by_key(|s|s.len());
                            if let Some(server) = server {
                                step["mcpTool"] = json!({"serverName":server,"toolName":&name[server.len()+5..],"argumentsJson":call["argumentsJson"]});
                                step["text"] = step["generic"]["result"]["result"].clone();
                            }
                        }
                        let Some(min) = self.min_step else {
                            continue;
                        };
                        if step["trajectoryId"] == self.config["cascadeId"] {
                            if step["stepIndex"].as_u64().unwrap_or(0) < min as u64 {
                                continue;
                            }
                            if step["source"] == "SOURCE_USER"
                                && self
                                    .seen_inputs
                                    .insert(step["stepIndex"].as_u64().unwrap_or(0) as usize)
                            {
                                self.saw_input = true;
                                self.pending_inputs = self.pending_inputs.saturating_sub(1);
                            }
                            if let Some(permission) = self.permission(step) {
                                self.queued.push_back(permission);
                            }
                            if let Some(questions) =
                                step["requestedInteraction"]["askQuestion"]["questions"]
                                    .as_array()
                                    .cloned()
                            {
                                let key = format!("question:{}", step["stepIndex"]);
                                self.pending.insert(key,json!({"trajectoryId":step["nativeTrajectoryId"],"stepIndex":step["stepIndex"],"questions":questions}));
                                step["questionsRequest"] = json!({"questions":questions.iter().map(|q|json!({"multipleChoice":{"question":q["question"],"choices":q["options"].as_array().into_iter().flatten().map(|o|o["text"].clone()).collect::<Vec<_>>()}})).collect::<Vec<_>>()});
                            }
                        }
                    }
                    if let Some(state) = event.get_mut("trajectoryStateUpdate") {
                        if self.min_step.is_none() || !self.saw_input {
                            continue;
                        }
                        if state["state"] == "STATE_FULLY_IDLE" && self.pending_inputs > 0 {
                            continue;
                        }
                        if state["state"] == "STATE_FULLY_IDLE"
                            && let Some(reason) = &self.budget_stop
                        {
                            state["error"] = json!({"message":reason});
                            state["budgetLimited"] = json!(true);
                            state["stopReason"] = json!("STOP_REASON_TOKEN_BUDGET_EXCEEDED");
                        }
                    }
                    self.queued.push_back(event);
                }
            } else {
                if let Some(hook) = event.get("budgetHookRequest") {
                    let result = async {
                        let snapshot = rpc(
                            &self.process.endpoint,
                            "GetCascadeTrajectory",
                            json!({"cascadeId":self.config["cascadeId"]}),
                        )
                        .await?;
                        let mut observation =
                            crate::native_state::NativeState::new(text(&self.config, "cascadeId")?);
                        let events = observation.snapshot(&snapshot);
                        let usage = events
                            .iter()
                            .find_map(|e| e.get("usageUpdate"))
                            .context("native budget usage unavailable")?;
                        self.budget.observe_usage(usage)?;
                        Ok::<_, anyhow::Error>(self.budget.token_stop_reason())
                    }
                    .await;
                    self.budget_stop = match result {
                        Ok(reason) => reason,
                        Err(e) => Some(format!("Token budget observation failed: {e}")),
                    };
                    self.bridge.reply(
                        text(hook, "requestId")?,
                        if self.budget_stop.is_some() {
                            json!({"terminationBehavior":"terminate"})
                        } else {
                            json!({})
                        },
                    )?;
                    continue;
                }
                if let Some(hook) = event.get("callHookRequest") {
                    let call = &hook["preToolArgs"];
                    let key = format!("{}:{}", call["trajectoryId"], call["stepIndex"]);
                    if let Err(error) = self.budget.claim_tool(&key) {
                        self.bridge.reply(
                            text(hook, "requestId")?,
                            json!({"preToolResult":{"decision":"DENY","reason":error.to_string()}}),
                        )?;
                        rpc(&self.process.endpoint,"CancelCascadeInvocation",json!({"cascadeId":self.config["cascadeId"],"killBackgroundTasks":true})).await?;
                        self.min_step = None;
                        self.pending_inputs = 0;
                        return Ok(
                            json!({"trajectoryStateUpdate":{"trajectoryId":self.config["cascadeId"],"state":"STATE_FULLY_IDLE","fullyIdle":true,"error":{"message":error.to_string()},"budgetLimited":true}}),
                        );
                    }
                }
                return Ok(event);
            }
        }
    }
    pub async fn close(mut self) {
        self.bridge.cancel();
        self.stream_task.abort();
        let id = self.config["cascadeId"].clone();
        if self.min_step.is_some() {
            let _ = rpc(
                &self.process.endpoint,
                "CancelCascadeInvocation",
                json!({"cascadeId":id,"killBackgroundTasks":true}),
            )
            .await;
        }
        drop(self.process.child.stdin.take());
        if tokio::time::timeout(Duration::from_secs(3), self.process.child.wait())
            .await
            .is_err()
        {
            let _ = self.process.child.kill().await;
        }
    }
}
async fn stream(
    endpoint: &Value,
    id: &str,
    events: mpsc::UnboundedSender<Result<Value>>,
) -> Result<()> {
    // A stream is observational only. Reconnects never resend user input.
    for attempt in 0..=3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _ = events.send(Ok(json!({"streamReset":true})));
        }
        let result = stream_once(endpoint, id, &events).await;
        if events.is_closed() {
            return Ok(());
        }
        if attempt == 3 {
            return result;
        }
    }
    unreachable!()
}
async fn stream_once(
    endpoint: &Value,
    id: &str,
    events: &mpsc::UnboundedSender<Result<Value>>,
) -> Result<()> {
    let body = serde_json::to_vec(
        &json!({"conversationId":id,"subscriberId":uuid::Uuid::new_v4().to_string(),"trajectoryVerbosity":3,"initialStepsPageBounds":{"startIndex":0},"initialGeneratorMetadatasPageBounds":{"startIndex":0},"initialExecutorMetadatasPageBounds":{"startIndex":0}}),
    )?;
    let mut frame = vec![0];
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend(body);
    let response = client()?
        .post(format!(
            "{}{SERVICE}StreamAgentStateUpdates",
            text(endpoint, "baseUrl")?
        ))
        .header("Content-Type", "application/connect+json")
        .header("Connect-Protocol-Version", "1")
        .header("x-codeium-csrf-token", text(endpoint, "csrf")?)
        .body(frame)
        .send()
        .await?
        .error_for_status()?;
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    while let Some(chunk) = stream.next().await {
        buffer.extend_from_slice(&chunk?);
        loop {
            if buffer.len() < 5 {
                break;
            }
            let size = u32::from_be_bytes(buffer[1..5].try_into().unwrap()) as usize;
            ensure!(size <= MAX_FRAME, "App core state frame exceeds 64 MiB");
            if buffer.len() < size + 5 {
                break;
            }
            let flags = buffer[0];
            let value: Value = serde_json::from_slice(&buffer[5..5 + size])?;
            buffer.drain(..5 + size);
            if flags == 2 {
                bail!(
                    "App core state stream ended: {}",
                    value["error"]["message"]
                        .as_str()
                        .unwrap_or("connection closed")
                );
            }
            ensure!(flags == 0, "unsupported App core stream compression");
            events
                .send(Ok(json!({"coreUpdate":value})))
                .map_err(|_| anyhow::anyhow!("state consumer closed"))?;
        }
    }
    bail!("App core state stream disconnected")
}
