mod codex;
mod compat;
mod execution;
mod goal;
mod guardian;
mod input;
mod instructions;
mod mcp;
mod media;
mod models;
mod native;
mod native_auth;
mod native_bridge;
mod native_budget;
mod native_commands;
mod native_config;
mod native_state;
mod permissions;
mod session_options;
mod skills;
mod steer;
mod store;
mod subagents;
#[cfg(test)]
mod tests;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use store::{Store, Thread};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
};

type Output = mpsc::UnboundedSender<Value>;
const METHODS: &[&str] = &[
    "initialize",
    "thread/start",
    "thread/list",
    "thread/read",
    "thread/resume",
    "thread/configure",
    "thread/unload",
    "thread/reconcile",
    "thread/inject_items",
    "turn/start",
    "turn/steer",
    "turn/interrupt",
    "thread/loaded/list",
    "thread/settings/update",
    "thread/name/set",
    "thread/archive",
    "thread/unarchive",
    "collaborationMode/list",
    "thread/goal/get",
    "thread/goal/set",
    "thread/goal/clear",
    "model/list",
    "config/read",
    "guardian/status",
    "account/read",
    "account/rateLimits/read",
    "skills/list",
    "skills/config/write",
    "mcpServerStatus/list",
    "mcpServer/tool/call",
    "mcpServer/resources/list",
    "mcpServer/resource/read",
];
enum Command {
    Send(Value, oneshot::Sender<Result<()>>),
    Close(oneshot::Sender<()>),
}
struct RuntimeEvent {
    id: String,
    generation: String,
    event: Result<Value>,
}
struct Connection {
    pid: Option<u32>,
    generation: String,
    commands: mpsc::Sender<Command>,
}
struct Server {
    store: Arc<Store>,
    binary: PathBuf,
    endpoint: Value,
    model_catalog: Option<(std::time::Instant, models::Catalog)>,
    output: Output,
    events: mpsc::UnboundedSender<RuntimeEvent>,
    connections: BTreeMap<String, Connection>,
    approvals: BTreeMap<String, (String, Value)>,
    grants: std::collections::BTreeSet<(String, String)>,
    tool_jobs: BTreeMap<(String, String), tokio::task::JoinHandle<()>>,
    reviews: BTreeMap<String, guardian::PendingReview>,
    mcp: BTreeMap<String, Arc<mcp::Hub>>,
    config: Value,
    control: mpsc::UnboundedSender<goal::Action>,
    agent_waits: BTreeMap<(String, String), std::time::Instant>,
    initialized: bool,
}

fn text<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .filter(|s| !s.is_empty())
        .with_context(|| format!("missing {field}"))
}
fn notify(output: &Output, method: &str, params: Value) {
    let _ = output.send(json!({"jsonrpc":"2.0","method":method,"params":params}));
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn recover_interrupted_thread(thread: &mut Thread) -> bool {
    if thread.status != "unknown"
        || !thread
            .turn
            .as_ref()
            .is_some_and(|t| t["interruptRequested"] == true)
    {
        return false;
    }
    thread.status = "idle".into();
    thread.revision += 1;
    thread.checkpoint_turn();
    true
}

impl Server {
    async fn configure_thread(&mut self, thread: &mut Thread, params: &Value) -> Result<()> {
        let selecting = !params["model"].is_null()
            || !params["collaborationMode"]["settings"]["model"].is_null()
            || models::effort(params).is_some_and(|v| !v.is_null());
        if selecting && self.model_catalog.is_none() {
            self.model_catalog = Some((
                std::time::Instant::now(),
                models::fetch(&self.endpoint).await?,
            ));
        }
        let mut params = params.clone();
        if selecting && let Some((_, catalog)) = &self.model_catalog {
            params["model"] = json!(catalog.resolve(&thread.model, &params)?);
        }
        configure(thread, &params)
    }
    async fn connect(&mut self, thread: &Thread, resume: bool) -> Result<Value> {
        thread.executable()?;
        ensure!(
            !self.connections.contains_key(&thread.id),
            "thread already connected"
        );
        let permissions = thread.permissions()?;
        permissions::verify(&permissions, &thread.cwd).await?;
        // MCP tool annotations are advisory, not an enforceable read-only boundary.
        // Do not even start external servers while planning.
        let plan_servers = json!({});
        let hub = mcp::Hub::with_control(
            &thread.id,
            if thread.is_plan() || thread.options.tools_disabled {
                &plan_servers
            } else {
                &thread.mcp_servers
            },
            &thread.cwd,
            if thread.is_plan() || thread.options.tools_disabled {
                None
            } else {
                Some(self.control.clone())
            },
        )
        .await?;
        let native_root = self.store.native_root(&thread.id);
        let mut config = native_config::build(&self.store, thread, hub.native())?;
        config["authHome"] = self.endpoint["authHome"].clone();
        config["resume"] = json!(resume);
        let (mut native, init) = native::Native::open(&self.binary, &native_root, config).await?;
        if init["initializeConversationResponse"]["cascadeId"] != thread.id {
            native.close().await;
            bail!("native session identity mismatch");
        }
        let (tx, mut rx) = mpsc::channel(16);
        let generation = uuid::Uuid::new_v4().to_string();
        self.connections.insert(
            thread.id.clone(),
            Connection {
                pid: native.pid(),
                generation: generation.clone(),
                commands: tx,
            },
        );
        self.mcp.insert(thread.id.clone(), hub);
        let id = thread.id.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    command = rx.recv() => match command {
                        Some(Command::Send(value, reply)) => {
                            let result = native.send(value).await;
                            let failed = result.is_err();
                            let _ = reply.send(result);
                            if failed { break; }
                        }
                        Some(Command::Close(reply)) => { native.close().await; let _ = reply.send(()); return; }
                        None => break,
                    },
                    event = native.next() => {
                        let failed = event.is_err();
                        if events.send(RuntimeEvent { id:id.clone(), generation:generation.clone(), event }).is_err() || failed { break; }
                    }
                }
            }
            let _ = events.send(RuntimeEvent {
                id,
                generation,
                event: Err(anyhow::anyhow!("native connection ended")),
            });
            native.close().await;
        });
        Ok(init["initializeConversationResponse"].clone())
    }
    async fn send(&self, id: &str, event: Value) -> Result<()> {
        let tx = self
            .connections
            .get(id)
            .context("thread must be resumed first")?;
        let (reply, wait) = oneshot::channel();
        tx.commands
            .send(Command::Send(event, reply))
            .await
            .context("native worker stopped")?;
        wait.await
            .context("native worker stopped before acknowledgment")??;
        Ok(())
    }
    async fn disconnect(&mut self, id: &str) {
        match self.store.load(id) {
            Ok(mut thread) => {
                if let Err(error) = self.cancel_tools(
                    &mut thread,
                    "Execution interrupted by disconnect; outcome unknown, not replayed",
                ) {
                    eprintln!("tool cancellation checkpoint failed: {error}");
                }
            }
            Err(_) => self.abort_tools(id),
        }
        if let Some(tx) = self.connections.remove(id) {
            let (reply, wait) = oneshot::channel();
            let _ = tx.commands.send(Command::Close(reply)).await;
            let _ = wait.await;
        }
        self.approvals.retain(|_, (thread, _)| thread != id);
        self.grants.retain(|(thread, _)| thread != id);
        self.mcp.remove(id);
    }
    async fn request(&mut self, method: &str, p: Value) -> Result<Value> {
        if method == "initialize" {
            ensure!(!self.initialized, "already initialized");
            self.initialized = true;
            return Ok(
                json!({"serverInfo":{"name":"antigravity-app-server","version":env!("CARGO_PKG_VERSION")},
                "methods":METHODS,
                "capabilities":{"protocol":"app-server-subset-v1","nativeEvents":true,"toolApproval":true,"acp":false}}),
            );
        }
        ensure!(self.initialized, "initialize first");
        if matches!(
            method,
            "turn/start"
                | "turn/steer"
                | "turn/interrupt"
                | "thread/configure"
                | "thread/settings/update"
                | "thread/inject_items"
                | "thread/reconcile"
                | "thread/goal/set"
                | "thread/goal/clear"
                | "mcpServer/tool/call"
        ) {
            self.store.load(text(&p, "threadId")?)?.executable()?;
        }
        match method {
            "collaborationMode/list" => Ok(
                json!({"data":[{"mode":"default","name":"Default","model":null,"reasoning_effort":null},{"mode":"plan","name":"Plan","model":null,"reasoning_effort":null}]}),
            ),
            "thread/name/set" | "thread/archive" | "thread/unarchive" => {
                let mut thread = self.store.load(text(&p, "threadId")?)?;
                if method == "thread/name/set" {
                    let name = text(&p, "name")?.trim();
                    ensure!(
                        !name.is_empty() && name.len() <= 1024,
                        "invalid thread name"
                    );
                    thread.name = Some(name.into());
                } else {
                    ensure!(
                        thread.executable().is_err() || thread.status == "idle",
                        "archive requires an idle thread"
                    );
                    thread.archived = method == "thread/archive";
                    if thread.archived {
                        self.unload_agent_children(&thread).await?;
                        self.disconnect(&thread.id).await;
                    }
                }
                thread.updated_at = now();
                self.store.save(&thread)?;
                Ok(json!({"thread":compat::thread(&thread)}))
            }
            "mcpServerStatus/list" => {
                let id = p["threadId"].as_str().unwrap_or("");
                if id.is_empty() && !self.mcp.contains_key("") {
                    let hub =
                        mcp::Hub::open("", &self.config["mcp_servers"], &std::env::current_dir()?)
                            .await?;
                    self.mcp.insert("".into(), hub);
                }
                self.mcp
                    .get(id)
                    .context("thread must be loaded for MCP discovery")?
                    .catalog()
                    .await
            }
            "mcpServer/tool/call" | "mcpServer/resources/list" | "mcpServer/resource/read" => {
                let id = text(&p, "threadId")?;
                if method == "mcpServer/tool/call" && p["server"] == "harness_goal" {
                    ensure!(p["tool"] == "goal_update", "unknown goal tool");
                    return self.goal_action(id, &p["arguments"]);
                }
                let hub = self
                    .mcp
                    .get(id)
                    .context("thread must be loaded for MCP calls")?;
                let server = text(&p, "server")?;
                match method {
                    "mcpServer/tool/call" => {
                        hub.call(
                            server,
                            "tools/call",
                            json!({"name":text(&p,"tool")?,"arguments":p["arguments"]}),
                        )
                        .await
                    }
                    "mcpServer/resources/list" => {
                        hub.call(server, "resources/list", json!({"cursor":p["cursor"]}))
                            .await
                    }
                    _ => {
                        hub.call(server, "resources/read", json!({"uri":text(&p,"uri")?}))
                            .await
                    }
                }
            }
            "skills/list" => {
                let cwds = p["cwds"].as_array().context("cwds must be an array")?;
                let mut data = Vec::new();
                for cwd in cwds {
                    let path = PathBuf::from(cwd.as_str().context("cwd must be a string")?);
                    match path.canonicalize().and_then(|p|if p.is_dir(){Ok(p)}else{Err(std::io::Error::other("cwd is not a directory"))}) {
                        Ok(path)=>data.push(skills::discover(&self.store,&path)?),
                        Err(error)=>data.push(json!({"cwd":cwd,"skills":[],"errors":[{"path":cwd,"message":error.to_string()}]})),
                    }
                }
                Ok(json!({"data":data}))
            }
            "skills/config/write" => {
                ensure!(
                    self.connections.is_empty(),
                    "unload active sessions before changing skill configuration"
                );
                let mut cwds = self
                    .store
                    .list()?
                    .into_iter()
                    .map(|t| t.cwd)
                    .collect::<Vec<_>>();
                cwds.push(std::env::current_dir()?);
                skills::write(&self.store, &p, &cwds)
            }
            "model/list" => {
                ensure!(
                    p["limit"].is_null() || p["limit"].as_u64().is_some_and(|v| v > 0),
                    "limit must be a positive integer"
                );
                ensure!(
                    p["includeHidden"].is_null() || p["includeHidden"].is_boolean(),
                    "includeHidden must be boolean"
                );
                if self
                    .model_catalog
                    .as_ref()
                    .is_none_or(|(at, _)| at.elapsed().as_secs() >= 300 && p["cursor"].is_null())
                {
                    self.model_catalog = Some((
                        std::time::Instant::now(),
                        models::fetch(&self.endpoint).await?,
                    ));
                }
                let entries = self
                    .model_catalog
                    .as_ref()
                    .unwrap()
                    .1
                    .entries
                    .iter()
                    .filter(|entry| p["includeHidden"] == true || entry["hidden"] != true)
                    .collect::<Vec<_>>();
                let offset = if p["cursor"].is_null() {
                    0
                } else {
                    text(&p, "cursor")?
                        .parse::<usize>()
                        .context("invalid model cursor")?
                };
                ensure!(offset <= entries.len(), "invalid model cursor");
                let limit = p["limit"].as_u64().unwrap_or(100).clamp(1, 100) as usize;
                let end = (offset + limit).min(entries.len());
                Ok(
                    json!({"data":entries[offset..end],"nextCursor":if end < entries.len() {Some(end.to_string())}else{None}}),
                )
            }
            "thread/inject_items" => {
                let mut thread = self.store.load(text(&p, "threadId")?)?;
                ensure!(
                    thread.status == "idle",
                    "thread must be idle for context injection"
                );
                let items = p["items"].as_array().context("items must be an array")?;
                input::injected(items)?;
                thread.pending_context.extend(items.iter().cloned());
                input::injected(&thread.pending_context)?;
                thread.revision += 1;
                self.store.save(&thread)?;
                Ok(json!({}))
            }
            "account/rateLimits/read" => models::rate_limits(&self.endpoint).await,
            "guardian/status" => {
                let config = if let Some(id) = p["threadId"].as_str() {
                    self.store.load(id)?.review_config
                } else {
                    guardian::ReviewConfig::default()
                };
                Ok(guardian::status(&config).await)
            }
            "config/read" => {
                if self.model_catalog.is_none() {
                    self.model_catalog = Some((
                        std::time::Instant::now(),
                        models::fetch(&self.endpoint).await?,
                    ));
                }
                let catalog = &self.model_catalog.as_ref().unwrap().1;
                let model = &catalog
                    .entries
                    .iter()
                    .find(|m| m["isDefault"] == true)
                    .unwrap_or(&catalog.entries[0])["model"];
                Ok(
                    json!({"config":{"model":model,"model_provider":"google","approval_policy":"on-request","approvals_reviewer":guardian::default_reviewer(),"guardian":guardian::ReviewConfig::default(),"sandbox_mode":"workspace-write"},"origins":{},"layers":[]}),
                )
            }
            "account/read" => Ok(
                json!({"account":null,"requiresOpenaiAuth":false,"provider":"google","authentication":native_auth::status(&self.endpoint,&native::auth_home(&self.store.root)).await}),
            ),
            "thread/goal/get" => {
                let thread = self.store.load(text(&p, "threadId")?)?;
                Ok(json!({"goal":thread.goal}))
            }
            "thread/goal/set" | "thread/goal/clear" => {
                let mut thread = self.store.load(text(&p, "threadId")?)?;
                goal::mutate(&mut thread, &p, method == "thread/goal/clear")?;
                self.store.save(&thread)?;
                goal::notify(&self.output, &thread);
                Ok(json!({"goal":thread.goal}))
            }
            "thread/start" => {
                compat::policy(&p)?;
                let cwd = PathBuf::from(text(&p, "cwd")?).canonicalize()?;
                ensure!(cwd.is_dir(), "cwd must be a directory");
                let approval = p["approvalPolicy"].as_str().unwrap_or("on-request");
                ensure!(
                    ["on-request", "never"].contains(&approval),
                    "unsupported approvalPolicy"
                );
                if self.model_catalog.is_none() {
                    self.model_catalog = Some((
                        std::time::Instant::now(),
                        models::fetch(&self.endpoint).await?,
                    ));
                }
                let catalog = &self.model_catalog.as_ref().unwrap().1;
                let default = catalog
                    .entries
                    .iter()
                    .find(|m| m["isDefault"] == true)
                    .unwrap_or(&catalog.entries[0]);
                let model = catalog.resolve(
                    default["model"].as_str().context("invalid default model")?,
                    &p,
                )?;
                let mut thread = Thread {
                    runtime: Some("app-core-v1".into()),
                    subagents: subagents::State::default(),
                    options: session_options::Options {
                        ephemeral: p["ephemeral"] == true,
                        ..Default::default()
                    },
                    collaboration_mode: "default".into(),
                    id: uuid::Uuid::new_v4().to_string(),
                    cwd,
                    model,
                    pending_context: Vec::new(),
                    name: None,
                    archived: false,
                    permission_policy: None,
                    tool_receipts: BTreeMap::new(),
                    native_command_receipts: BTreeMap::new(),
                    tool_grants: Default::default(),
                    revision: 0,
                    status: "idle".into(),
                    turn: None,
                    approval_policy: approval.into(),
                    approvals_reviewer: guardian::default_reviewer(),
                    review_config: guardian::ReviewConfig::default(),
                    budget: None,
                    additional_directories: Vec::new(),
                    unrestricted: false,
                    usage: Value::Null,
                    token_usage: Value::Null,
                    receipts: BTreeMap::new(),
                    steer_receipts: BTreeMap::new(),
                    turns: Vec::new(),
                    created_at: now(),
                    updated_at: now(),
                    goal: None,
                    mcp_servers: p["config"]
                        .get("mcp_servers")
                        .cloned()
                        .unwrap_or_else(|| self.config["mcp_servers"].clone()),
                };
                self.configure_thread(&mut thread, &p).await?;
                // Reserve identity before native creation so a failed response cannot erase provenance.
                self.store.save(&thread)?;
                let init = match self.connect(&thread, false).await {
                    Ok(init) => init,
                    Err(error) => {
                        self.store.forget_ephemeral(&thread.id);
                        return Err(error);
                    }
                };
                Ok(compat::response(&thread, init))
            }
            "thread/loaded/list" => {
                Ok(json!({"data":self.connections.keys().collect::<Vec<_>>(),"nextCursor":null}))
            }
            "thread/list" => {
                let mut threads = self.store.list()?;
                threads.retain(|t| t.archived == (p["archived"] == true));
                threads.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.id.cmp(&b.id)));
                if let Some(ancestor) = p["ancestorThreadId"].as_str() {
                    threads.retain(|t| t.subagents.parent_id.as_deref() == Some(ancestor));
                }
                if let Some(cwd) = p["cwd"].as_str() {
                    threads.retain(|t| t.cwd == *cwd);
                }
                if let Some(kinds) = p["sourceKinds"].as_array().filter(|a| !a.is_empty()) {
                    threads.retain(|t| {
                        kinds.iter().any(|v| {
                            (v == "subAgent" && t.subagents.parent_id.is_some())
                                || v == if t.subagents.parent_id.is_some() {
                                    "subAgentThreadSpawn"
                                } else {
                                    "appServer"
                                }
                        })
                    });
                }
                let start = match p["cursor"].as_str() {
                    Some(id) => {
                        threads
                            .iter()
                            .position(|t| t.id == id)
                            .context("invalid cursor")?
                            + 1
                    }
                    None => 0,
                };
                let limit = p["limit"].as_u64().unwrap_or(100).clamp(1, 1000) as usize;
                let page = threads.iter().skip(start).take(limit).collect::<Vec<_>>();
                let cursor = if start + page.len() < threads.len() {
                    page.last().map(|t| t.id.clone())
                } else {
                    None
                };
                Ok(
                    json!({"data":page.iter().map(|t|compat::thread(t)).collect::<Vec<_>>(),"nextCursor":cursor}),
                )
            }
            "thread/read" => {
                Ok(json!({"thread":compat::thread(&self.store.load(text(&p,"threadId")?)?)}))
            }
            "thread/resume" => {
                compat::policy(&p)?;
                let mut thread = self.store.load(text(&p, "threadId")?)?;
                if thread.executable().is_err() {
                    return Ok(compat::response(&thread, Value::Null));
                }
                // A stopped worker must not leave an explicitly cancelled session
                // permanently locked. Preserve the unknown old outcome; never replay it.
                if !self.connections.contains_key(&thread.id)
                    && recover_interrupted_thread(&mut thread)
                {
                    self.store.save(&thread)?;
                    if let Some(turn) = &thread.turn {
                        notify(
                            &self.output,
                            "turn/completed",
                            json!({"threadId":thread.id,"turn":compat::turn(turn)}),
                        );
                    }
                    notify(
                        &self.output,
                        "thread/status/changed",
                        json!({"threadId":thread.id,"status":{"type":"idle"}}),
                    );
                }
                let previous = thread.clone();
                self.configure_thread(&mut thread, &p).await?;
                let changed = serde_json::to_value(&thread)? != serde_json::to_value(&previous)?;
                if self.connections.contains_key(&thread.id) && !changed {
                    return Ok(compat::response(&thread, Value::Null));
                }
                ensure!(
                    thread.status == "idle",
                    "reconcile unknown execution before resuming"
                );
                if changed {
                    self.disconnect(&thread.id).await;
                    thread.revision += 1;
                }
                let init = match self.connect(&thread, true).await {
                    Ok(init) => init,
                    Err(error) => {
                        if changed {
                            let _ = self.connect(&previous, true).await;
                        }
                        return Err(error);
                    }
                };
                self.store.save(&thread)?;
                Ok(compat::response(&thread, init))
            }
            "thread/configure" | "thread/settings/update" => {
                compat::policy(&p)?;
                let mut thread = self.store.load(text(&p, "threadId")?)?;
                let previous = thread.clone();
                ensure!(
                    thread.status == "idle",
                    "configuration requires an idle thread"
                );
                ensure!(
                    method == "thread/settings/update" || p["expectedRevision"] == thread.revision,
                    "revision conflict"
                );
                self.configure_thread(&mut thread, &p).await?;
                if serde_json::to_value(&thread)? == serde_json::to_value(&previous)? {
                    return Ok(compat::response(&thread, Value::Null));
                }
                self.disconnect(&thread.id).await;
                thread.revision += 1;
                let init = match self.connect(&thread, true).await {
                    Ok(init) => init,
                    Err(error) => {
                        // Settings remain durable at their last successful revision.
                        let _ = self.connect(&previous, true).await;
                        return Err(error);
                    }
                };
                self.store.save(&thread)?;
                Ok(compat::response(&thread, init))
            }
            "thread/unload" => {
                let id = text(&p, "threadId")?;
                let thread = self.store.load(id)?;
                ensure!(
                    thread.executable().is_err() || thread.status == "idle",
                    "thread is not idle"
                );
                self.unload_agent_children(&thread).await?;
                self.disconnect(id).await;
                self.store.forget_ephemeral(id);
                Ok(json!({}))
            }
            "turn/start" => {
                let id = text(&p, "threadId")?;
                let mut thread = self.store.load(id)?;
                compat::policy(&p)?;
                let key = p["clientUserMessageId"]
                    .as_str()
                    .or(p["requestId"].as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                ensure!(
                    !thread.steer_receipts.contains_key(&key),
                    "clientUserMessageId already used for turn/steer"
                );
                if let Some(receipt) = thread.receipts.get(&key) {
                    ensure!(
                        receipt["input"] == p["input"]
                            && receipt["additionalContext"] == p["additionalContext"]
                            && receipt["outputSchema"] == p["outputSchema"],
                        "requestId reused with different input"
                    );
                    return Ok(receipt.clone());
                }
                ensure!(
                    thread.status == "idle",
                    "thread is busy or needs reconciliation"
                );
                ensure!(
                    self.connections.contains_key(id),
                    "thread must be resumed first"
                );
                self.agent_capacity(&thread)?;
                let injected = std::mem::take(&mut thread.pending_context);
                let mut parts = input::injected(&injected)?;
                parts.extend(subagents::take_mail(&mut thread));
                parts.extend(input::context(&p)?);
                parts.extend(input::parts(&self.store, &thread, &p["input"])?);
                let previous = thread.clone();
                // Output constraints belong to this turn, not subsequent turns or Goals.
                thread.options.output_schema = None;
                self.configure_thread(&mut thread, &p).await?;
                if serde_json::to_value(&previous)? != serde_json::to_value(&thread)? {
                    self.disconnect(id).await;
                    if let Err(error) = self.connect(&thread, true).await {
                        let _ = self.connect(&previous, true).await;
                        return Err(error);
                    }
                }
                let turn = json!({"id":uuid::Uuid::new_v4().to_string(),"status":"inProgress","items":[],"error":null,"clientUserMessageId":p["clientUserMessageId"],"input":p["input"],"goalId":thread.goal.as_ref().filter(|g| g["status"] == "active" && !thread.is_plan()).map(|g|g["id"].clone()),"startedAt":now()});
                let receipt = json!({"turn":turn,"requestId":key,"input":p["input"],"additionalContext":p["additionalContext"],"outputSchema":p["outputSchema"]});
                thread.turn = Some(turn.clone());
                thread.turn.as_mut().unwrap()["injectedContext"] = json!(injected);
                thread.turn.as_mut().unwrap()["nativeInputText"] =
                    json!(steer::native_text(&parts));
                thread.status = "running".into();
                thread.revision += 1;
                thread.receipts.insert(key, receipt.clone());
                thread.checkpoint_turn();
                self.store.save(&thread)?;
                if let Some(hub) = self.mcp.get(id) {
                    hub.set_interrupted(false);
                }
                if let Err(error) = self.send(id, json!({"userInput":{"parts":parts}})).await {
                    if let Some(hub) = self.mcp.get(id) {
                        hub.set_interrupted(true);
                    }
                    thread.status = "unknown".into();
                    self.store.save(&thread)?;
                    return Err(error);
                }
                notify(
                    &self.output,
                    "turn/started",
                    json!({"threadId":id,"turn":turn}),
                );
                Ok(receipt)
            }
            "turn/steer" => self.steer(&p).await,
            "turn/interrupt" => {
                let id = text(&p, "threadId")?;
                let mut thread = self.store.load(id)?;
                ensure!(thread.status == "running", "turn is not running");
                ensure!(
                    thread.turn.as_ref().and_then(|t| t["id"].as_str())
                        == Some(text(&p, "turnId")?),
                    "turnId mismatch"
                );
                if let Some(hub) = self.mcp.get(id) {
                    hub.set_interrupted(true);
                }
                self.cancel_tools(
                    &mut thread,
                    "Execution interrupted; outcome unknown, not replayed",
                )?;
                if let Some(turn) = &mut thread.turn {
                    turn["interruptRequested"] = json!(true);
                }
                steer::mark_unknown(&mut thread);
                if let Some(goal) = &mut thread.goal
                    && goal["status"] == "active"
                {
                    goal["status"] = json!("paused");
                    goal["reason"] = json!("Turn interrupted by user");
                }
                self.store.save(&thread)?;
                if thread.goal.is_some() {
                    goal::notify(&self.output, &thread);
                }
                self.send(id, json!({"haltRequest":true})).await?;
                self.approvals.retain(|_, (t, _)| t != id);
                self.interrupt_agent_children(&thread).await?;
                Ok(json!({}))
            }
            "thread/reconcile" => {
                let mut thread = self.store.load(text(&p, "threadId")?)?;
                ensure!(
                    thread.status == "unknown",
                    "only unknown threads can be reconciled"
                );
                ensure!(
                    !self.connections.contains_key(&thread.id),
                    "unresolved native connection"
                );
                ensure!(
                    p["expectedRevision"] == thread.revision,
                    "revision conflict"
                );
                ensure!(
                    p["acknowledgeUnknownOutcome"] == true,
                    "acknowledgeUnknownOutcome required"
                );
                thread.status = "idle".into();
                thread.revision += 1;
                if let Some(turn) = &mut thread.turn {
                    turn["status"] = json!("unknown");
                }
                self.store.save(&thread)?;
                thread.checkpoint_turn();
                self.store.save(&thread)?;
                Ok(json!({"thread":compat::thread(&thread)}))
            }
            _ => bail!("unsupported method: {method}"),
        }
    }
    async fn event(&mut self, message: RuntimeEvent) -> Result<()> {
        let id = message.id;
        if !self
            .connections
            .get(&id)
            .is_some_and(|c| c.generation == message.generation)
        {
            return Ok(());
        }
        let mut thread = self.store.load(&id)?;
        let event = match message.event {
            Ok(event) => event,
            Err(error) => {
                steer::mark_unknown(&mut thread);
                self.cancel_tools(
                    &mut thread,
                    "Native connection lost; execution outcome unknown, not replayed",
                )?;
                self.connections.remove(&id);
                self.mcp.remove(&id);
                self.grants.retain(|(thread, _)| thread != &id);
                self.approvals.retain(|_, (t, _)| t != &id);
                if thread.status == "running" {
                    goal::clock(&mut thread, now());
                    thread.status = "unknown".into();
                    if let Some(turn) = &mut thread.turn {
                        turn["status"] = json!("unknown");
                    }
                    if let Some(goal) = &mut thread.goal
                        && goal["status"] == "active"
                    {
                        goal["status"] = json!("blocked");
                        goal["reason"] = json!("Native execution outcome unknown; not replayed");
                    }
                    thread.revision += 1;
                    thread.checkpoint_turn();
                    self.store.save(&thread)?;
                    if thread.goal.is_some() {
                        goal::notify(&self.output, &thread);
                    }
                }
                self.agent_finished(&mut thread)?;
                self.interrupt_agent_children(&thread).await?;
                notify(
                    &self.output,
                    "thread/disconnected",
                    json!({"threadId":id,"status":thread.status,"error":error.to_string()}),
                );
                return Ok(());
            }
        };
        if let Some(result) = event.get("guardianReviewResult") {
            return self.review_result(&mut thread, result).await;
        }
        if let Some(result) = event.get("wrapperToolResult") {
            return self
                .tool_result(&mut thread, &result["call"], &result["result"])
                .await;
        }
        self.store.event(&id, &event)?;
        notify(
            &self.output,
            "native/event",
            json!({"threadId":id,"event":event}),
        );
        if let Some(hook) = event.get("callHookRequest") {
            return self.native_hook(&mut thread, hook).await;
        }
        if let Some(usage) = event.get("usageUpdate").filter(|u| u["total"].is_object()) {
            if usage != &thread.usage {
                thread.token_usage = compat::usage(usage, &thread.usage);
                notify(
                    &self.output,
                    "thread/tokenUsage/updated",
                    json!({"threadId":id,"turnId":thread.turn.as_ref().map(|t|t["id"].clone()),"tokenUsage":thread.token_usage}),
                );
            }
            goal::charge(&mut thread, usage);
            thread.usage = usage.clone();
            self.store.save(&thread)?;
            if thread.goal.is_some() {
                goal::notify(&self.output, &thread);
            }
            notify(
                &self.output,
                "native/usage",
                json!({"threadId":id,"usage":usage}),
            );
        }
        if let Some(request) = event.get("policyDecisionRequest") {
            if request["toolArgs"]["toolName"] == "run_command" {
                return self.native_command_policy(&mut thread, request).await;
            }
            if thread
                .turn
                .as_ref()
                .is_some_and(|t| t["interruptRequested"] == true)
            {
                self.send(&id,json!({"policyDecisionResponse":{"requestId":request["requestId"],"outcome":"POLICY_EVALUATION_OUTCOME_DENY","denyReason":"Turn interrupted"}})).await?;
                self.send(&id, json!({"haltRequest":true})).await?;
                return Ok(());
            }
            let args = &request["toolArgs"];
            let name = args["toolName"].as_str().unwrap_or("");
            if thread.permissions()?.mode == "dangerFullAccess"
                || execution::supported(name)
                || subagents::control(name)
                || [
                    "view_file",
                    "find_by_name",
                    "grep_search",
                    "create_file",
                    "edit_file",
                ]
                .contains(&name)
            {
                self.send(&id,json!({"policyDecisionResponse":{"requestId":request["requestId"],"outcome":"POLICY_EVALUATION_OUTCOME_ALLOW"}})).await?;
                return Ok(());
            }
            if thread.approval_policy == "never" {
                self.send(&id,json!({"policyDecisionResponse":{"requestId":request["requestId"],"outcome":"POLICY_EVALUATION_OUTCOME_DENY","denyReason":"Approvals are disabled"}})).await?;
                return Ok(());
            }
            let grant = serde_json::to_string(
                &json!({"tool":args["toolName"],"server":args["serverName"],"arguments":args["argumentsJson"]}),
            )?;
            if self.grants.contains(&(id.clone(), grant.clone())) {
                self.send(&id,json!({"policyDecisionResponse":{"requestId":request["requestId"],"outcome":"POLICY_EVALUATION_OUTCOME_ALLOW"}})).await?;
                return Ok(());
            }
            let rpc_id = format!("approval:{}", uuid::Uuid::new_v4());
            if thread.approvals_reviewer == "auto_review" {
                return self.start_review(&thread, json!({"nativeRequest":request}));
            }
            self.approvals.insert(
                rpc_id.clone(),
                (
                    id.clone(),
                    json!({"policyDecisionResponse":{"requestId":request["requestId"]},"grantKey":grant}),
                ),
            );
            let arguments: Value =
                serde_json::from_str(args["argumentsJson"].as_str().unwrap_or("{}"))
                    .unwrap_or(Value::Null);
            let tool = args["toolName"].as_str().unwrap_or("unknown");
            let file = [
                "create_file",
                "replace_file_content",
                "multi_replace_file_content",
                "write_to_file",
                "file_edit",
            ]
            .contains(&tool);
            let method = if file {
                "item/fileChange/requestApproval"
            } else {
                "item/commandExecution/requestApproval"
            };
            let item_id = format!(
                "{}:{}",
                args["trajectoryId"].as_str().unwrap_or(&id),
                args["stepIndex"].as_u64().unwrap_or(0)
            );
            let _ = self.output.send(json!({"jsonrpc":"2.0","id":rpc_id,"method":method,"params":{
                "threadId":id,"turnId":thread.turn.as_ref().map(|t|t["id"].clone()),"itemId":item_id,
                "startedAtMs":now()*1000,"cwd":thread.cwd,
                "command":arguments["CommandLine"].as_str().map(str::to_owned).unwrap_or_else(||format!("Native tool {tool}: {}",args["argumentsJson"].as_str().unwrap_or(""))),
                "reason":format!("Approve native tool {tool}. Session approval applies only to identical tool arguments."),
                "availableDecisions":["accept","acceptForSession","decline","cancel"],"approvalScope":"identicalToolArguments","native":request}}));
        }
        if let Some(call) = event.get("toolCall") {
            if subagents::control(call["name"].as_str().unwrap_or("")) {
                return self.agent_tool(&thread.id, call).await;
            }
            return self.tool_call(&mut thread, call).await;
        }
        if let Some(step) = event.get("stepUpdate") {
            if step["state"] == "STATE_WAITING_FOR_USER" && step.get("questionsRequest").is_some() {
                let (key, pending, request) = input::question(step, &thread)?;
                if let std::collections::btree_map::Entry::Vacant(e) = self.approvals.entry(key) {
                    e.insert((id.clone(), pending));
                    let _ = self.output.send(request);
                }
            }
            if step.get("toolConfirmationRequest").is_some() {
                // Native dynamic policy has already gated execution; this is its UI confirmation phase.
                let accepted = thread
                    .turn
                    .as_ref()
                    .is_none_or(|t| t["interruptRequested"] != true);
                self.send(&id,json!({"toolConfirmation":{"trajectoryId":step["trajectoryId"],"stepIndex":step.get("stepIndex").cloned().unwrap_or(json!(0)),"accepted":accepted}})).await?;
            }
            let item_id = format!(
                "{}:{}",
                step["trajectoryId"].as_str().unwrap_or(""),
                step["stepIndex"].as_u64().unwrap_or(0)
            );
            let item_type = match step["source"].as_str() {
                Some("SOURCE_USER") => "userMessage",
                Some("SOURCE_MODEL") if step["target"] == "TARGET_USER" => "agentMessage",
                _ => "nativeStep",
            };
            let turn_id = thread
                .turn
                .as_ref()
                .map(|turn| turn["id"].clone())
                .unwrap_or(Value::Null);
            let mut item = json!({"id":item_id,"type":item_type,"text":step["text"].as_str().unwrap_or(""),"state":step["state"],"native":step});
            if thread.options.output_schema.is_some()
                && let Some(output) = step["finish"]["outputString"].as_str()
            {
                item["text"] = json!(output);
            }
            if item_type == "nativeStep"
                && let Some(tool) = compat::tool_item(step, &item_id)
            {
                item = tool;
            }
            self.native_command_step(&mut thread, step, &mut item)?;
            let item_type = item["type"].as_str().unwrap_or("nativeStep").to_owned();
            if item_type == "userMessage" {
                item["content"] =
                    json!([{"type":"text","text":step["text"].as_str().unwrap_or("")}]);
                steer::user_item(&mut thread, step, &mut item);
                subagents::display_mail(&thread, &mut item);
            }
            for item in compat::message_items(item) {
                let item_id = item["id"].as_str().unwrap().to_owned();
                let item_type = item["type"].as_str().unwrap_or("nativeStep").to_owned();
                // Child trajectories have separate ownership; never fold their messages into the root.
                if step["trajectoryId"] == thread.id && item_type != "nativeStep" {
                    let mut first = true;
                    let mut changed = true;
                    let mut delta = None;
                    if let Some(turn) = &mut thread.turn {
                        if !turn["items"].is_array() {
                            turn["items"] = json!([]);
                        }
                        let items = turn["items"].as_array_mut().unwrap();
                        if let Some(previous) = items.iter_mut().find(|i| i["id"] == item_id) {
                            first = false;
                            changed = previous != &item;
                            let current_text = if item_type == "reasoning" {
                                &item["summary"][0]
                            } else {
                                &item["text"]
                            };
                            let previous_text = if item_type == "reasoning" {
                                &previous["summary"][0]
                            } else {
                                &previous["text"]
                            };
                            delta = current_text
                                .as_str()
                                .and_then(|current| {
                                    current.strip_prefix(previous_text.as_str().unwrap_or(""))
                                })
                                .map(str::to_owned);
                            *previous = item.clone();
                        } else {
                            items.push(item.clone());
                        }
                    }
                    thread.checkpoint_turn();
                    self.store.save(&thread)?;
                    if first {
                        let mut started = item.clone();
                        if item_type == "reasoning" {
                            started["summary"] = json!([]);
                        }
                        notify(
                            &self.output,
                            "item/started",
                            json!({"threadId":id,"turnId":turn_id,"item":started}),
                        );
                        if item_type == "reasoning" {
                            notify(
                                &self.output,
                                "item/reasoning/summaryPartAdded",
                                json!({"threadId":id,"turnId":turn_id,"itemId":item_id,"summaryIndex":0}),
                            );
                            notify(
                                &self.output,
                                "item/reasoning/summaryTextDelta",
                                json!({"threadId":id,"turnId":turn_id,"itemId":item_id,"summaryIndex":0,"delta":item["summary"][0]}),
                            );
                        }
                    }
                    // Full native text snapshots are authoritative; repeated deltas are not replay-safe.
                    if changed && (step["state"] == "STATE_DONE" || step["state"] == "STATE_ERROR")
                    {
                        notify(
                            &self.output,
                            "item/completed",
                            json!({"threadId":id,"turnId":turn_id,"item":item}),
                        );
                    } else if changed
                        && item_type == "agentMessage"
                        && !first
                        && delta.as_ref().is_some_and(|s| !s.is_empty())
                    {
                        notify(
                            &self.output,
                            "item/agentMessage/delta",
                            json!({"threadId":id,"turnId":turn_id,"itemId":item_id,"delta":delta}),
                        );
                    } else if changed
                        && item_type == "reasoning"
                        && !first
                        && delta.as_ref().is_some_and(|s| !s.is_empty())
                    {
                        notify(
                            &self.output,
                            "item/reasoning/summaryTextDelta",
                            json!({"threadId":id,"turnId":turn_id,"itemId":item_id,"summaryIndex":0,"delta":delta}),
                        );
                    } else if changed && !first {
                        notify(
                            &self.output,
                            "item/started",
                            json!({"threadId":id,"turnId":turn_id,"item":item}),
                        );
                    }
                }
            }
        }
        if let Some(state) = event.get("trajectoryStateUpdate") {
            let root = state["trajectoryId"] == thread.id;
            if root
                && ["STATE_FULLY_IDLE", "STATE_CANCELLED"]
                    .contains(&state["state"].as_str().unwrap_or(""))
                && thread.status == "running"
            {
                if state["state"] == "STATE_FULLY_IDLE"
                    && state.get("stopReason").is_none_or(Value::is_null)
                    && state.get("error").is_none_or(Value::is_null)
                    && thread
                        .turn
                        .as_ref()
                        .is_some_and(|t| t["interruptRequested"] != true)
                    && thread
                        .steer_receipts
                        .values()
                        .any(|r| r["state"] == "queued")
                {
                    // An idle frame can race a steer dispatch; wait for its input acknowledgment.
                    return Ok(());
                }
                goal::clock(&mut thread, now());
                steer::mark_unknown(&mut thread);
                self.cancel_tools(
                    &mut thread,
                    "Native turn ended without a tool result; outcome unknown, not replayed",
                )?;
                thread.status = "idle".into();
                if state["stopReason"]
                    .as_str()
                    .is_some_and(|s| s.contains("EXCEEDED"))
                    && let Some(goal) = &mut thread.goal
                    && goal["status"] == "active"
                {
                    goal["status"] = json!("budgetLimited");
                }
                thread.revision += 1;
                if let Some(turn) = &mut thread.turn {
                    turn["status"] = json!(if state["state"] == "STATE_CANCELLED"
                        || turn["interruptRequested"] == true
                    {
                        "interrupted"
                    } else if !state["error"].is_null() {
                        "failed"
                    } else {
                        "completed"
                    });
                    turn["nativeStop"] = state.clone();
                    *turn = compat::turn(turn);
                    if turn["status"] == "completed"
                        && let Some(schema) = &thread.options.output_schema
                    {
                        let output = turn["items"]
                            .as_array()
                            .and_then(|items| {
                                items.iter().rev().find(|i| i["type"] == "agentMessage")
                            })
                            .and_then(|i| i["text"].as_str())
                            .unwrap_or("");
                        if let Err(error) = session_options::validate_output(schema, output) {
                            turn["status"] = json!("failed");
                            turn["error"] = json!({"message":error.to_string(),"codexErrorInfo":null,"additionalDetails":null});
                        }
                    }
                }
                thread.checkpoint_turn();
                self.store.save(&thread)?;
                if thread.goal.is_some() {
                    goal::notify(&self.output, &thread);
                }
                self.approvals.retain(|_, (t, _)| t != &id);
                self.agent_finished(&mut thread)?;
                notify(
                    &self.output,
                    "turn/completed",
                    json!({"threadId":id,"turn":thread.turn.as_ref().map(compat::turn)}),
                );
            }
        }
        Ok(())
    }
    async fn approval(&mut self, message: Value) -> Result<()> {
        let key = text(&message, "id")?;
        let (id, mut event) = self.approvals.remove(key).context("unknown approval ID")?;
        if event.get("nativeCommand").is_some() {
            let decision = message["result"]["decision"].as_str().unwrap_or("");
            let allow = message.get("error").is_none()
                && ["accept", "acceptForSession"].contains(&decision);
            let mut thread = self.store.load(&id)?;
            return self
                .native_command_approval(
                    &mut thread,
                    &event,
                    allow,
                    decision == "acceptForSession",
                    if allow {
                        ""
                    } else {
                        "User denied sandbox bypass"
                    },
                )
                .await;
        }
        if let Some(call) = event.get("externalTool") {
            return self.tool_approval(&id, call, &message).await;
        }
        if let Some(step) = event.get("questionStep") {
            return self.send(&id, input::answer(step, &message)).await;
        }
        let allow = ["accept", "acceptForSession"]
            .contains(&message["result"]["decision"].as_str().unwrap_or(""))
            && message.get("error").is_none();
        let grant = event.as_object_mut().unwrap().remove("grantKey");
        if allow
            && message["result"]["decision"] == "acceptForSession"
            && let Some(grant) = grant.and_then(|v| v.as_str().map(str::to_owned))
        {
            self.grants.insert((id.clone(), grant));
        }
        event["policyDecisionResponse"]["outcome"] = json!(if allow {
            "POLICY_EVALUATION_OUTCOME_ALLOW"
        } else {
            "POLICY_EVALUATION_OUTCOME_DENY"
        });
        self.send(&id, event).await
    }
}

fn configure(thread: &mut Thread, params: &Value) -> Result<()> {
    session_options::configure(thread, params)?;
    if let Some(reviewer) = params.get("approvalsReviewer").filter(|v| !v.is_null()) {
        ensure!(
            reviewer == "user" || reviewer == "auto_review",
            "unsupported approvals reviewer"
        );
        thread.approvals_reviewer = reviewer.as_str().unwrap().into();
    }
    if let Some(config) = params["config"].get("guardian") {
        let config: guardian::ReviewConfig = serde_json::from_value(config.clone())?;
        config.validate()?;
        thread.review_config = config;
    }
    if let Some(mode) = params["collaborationMode"]["mode"].as_str() {
        ensure!(
            ["default", "plan"].contains(&mode),
            "unsupported collaboration mode"
        );
        if thread.collaboration_mode != mode {
            thread.tool_grants.clear();
        }
        thread.collaboration_mode = mode.into();
    }
    if let Some(cwd) = params["cwd"].as_str() {
        ensure!(
            PathBuf::from(cwd).canonicalize()? == thread.cwd,
            "cannot change thread workspace"
        );
    }
    if params["sandbox"] == "danger-full-access"
        || params["sandboxPolicy"]["type"] == "dangerFullAccess"
    {
        thread.unrestricted = true;
    }
    if params["sandbox"] == "workspace-write" {
        thread.unrestricted = false;
    }
    thread.model = models::select(&thread.model, params)?;
    if let Some(config) = params["config"].get("mcp_servers") {
        ensure!(config.is_object(), "mcp_servers must be an object");
        thread.mcp_servers = config.clone();
    }
    if let Some(policy) = params.get("approvalPolicy") {
        let policy = policy.as_str().context("approvalPolicy must be a string")?;
        ensure!(
            ["on-request", "never"].contains(&policy),
            "unsupported approvalPolicy"
        );
        thread.approval_policy = policy.into();
    }
    if let Some(mode) = params.get("unrestricted") {
        thread.unrestricted = mode.as_bool().context("unrestricted must be boolean")?;
    }
    if let Some(directories) = params.get("additionalDirectories") {
        let directories = directories
            .as_array()
            .context("additionalDirectories must be an array")?;
        ensure!(directories.len() <= 64, "too many additional directories");
        thread.additional_directories = directories
            .iter()
            .map(|v| {
                let path = PathBuf::from(v.as_str().context("directory must be a string")?);
                ensure!(
                    path.is_absolute(),
                    "additional directories must be absolute"
                );
                let path = path.canonicalize()?;
                ensure!(path.is_dir(), "additional path must be a directory");
                Ok(path)
            })
            .collect::<Result<Vec<_>>>()?;
        thread.additional_directories.sort();
        thread.additional_directories.dedup();
    }
    if let Some(budget) = params.get("budget") {
        if budget.is_null() {
            thread.budget = None;
        } else {
            let fields = budget
                .as_object()
                .context("budget must be an object or null")?;
            for (name, value) in fields {
                match name.as_str() {
                    "scope" => ensure!(
                        ["BUDGET_SCOPE_LIFETIME", "BUDGET_SCOPE_FORWARD_LOOKING"]
                            .contains(&value.as_str().unwrap_or("")),
                        "invalid budget scope"
                    ),
                    "maxModelCalls" | "maxToolCalls" => ensure!(
                        value
                            .as_u64()
                            .is_some_and(|n| n > 0 && n <= i32::MAX as u64),
                        "budget call limits must be positive int32 values"
                    ),
                    "maxInputTokens" | "maxOutputTokens" | "maxTotalTokens" => ensure!(
                        value
                            .as_u64()
                            .is_some_and(|n| n > 0 && n <= i64::MAX as u64),
                        "budget token limits must be positive int64 values"
                    ),
                    _ => bail!("unsupported budget field: {name}"),
                }
            }
            thread.budget = Some(budget.clone());
            native_budget::validate(thread.budget.as_ref())?;
        }
    }
    if thread.permission_policy.is_none()
        || [
            "sandboxPolicy",
            "sandbox",
            "unrestricted",
            "additionalDirectories",
        ]
        .iter()
        .any(|k| params.get(k).is_some_and(|v| !v.is_null()))
    {
        ensure!(
            params.get("unrestricted").is_none()
                || params.get("sandbox").is_none() && params.get("sandboxPolicy").is_none(),
            "unrestricted conflicts with sandbox policy"
        );
        let mut policy_params = params.clone();
        if params.get("unrestricted").is_some() {
            policy_params["sandbox"] = json!(if thread.unrestricted {
                "danger-full-access"
            } else {
                "workspace-write"
            });
        } else if params.get("sandbox").is_none_or(Value::is_null)
            && params.get("sandboxPolicy").is_none_or(Value::is_null)
        {
            policy_params["sandboxPolicy"] = thread.requested_permissions()?.effective();
        }
        let full = policy_params["sandbox"] == "danger-full-access"
            || policy_params["sandboxPolicy"]["type"] == "dangerFullAccess";
        if full {
            policy_params
                .as_object_mut()
                .unwrap()
                .remove("additionalDirectories");
        } else if policy_params["sandbox"] != "read-only"
            && policy_params["sandboxPolicy"]["type"] != "readOnly"
        {
            policy_params["additionalDirectories"] = json!(thread.additional_directories);
        }
        let next = permissions::PermissionPolicy::from_rpc(&policy_params, &thread.cwd)?;
        if thread.requested_permissions()?.effective() != next.effective() {
            thread.tool_grants.clear();
        }
        thread.unrestricted = next.mode == "dangerFullAccess";
        thread.permission_policy = Some(next);
    }
    Ok(())
}

fn state_home() -> PathBuf {
    let path = PathBuf::from(
        std::env::var_os("AGY_APP_SERVER_HOME")
            .or_else(|| std::env::var_os("AGY_HARNESS_HOME"))
            .unwrap_or_else(|| ".harness".into()),
    );
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir().unwrap().join(path)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if native_bridge::helper_main().await? {
        return Ok(());
    }
    if let Some(code) = permissions::helper_main() {
        std::process::exit(code);
    }
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.as_slice() {
        [] => {}
        [flag] if flag == "--help" || flag == "-h" => {
            println!(
                "antigravity-app-server {}\n\nUsage: antigravity-app-server [--login | --doctor | --version | --help]\n\nWith no arguments, serve newline-delimited JSON-RPC over stdin/stdout.\n--doctor checks local Codex connectivity and prerequisites without model calls.\n\nRequired: AGY_APP_CORE_BINARY. Run --login once for native account authentication.\nOptional: AGY_APP_SERVER_HOME, CODEX_HOME, AGY_CODEX_BINARY,\n          AGY_CODEX_CONNECTION=auto|daemon|stdio, AGY_CODEX_SOCKET.\nSee README.md for setup and examples/client.mjs for a complete client.",
                env!("CARGO_PKG_VERSION")
            );
            return Ok(());
        }
        [flag] if flag == "--version" || flag == "-V" => {
            println!("antigravity-app-server {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        [flag] if flag == "--login" => {
            let binary = native::binary()?;
            native::verify_binary(&binary)?;
            let store = Store::open(&state_home())?;
            let status = native_auth::login(
                &binary,
                &store.root.join("app-core-control"),
                &native::auth_home(&store.root),
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&status)?);
            return Ok(());
        }
        [flag] if flag == "--doctor" => {
            let review = guardian::status(&guardian::ReviewConfig::default()).await;
            let native = native::binary().and_then(|p| native::verify_binary(&p));
            let root = state_home();
            let credentials_configured = native::auth_home(&root)
                .join(".gemini/jetski-standalone-oauth-token")
                .is_file();
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &json!({"application":"antigravity-app-server","version":env!("CARGO_PKG_VERSION"),"platform":format!("{}-{}",std::env::consts::OS,std::env::consts::ARCH),"nativeRuntime":{"available":native.is_ok(),"error":native.err().map(|e|e.to_string())},"googleCredentialsConfigured":credentials_configured,"googleAuthenticationTested":false,"guardian":review,"modelCalls":0})
                )?
            );
            return Ok(());
        }
        _ => {} // Existing app-server/--listen/-c arguments are validated below.
    }
    let config = mcp::launch_config()?;
    let root = PathBuf::from(
        std::env::var("AGY_APP_SERVER_HOME")
            .or_else(|_| std::env::var("AGY_HARNESS_HOME"))
            .unwrap_or(".harness".into()),
    );
    let binary = native::binary()?;
    native::verify_binary(&binary)?;
    let store = Arc::new(Store::open(&root)?);
    let auth_home = native::auth_home(&store.root);
    let core =
        native::Process::open(&binary, &store.root.join("app-core-control"), &auth_home).await?;
    let endpoint = core.endpoint.clone();
    for mut thread in store.list()? {
        if thread.executable().is_err() {
            continue;
        }
        if thread.status == "running" {
            steer::mark_unknown(&mut thread);
            native_commands::mark_unknown(
                &mut thread,
                "Process restarted; execution outcome unknown, not replayed",
            )?;
            thread.status = "unknown".into();
            if let Some(turn) = &mut thread.turn {
                turn["status"] = json!("unknown");
            }
            if let Some(goal) = &mut thread.goal
                && goal["status"] == "active"
            {
                goal["status"] = json!("blocked");
                goal["reason"] = json!("Process restarted with unknown execution; not replayed");
            }
            thread.revision += 1;
            thread.checkpoint_turn();
            store.save(&thread)?;
        }
    }
    let (output, mut output_rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(value) = output_rx.recv().await {
            stdout.write_all(format!("{value}\n").as_bytes()).await?;
            stdout.flush().await?;
        }
        Ok::<_, std::io::Error>(())
    });
    let (events, mut events_rx) = mpsc::unbounded_channel();
    let (control, mut control_rx) = mpsc::unbounded_channel::<goal::Action>();
    let mut server = Server {
        store,
        binary,
        endpoint,
        model_catalog: None,
        output,
        events,
        connections: BTreeMap::new(),
        approvals: BTreeMap::new(),
        grants: Default::default(),
        tool_jobs: BTreeMap::new(),
        reviews: BTreeMap::new(),
        mcp: BTreeMap::new(),
        config,
        control,
        agent_waits: BTreeMap::new(),
        initialized: false,
    };
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut goal_tick = tokio::time::interval(std::time::Duration::from_millis(250));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
            _ = server.output.closed() => break,
            Some(action)=control_rx.recv()=>{let result=server.goal_action(&action.thread_id,&action.params);let _=action.reply.send(result);},
            _ = goal_tick.tick() => {
                if let Err(error) = server.advance_agents().await { eprintln!("agent scheduler failed: {error}"); break; }
                if let Err(error) = server.advance_goals().await { eprintln!("goal scheduler failed: {error}"); break; }
            }
            line = lines.next_line() => {
                let Some(line) = line? else { break; };
                let message: Value = match serde_json::from_str(&line) {
                    Ok(value) => value,
                    Err(_) => { let _ = server.output.send(json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"invalid JSON"}})); continue; }
                };
                if !message.is_object() || message.get("jsonrpc").is_some_and(|v|v != "2.0") || (message.get("method").is_none() && message.get("result").is_none() && message.get("error").is_none()) {
                    let _ = server.output.send(json!({"jsonrpc":"2.0","id":message.get("id").cloned().unwrap_or(Value::Null),"error":{"code":-32600,"message":"invalid JSON-RPC request"}}));
                    continue;
                }
                if let Some(method) = message["method"].as_str() {
                    if message.get("id").is_none() { continue; }
                    if !METHODS.contains(&method) {
                        let _ = server.output.send(json!({"jsonrpc":"2.0","id":message["id"],"error":{"code":-32601,"message":"method not found"}}));
                        continue;
                    }
                    let result = server.request(method,message["params"].clone()).await;
                    let response = match result {
                        Ok(result) => json!({"jsonrpc":"2.0","id":message["id"],"result":result}),
                        Err(error) => json!({"jsonrpc":"2.0","id":message["id"],"error":{"code":-32000,"message":error.to_string()}}),
                    };
                    let _ = server.output.send(response);
                } else if let Err(error) = server.approval(message).await {
                    eprintln!("approval response rejected: {error}");
                }
            }
            Some(event) = events_rx.recv() => {
                if let Err(error) = server.event(event).await { eprintln!("native event failed: {error}"); break; }
            }
        }
    }
    for id in server.connections.keys().cloned().collect::<Vec<_>>() {
        server.disconnect(&id).await;
    }
    drop(server);
    core.close().await;
    writer.await??;
    // Tokio's blocking stdin read cannot be cancelled when a signalling client keeps its pipe open.
    // All sessions and the output writer have already been closed above.
    std::process::exit(0)
}
