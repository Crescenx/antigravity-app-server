//! Codex-style collaboration controls composed from native Thread/Turn operations.
use crate::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct State {
    pub parent_id: Option<String>,
    pub root_id: Option<String>,
    pub path: Option<String>,
    pub controls: BTreeMap<String, Value>,
    pub mailbox: Vec<Value>,
    pub reported_turn: Option<String>,
}

pub fn root(thread: &Thread) -> &str {
    thread.subagents.root_id.as_deref().unwrap_or(&thread.id)
}
pub fn path(thread: &Thread) -> &str {
    thread.subagents.path.as_deref().unwrap_or("/root")
}
pub fn enabled(thread: &Thread) -> bool {
    !thread.is_plan() && !thread.options.tools_disabled
}
pub fn control(name: &str) -> bool {
    matches!(
        name,
        "spawn_agent"
            | "send_message"
            | "followup_task"
            | "list_agents"
            | "wait_agent"
            | "interrupt_agent"
    )
}
pub fn source(thread: &Thread) -> Value {
    match &thread.subagents.parent_id {
        Some(parent) => {
            json!({"subAgent":{"thread_spawn":{"parent_thread_id":parent,"depth":path(thread).matches('/').count()-1,"agent_path":path(thread),"agent_nickname":path(thread).rsplit('/').next(),"agent_role":"default"}}})
        }
        None => json!("appServer"),
    }
}
pub fn status(thread: &Thread) -> Value {
    match thread.status.as_str() {
        "running" => json!("running"),
        "unknown" => {
            json!({"errored":"Execution outcome unknown; explicit reconciliation required"})
        }
        _ => match thread.turn.as_ref() {
            Some(turn) if turn["status"] == "interrupted" => json!("interrupted"),
            Some(turn) if turn["status"] == "failed" => {
                json!({"errored":turn["error"].to_string()})
            }
            Some(turn) => {
                json!({"completed":turn["items"].as_array().and_then(|a|a.iter().rev().find(|i|i["type"]=="agentMessage")).and_then(|i|i["text"].as_str())})
            }
            None => json!("pendingInit"),
        },
    }
}
pub fn guidance(thread: &Thread) -> Value {
    json!({"title":"Collaboration","content":format!("Your agent name is {}. Use spawn_agent, send_message, followup_task, list_agents, wait_agent and interrupt_agent when delegation is authorized. Agents share the workspace and inherit effective permissions, rules and tools. send_message only queues messages for idle agents; followup_task starts their next task. interrupt_agent stops a task while preserving the agent. Messages and completed results arrive during active turns; use wait_agent when awaiting them. Do not infer completion from a successful spawn. At most four agents in a tree can run concurrently, including the root; children cannot spawn further agents. fork_turns=all supplies the parent's visible conversation as attributed context; fork_turns=none starts with the task alone. Only none allows model overrides.",path(thread))})
}
pub fn native(thread: &Thread, config: &mut Value) {
    if !enabled(thread) {
        return;
    }
    let mut specs = vec![
        (
            "send_message",
            "Queue a message to an agent. Does not start an idle agent.",
            json!({"target":{"type":"string"},"message":{"type":"string"}}),
            vec!["target", "message"],
        ),
        (
            "followup_task",
            "Send a task; starts an idle non-root agent, or steers a running agent.",
            json!({"target":{"type":"string"},"message":{"type":"string"}}),
            vec!["target", "message"],
        ),
        (
            "interrupt_agent",
            "Interrupt a non-root agent's current task, preserving its identity and history.",
            json!({"target":{"type":"string"}}),
            vec!["target"],
        ),
        (
            "list_agents",
            "List agents in this tree, optionally filtering by canonical path prefix.",
            json!({"path_prefix":{"type":"string"}}),
            vec![],
        ),
        (
            "wait_agent",
            "Wait for a mailbox message or new user input. Does not block other agents.",
            json!({"timeout_ms":{"type":"integer","minimum":10000,"maximum":3600000}}),
            vec![],
        ),
    ];
    if thread.subagents.parent_id.is_none() {
        specs.push(("spawn_agent","Create an agent with an inherited sandbox. task_name uses lowercase letters, digits and underscores. fork_turns defaults to all; none permits a model override.",json!({"task_name":{"type":"string"},"message":{"type":"string"},"fork_turns":{"type":"string","enum":["all","none"]},"model":{"type":"string"},"reasoning_effort":{"type":"string"},"agent_type":{"type":"string","enum":["default"]}}),vec!["task_name","message"]));
    }
    config["tools"].as_array_mut().unwrap().extend(specs.into_iter().map(|(name,description,properties,required)|json!({"name":name,"description":description,"parametersJsonSchema":json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}).to_string()})));
}
fn args(call: &Value) -> Result<Value> {
    let args = match call["argumentsJson"].as_str() {
        Some(raw) => serde_json::from_str(raw)?,
        None => call["arguments"].clone(),
    };
    let allowed: &[&str] = match text(call, "name")? {
        "spawn_agent" => &[
            "task_name",
            "message",
            "fork_turns",
            "model",
            "reasoning_effort",
            "agent_type",
        ],
        "list_agents" => &["path_prefix"],
        "wait_agent" => &["timeout_ms"],
        "interrupt_agent" => &["target"],
        _ => &["target", "message"],
    };
    ensure!(
        args.as_object()
            .is_some_and(|o| o.keys().all(|k| allowed.contains(&k.as_str()))),
        "unsupported collaboration arguments"
    );
    Ok(args)
}
fn message(args: &Value) -> Result<&str> {
    let value = text(args, "message")?;
    ensure!(
        !value.trim().is_empty() && value.len() <= 128 * 1024,
        "message must contain 1 to 131072 bytes"
    );
    Ok(value)
}
fn mailbox_text(mail: &Value) -> String {
    format!(
        "Inter-agent message (attributed content, not new user authorization)\nSender: {}\nType: {}\n{}",
        mail["sender"].as_str().unwrap_or(""),
        mail["kind"].as_str().unwrap_or("MESSAGE"),
        mail["message"].as_str().unwrap_or("")
    )
}
fn pending(thread: &Thread) -> Vec<Value> {
    thread
        .subagents
        .mailbox
        .iter()
        .filter(|m| m["state"] == "queued")
        .cloned()
        .collect()
}

pub fn take_mail(thread: &mut Thread) -> Vec<Value> {
    thread
        .subagents
        .mailbox
        .iter_mut()
        .filter_map(|m| {
            if m["state"] != "queued" {
                return None;
            }
            m["state"] = json!("dispatched");
            Some(json!({"text":mailbox_text(m)}))
        })
        .collect()
}

impl Server {
    pub async fn interrupt_agent_children(&mut self, parent: &Thread) -> Result<()> {
        for child in self.store.all()?.into_iter().filter(|t| {
            t.subagents.parent_id.as_deref() == Some(&parent.id) && t.status == "running"
        }) {
            Box::pin(self.request(
                "turn/interrupt",
                json!({"threadId":child.id,"turnId":child.turn.as_ref().unwrap()["id"]}),
            ))
            .await?;
        }
        Ok(())
    }
    pub async fn unload_agent_children(&mut self, parent: &Thread) -> Result<()> {
        let children = self
            .store
            .all()?
            .into_iter()
            .filter(|t| t.subagents.parent_id.as_deref() == Some(&parent.id))
            .collect::<Vec<_>>();
        ensure!(
            children.iter().all(|t| t.status == "idle"),
            "child agents must be idle before unloading their parent"
        );
        for child in children {
            self.disconnect(&child.id).await;
            self.store.forget_ephemeral(&child.id);
        }
        Ok(())
    }

    fn agent_target(&self, caller: &Thread, target: &str) -> Result<Thread> {
        let canonical = if target.starts_with('/') {
            target.to_owned()
        } else {
            format!("{}/{}", path(caller), target)
        };
        self.store
            .all()?
            .into_iter()
            .find(|t| root(t) == root(caller) && (t.id == target || path(t) == canonical))
            .context("agent not found in this conversation tree")
    }
    pub fn agent_capacity(&self, caller: &Thread) -> Result<()> {
        let count = self
            .store
            .all()?
            .iter()
            .filter(|t| root(t) == root(caller) && t.status == "running")
            .count();
        ensure!(count < 4, "all four concurrent agent slots are occupied");
        Ok(())
    }
    fn agent_notice(
        &self,
        thread: &mut Thread,
        call: &Value,
        result: Option<&Value>,
    ) -> Result<()> {
        let args = args(call)?;
        let name = text(call, "name")?;
        let tool = match name {
            "spawn_agent" => "spawnAgent",
            "send_message" => "sendMessage",
            "followup_task" => "followupTask",
            "list_agents" => "listAgents",
            "interrupt_agent" => "interruptAgent",
            _ => "wait",
        };
        let target = result
            .and_then(|r| r["agent_id"].as_str())
            .or(args["target"].as_str())
            .and_then(|t| self.agent_target(thread, t).ok());
        let item = json!({"id":format!("agent:{}",text(call,"id")?),"type":"collabAgentToolCall","tool":tool,"status":match result {None=>"inProgress",Some(r) if r.get("error").is_some()=>"failed",Some(_)=>"completed"},"senderThreadId":thread.id,"receiverThreadIds":target.as_ref().map(|t|vec![t.id.clone()]).unwrap_or_default(),"receiverAgents":[],"prompt":args["message"],"model":args["model"],"reasoningEffort":args["reasoning_effort"],"agentsStates":{},"result":result});
        if let Some(turn) = &mut thread.turn {
            let items = turn["items"].as_array_mut().context("turn items")?;
            if let Some(old) = items.iter_mut().find(|i| i["id"] == item["id"]) {
                *old = item.clone();
            } else {
                items.push(item.clone());
            }
            notify(
                &self.output,
                if result.is_some() {
                    "item/completed"
                } else {
                    "item/started"
                },
                json!({"threadId":thread.id,"turnId":turn["id"],"item":item}),
            );
        }
        thread.checkpoint_turn();
        Ok(())
    }
    async fn agent_result(&mut self, id: &str, call: &Value, result: Value) -> Result<()> {
        let mut thread = self.store.load(id)?;
        thread
            .subagents
            .controls
            .get_mut(text(call, "id")?)
            .context("missing agent receipt")?["result"] = result.clone();
        self.agent_notice(&mut thread, call, Some(&result))?;
        self.store.save(&thread)?;
        self.send(
            id,
            json!({"toolResponse":{"id":call["id"],"responseJson":result.to_string()}}),
        )
        .await
    }
    pub async fn agent_tool(&mut self, id: &str, call: &Value) -> Result<()> {
        let mut thread = self.store.load(id)?;
        let call_id = text(call, "id")?.to_owned();
        if let Some(receipt) = thread.subagents.controls.get(&call_id) {
            ensure!(
                receipt["call"] == *call,
                "collaboration call ID reused with changed arguments"
            );
            if let Some(result) = receipt.get("result") {
                return self
                    .send(
                        id,
                        json!({"toolResponse":{"id":call_id,"responseJson":result.to_string()}}),
                    )
                    .await;
            }
            if self.agent_waits.contains_key(&(id.into(), call_id.clone())) {
                return Ok(());
            }
            return self.send(id,json!({"toolResponse":{"id":call_id,"errorMessage":"Previous collaboration outcome unknown; not replayed"}})).await;
        }
        thread.subagents.controls.insert(
            call_id.clone(),
            json!({"call":call,"revision":thread.revision}),
        );
        // Validation failures are tool results, not a server-wide native event failure.
        let validation = (|| {
            ensure!(enabled(&thread), "collaboration unavailable in this mode");
            ensure!(
                thread.status == "running"
                    && thread
                        .turn
                        .as_ref()
                        .is_some_and(|t| t["interruptRequested"] != true),
                "no active turn"
            );
            ensure!(
                call["trajectoryId"].as_str().is_none_or(|s| s == id),
                "foreign trajectory"
            );
            args(call)
        })();
        if validation.is_ok() {
            self.agent_notice(&mut thread, call, None)?;
        }
        self.store.save(&thread)?;
        let outcome = match validation {
            Ok(args) => self.agent_action(&thread, text(call, "name")?, &args).await,
            Err(error) => Err(error),
        };
        match outcome {
            Ok(Some(result)) => self.agent_result(id, call, result).await,
            Ok(None) => {
                let ms = args(call)?["timeout_ms"]
                    .as_u64()
                    .unwrap_or(30000)
                    .max(10000);
                self.agent_waits.insert(
                    (id.into(), call_id),
                    std::time::Instant::now() + std::time::Duration::from_millis(ms),
                );
                Ok(())
            }
            Err(error) => {
                // Invalid argument objects cannot be rendered by agent_notice.
                if args(call).is_err() {
                    let mut t = self.store.load(id)?;
                    let result = json!({"error":error.to_string()});
                    t.subagents.controls.get_mut(&call_id).unwrap()["result"] = result.clone();
                    self.store.save(&t)?;
                    self.send(
                        id,
                        json!({"toolResponse":{"id":call_id,"responseJson":result.to_string()}}),
                    )
                    .await
                } else {
                    self.agent_result(id, call, json!({"error":error.to_string()}))
                        .await
                }
            }
        }
    }
    async fn agent_action(
        &mut self,
        caller: &Thread,
        name: &str,
        args: &Value,
    ) -> Result<Option<Value>> {
        match name {
            "spawn_agent" => {
                ensure!(
                    caller.subagents.parent_id.is_none(),
                    "maximum agent depth reached"
                );
                self.agent_capacity(caller)?;
                let task = text(args, "task_name")?;
                ensure!(
                    task.len() <= 64
                        && task
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                    "invalid task_name"
                );
                let agent_path = format!("{}/{}", path(caller), task);
                ensure!(
                    !self
                        .store
                        .all()?
                        .iter()
                        .any(|t| root(t) == root(caller) && path(t) == agent_path),
                    "task_name already exists; use followup_task"
                );
                let task_message = message(args)?;
                ensure!(
                    args["fork_turns"].is_null() || args["fork_turns"].is_string(),
                    "fork_turns must be text"
                );
                let fork = args["fork_turns"].as_str().unwrap_or("all");
                ensure!(
                    ["all", "none"].contains(&fork),
                    "fork_turns must be all or none"
                );
                ensure!(
                    args["agent_type"].is_null() || args["agent_type"] == "default",
                    "only default agent_type is available"
                );
                ensure!(
                    fork == "none"
                        || (args["model"].is_null() && args["reasoning_effort"].is_null()),
                    "model overrides require fork_turns=none"
                );
                let mut child = caller.clone();
                child.id = uuid::Uuid::new_v4().to_string();
                child.name = Some(task.into());
                child.status = "idle".into();
                child.revision = 0;
                child.subagents = State {
                    parent_id: Some(caller.id.clone()),
                    root_id: Some(root(caller).into()),
                    path: Some(agent_path.clone()),
                    ..Default::default()
                };
                child.turn = None;
                child.turns.clear();
                child.pending_context.clear();
                child.receipts.clear();
                child.steer_receipts.clear();
                child.tool_receipts.clear();
                child.native_command_receipts.clear();
                child.tool_grants.clear();
                child.goal = None;
                child.usage = Value::Null;
                child.token_usage = Value::Null;
                child.options.output_schema = None;
                child.created_at = now();
                child.updated_at = now();
                // Fork only the visible transcript; no database copies or unverified native state edits.
                if fork == "all" {
                    let history = serde_json::to_string(
                        &caller
                            .turns
                            .iter()
                            .map(|turn| {
                                let mut history = compat::turn(turn);
                                if let Some(object) = history.as_object_mut() {
                                    object.retain(|key, _| {
                                        ["id", "status", "items"].contains(&key.as_str())
                                    });
                                }
                                if let Some(items) = history["items"].as_array_mut() {
                                    for item in items {
                                        if let Some(object) = item.as_object_mut() {
                                            object.remove("native");
                                            object.remove("nativeTool");
                                        }
                                    }
                                }
                                history
                            })
                            .collect::<Vec<_>>(),
                    )?;
                    child.pending_context.push(json!({"type":"message","role":"developer","content":[{"type":"input_text","text":format!("Parent conversation, before this delegated task:\n{history}")}]}));
                    input::injected(&child.pending_context)?;
                }
                if fork == "none" {
                    self.configure_thread(
                        &mut child,
                        &json!({"model":args["model"],"reasoningEffort":args["reasoning_effort"]}),
                    )
                    .await?;
                }
                self.store.save(&child)?;
                self.connect(&child, false).await?;
                notify(
                    &self.output,
                    "thread/started",
                    json!({"thread":compat::thread(&child)}),
                );
                Box::pin(self.request(
                    "turn/start",
                    json!({"threadId":child.id,"input":[{"type":"text","text":task_message}]}),
                ))
                .await?;
                Ok(Some(json!({"agent_id":child.id,"agent_name":agent_path})))
            }
            "list_agents" => {
                let prefix = args["path_prefix"].as_str();
                ensure!(
                    args["path_prefix"].is_null() || prefix.is_some(),
                    "path_prefix must be text"
                );
                let agents=self.store.all()?.into_iter().filter(|t|root(t)==root(caller)&&prefix.is_none_or(|p|path(t)==p||path(t).strip_prefix(p).is_some_and(|s|s.starts_with('/')))).map(|t|json!({"agent_name":path(&t),"agent_id":t.id,"agent_status":status(&t)})).collect::<Vec<_>>();
                Ok(Some(json!({"agents":agents})))
            }
            "send_message" | "followup_task" => {
                let target = self.agent_target(caller, text(args, "target")?)?;
                ensure!(target.id != caller.id, "cannot message yourself");
                if name == "followup_task" {
                    ensure!(
                        target.subagents.parent_id.is_some(),
                        "followup_task cannot start the root agent"
                    );
                    if target.status == "idle" {
                        self.agent_capacity(caller)?;
                    }
                }
                ensure!(
                    target.status != "unknown",
                    "target requires explicit reconciliation"
                );
                self.queue_agent_message(
                    &target.id,
                    path(caller),
                    if name == "followup_task" {
                        "TASK"
                    } else {
                        "MESSAGE"
                    },
                    message(args)?,
                )?;
                if name == "followup_task" && target.status == "idle" {
                    if !self.connections.contains_key(&target.id) {
                        self.connect(&target, true).await?;
                    }
                    self.deliver_agent_mail(&target.id, true).await?;
                }
                Ok(Some(json!({})))
            }
            "interrupt_agent" => {
                let target = self.agent_target(caller, text(args, "target")?)?;
                ensure!(
                    target.subagents.parent_id.is_some() && target.id != caller.id,
                    "interrupt_agent requires another non-root agent"
                );
                let previous = status(&target);
                if target.status == "running" {
                    Box::pin(self.request(
                        "turn/interrupt",
                        json!({"threadId":target.id,"turnId":target.turn.as_ref().unwrap()["id"]}),
                    ))
                    .await?;
                }
                Ok(Some(json!({"previous_status":previous})))
            }
            "wait_agent" => {
                ensure!(
                    args["timeout_ms"].is_null()
                        || args["timeout_ms"].as_u64().is_some_and(|n| n <= 3600000),
                    "timeout_ms must be a nonnegative integer up to 3600000"
                );
                Ok(None)
            }
            _ => bail!("unknown collaboration tool"),
        }
    }
    fn queue_agent_message(&self, id: &str, sender: &str, kind: &str, message: &str) -> Result<()> {
        let mut thread = self.store.load(id)?;
        thread.subagents.mailbox.push(json!({"id":uuid::Uuid::new_v4().to_string(),"sender":sender,"kind":kind,"message":message,"state":"queued"}));
        self.store.save(&thread)
    }
    async fn deliver_agent_mail(&mut self, id: &str, wake: bool) -> Result<()> {
        let mut thread = self.store.load(id)?;
        let mail = pending(&thread);
        if mail.is_empty()
            || (thread.status != "running" && !wake)
            || thread
                .turn
                .as_ref()
                .is_some_and(|t| thread.status == "running" && t["interruptRequested"] == true)
        {
            return Ok(());
        }
        let input = mail
            .iter()
            .map(|m| json!({"type":"text","text":mailbox_text(m)}))
            .collect::<Vec<_>>();
        // Reserve delivery before sending. On uncertain transport failures, never repeat it.
        for item in &mut thread.subagents.mailbox {
            if item["state"] == "queued" {
                item["state"] = json!("dispatched");
            }
        }
        self.store.save(&thread)?;
        if thread.status == "running" {
            self.steer(&json!({"threadId":id,"expectedTurnId":thread.turn.as_ref().unwrap()["id"],"clientUserMessageId":format!("mail:{}",mail[0]["id"].as_str().unwrap()),"input":input})).await?;
        } else {
            Box::pin(self.request("turn/start", json!({"threadId":id,"input":input}))).await?;
        }
        Ok(())
    }
    pub fn cancel_agent_tools(&mut self, thread: &mut Thread, reason: &str) -> Result<()> {
        self.agent_waits.retain(|(id, _), _| id != &thread.id);
        let calls = thread
            .subagents
            .controls
            .values()
            .filter(|r| r.get("result").is_none())
            .map(|r| r["call"].clone())
            .collect::<Vec<_>>();
        for call in calls {
            let result = json!({"error":reason});
            thread
                .subagents
                .controls
                .get_mut(text(&call, "id")?)
                .unwrap()["result"] = result.clone();
            if args(&call).is_ok() {
                self.agent_notice(thread, &call, Some(&result))?;
            }
        }
        Ok(())
    }
    pub fn agent_finished(&self, thread: &mut Thread) -> Result<()> {
        let (Some(parent_id), Some(turn)) = (&thread.subagents.parent_id, &thread.turn) else {
            return Ok(());
        };
        let turn_id = text(turn, "id")?.to_owned();
        if thread.status == "running" || thread.subagents.reported_turn.as_deref() == Some(&turn_id)
        {
            return Ok(());
        }
        let mut parent = self.store.load(parent_id)?;
        let id = format!("completion:{}:{turn_id}", thread.id);
        if !parent.subagents.mailbox.iter().any(|m| m["id"] == id) {
            parent.subagents.mailbox.push(json!({"id":id,"sender":path(thread),"kind":"FINAL_ANSWER","message":status(thread).to_string(),"state":"queued"}));
            self.store.save(&parent)?;
        }
        thread.subagents.reported_turn = Some(turn_id);
        self.store.save(thread)
    }
    pub async fn advance_agents(&mut self) -> Result<()> {
        let ids = self.connections.keys().cloned().collect::<Vec<_>>();
        for id in &ids {
            self.agent_finished(&mut self.store.load(id)?)?;
        }
        for ((id, call_id), deadline) in self.agent_waits.clone() {
            let thread = self.store.load(&id)?;
            let mailbox = !pending(&thread).is_empty();
            let steered = thread.revision
                > thread.subagents.controls[&call_id]["revision"]
                    .as_u64()
                    .unwrap_or(thread.revision);
            if mailbox || steered || std::time::Instant::now() >= deadline {
                self.agent_waits.remove(&(id.clone(), call_id.clone()));
                let call = thread.subagents.controls[&call_id]["call"].clone();
                self.deliver_agent_mail(&id, false).await?;
                self.agent_result(&id,&call,json!({"message":if mailbox{"Wait completed."}else if steered{"Wait interrupted by new input."}else{"Wait timed out."},"timed_out":!mailbox&&!steered})).await?;
            }
        }
        for id in ids {
            let thread = self.store.load(&id)?;
            let wake = thread.status == "idle"
                && pending(&thread).iter().any(|m| m["kind"] == "TASK")
                && self.agent_capacity(&thread).is_ok();
            self.deliver_agent_mail(&id, wake).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (Server, Thread, Thread) {
        let (server, mut parent, _) = crate::tests::fixture();
        parent.status = "running".into();
        parent.turn = Some(json!({"id":"parent-turn","items":[],"status":"inProgress"}));
        server.store.save(&parent).unwrap();
        let mut child = parent.clone();
        child.id = uuid::Uuid::new_v4().to_string();
        child.status = "idle".into();
        child.turn = None;
        child.subagents = State {
            root_id: Some(parent.id.clone()),
            parent_id: Some(parent.id.clone()),
            path: Some("/root/worker".into()),
            ..Default::default()
        };
        server.store.save(&child).unwrap();
        (server, parent, child)
    }
    #[tokio::test]
    async fn mailbox_is_durable_idle_messages_do_not_wake_and_targets_are_tree_scoped() {
        let (mut server, parent, child) = setup();
        server
            .agent_action(
                &parent,
                "send_message",
                &json!({"target":"worker","message":"hello"}),
            )
            .await
            .unwrap();
        server.advance_agents().await.unwrap();
        let saved = server.store.load(&child.id).unwrap();
        assert_eq!(saved.status, "idle");
        assert_eq!(pending(&saved).len(), 1);
        assert!(server.connections.is_empty());
        let mut unrelated = child.clone();
        unrelated.id = uuid::Uuid::new_v4().to_string();
        unrelated.subagents = State::default();
        server.store.save(&unrelated).unwrap();
        assert!(server.agent_target(&parent, &unrelated.id).is_err());
        assert_eq!(server.agent_target(&child, "/root").unwrap().id, parent.id);
        assert!(
            server
                .agent_action(&child, "interrupt_agent", &json!({"target":"/root"}))
                .await
                .is_err()
        );
        assert!(
            server
                .agent_action(
                    &child,
                    "spawn_agent",
                    &json!({"task_name":"nested","message":"go"})
                )
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn waiting_is_nonblocking_and_receipts_prevent_duplicate_delivery() {
        let (mut server, parent, child) = setup();
        let (tx, mut rx) = mpsc::channel(8);
        server.connections.insert(
            parent.id.clone(),
            Connection {
                pid: None,
                generation: "test".into(),
                commands: tx,
            },
        );
        let worker = tokio::spawn(async move {
            let mut values = vec![];
            while let Some(Command::Send(value, reply)) = rx.recv().await {
                values.push(value);
                reply.send(Ok(())).unwrap();
                if values.len() == 4 {
                    break;
                }
            }
            values
        });
        let call = json!({"id":"send","name":"send_message","arguments":{"target":"worker","message":"once"}});
        server.agent_tool(&parent.id, &call).await.unwrap();
        server.agent_tool(&parent.id, &call).await.unwrap();
        assert_eq!(pending(&server.store.load(&child.id).unwrap()).len(), 1);
        let wait = json!({"id":"wait","name":"wait_agent","arguments":{"timeout_ms":10000}});
        server.agent_tool(&parent.id, &wait).await.unwrap();
        assert_eq!(server.agent_waits.len(), 1);
        server
            .queue_agent_message(&parent.id, "/root/worker", "MESSAGE", "reply")
            .unwrap();
        server.advance_agents().await.unwrap();
        assert!(server.agent_waits.is_empty());
        let values = worker.await.unwrap();
        assert_eq!(values[0], values[1]);
        assert!(values[2].get("userInput").is_some());
        assert!(
            values[3]["toolResponse"]["responseJson"]
                .as_str()
                .unwrap()
                .contains("Wait completed")
        );
    }
    #[tokio::test]
    async fn permission_modes_and_unknown_execution_do_not_launch_agents() {
        let (mut server, mut parent, child) = setup();
        parent.options.tools_disabled = true;
        server.store.save(&parent).unwrap();
        let (tx, mut rx) = mpsc::channel(4);
        server.connections.insert(
            parent.id.clone(),
            Connection {
                pid: None,
                generation: "test".into(),
                commands: tx,
            },
        );
        let worker = tokio::spawn(async move {
            let Some(Command::Send(v, r)) = rx.recv().await else {
                panic!()
            };
            r.send(Ok(())).unwrap();
            v
        });
        server.agent_tool(&parent.id,&json!({"id":"disabled","name":"spawn_agent","arguments":{"task_name":"x","message":"go"}})).await.unwrap();
        assert!(
            worker.await.unwrap()["toolResponse"]["responseJson"]
                .as_str()
                .unwrap()
                .contains("unavailable")
        );
        assert_eq!(server.store.all().unwrap().len(), 2);
        let mut unknown = child;
        unknown.status = "unknown".into();
        server.store.save(&unknown).unwrap();
        assert!(
            server
                .agent_action(
                    &parent,
                    "followup_task",
                    &json!({"target":"worker","message":"do not retry"})
                )
                .await
                .is_err()
        );
        assert!(pending(&server.store.load(&unknown.id).unwrap()).is_empty());
    }
    #[test]
    fn capacity_counts_the_root_and_persistent_metadata_survives_roundtrip() {
        let (server, parent, mut child) = setup();
        for i in 0..3 {
            child.id = uuid::Uuid::new_v4().to_string();
            child.status = "running".into();
            child.subagents.path = Some(format!("/root/worker{i}"));
            server.store.save(&child).unwrap();
        }
        assert!(server.agent_capacity(&parent).is_err());
        let saved = server.store.load(&child.id).unwrap();
        assert_eq!(
            source(&saved)["subAgent"]["thread_spawn"]["parent_thread_id"],
            parent.id
        );
        let mut config = json!({"tools":[]});
        native(&saved, &mut config);
        assert!(
            !config["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "spawn_agent")
        );
    }
}
