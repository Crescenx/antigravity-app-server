//! Keep the native command tool while binding each execution to its pre-tool authorization.
use crate::*;

fn arguments(call: &Value) -> Result<Value> {
    let args: Value = serde_json::from_str(text(call, "argumentsJson")?)?;
    ensure!(
        args.is_object(),
        "native command arguments must be an object"
    );
    Ok(args)
}

fn validate_native_arguments(args: &Value) -> Result<()> {
    for key in args
        .as_object()
        .context("native command arguments must be an object")?
        .keys()
    {
        ensure!(
            matches!(
                key.as_str(),
                "BypassSandbox"
                    | "CommandLine"
                    | "Cwd"
                    | "NotificationTimeoutSeconds"
                    | "RequestedTerminalID"
                    | "RunPersistent"
                    | "WaitMsBeforeAsync"
                    | "toolAction"
                    | "toolSummary"
            ),
            "unsupported native command argument {key}"
        );
    }
    text(args, "CommandLine")?;
    text(args, "Cwd")?;
    ensure!(
        args.get("BypassSandbox").is_none_or(Value::is_boolean),
        "BypassSandbox must be boolean"
    );
    genai_struct(args)?;
    Ok(())
}

// localharness uses genai.Struct, not google.protobuf.Struct or an ordinary JSON map.
fn genai_struct(args: &Value) -> Result<Value> {
    let fields = args
        .as_object()
        .context("native command arguments must be an object")?
        .iter()
        .map(|(name, value)| {
            let wire = match value {
                Value::String(_) => json!({"stringValue":value}),
                Value::Bool(_) => json!({"boolValue":value}),
                Value::Number(_) => json!({"numberValue":value}),
                _ => bail!("native command argument {name} must be a string, boolean or number"),
            };
            Ok(json!({"name":name,"value":wire}))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({"fields":fields}))
}

fn live_context(server: &Server, thread: &Thread) -> Result<Value> {
    ensure!(
        thread.status == "running",
        "native command has no active turn"
    );
    ensure!(
        !thread.options.tools_disabled,
        "tools disabled for this session"
    );
    let turn = thread.turn.as_ref().context("native command has no turn")?;
    ensure!(turn["interruptRequested"] != true, "turn interrupted");
    let generation = &server
        .connections
        .get(&thread.id)
        .context("thread disconnected")?
        .generation;
    Ok(
        json!({"generation":generation,"turnId":text(turn,"id")?,"revision":thread.revision,
        "permissions":thread.permissions()?.effective(),"approvalPolicy":thread.approval_policy,
        "reviewer":thread.approvals_reviewer,"plan":thread.is_plan()}),
    )
}

fn matches_context(receipt: &Value, context: &Value) -> bool {
    receipt["context"] == *context
}

fn validate_policy(receipt: &Value, call: &Value, args: &Value, context: &Value) -> Result<()> {
    ensure!(
        receipt["state"] == "hookReady",
        "native command authorization already consumed"
    );
    ensure!(
        matches_context(receipt, context),
        "native command authorization became stale"
    );
    ensure!(
        call["toolName"] == "run_command" && call["serverName"].as_str().unwrap_or("").is_empty(),
        "not a native command"
    );
    for field in ["callId", "trajectoryId", "stepIndex"] {
        ensure!(
            receipt[field] == call[field],
            "native command {field} changed after hook"
        );
    }
    ensure!(
        receipt["modified"] == *args,
        "native command arguments changed after hook"
    );
    Ok(())
}

fn grant_key(receipt: &Value) -> Result<String> {
    // Revision and turn deliberately do not appear: an identical command may be granted
    // for this connection, but a reconnect or permission/reviewer change invalidates it.
    let context = &receipt["context"];
    Ok(serde_json::to_string(
        &json!({"nativeCommand":receipt["original"],
        "generation":context["generation"],"permissions":context["permissions"],
        "approvalPolicy":context["approvalPolicy"],"reviewer":context["reviewer"],"plan":context["plan"]}),
    )?)
}

fn item_id(receipt: &Value) -> String {
    if let Some(index) = receipt["actualStepIndex"].as_u64() {
        format!("{}:{index}", receipt["trajectoryId"].as_str().unwrap_or(""))
    } else {
        format!(
            "native-command:{}",
            receipt["callId"].as_str().unwrap_or("")
        )
    }
}

fn execution_line(command: &str) -> String {
    // Native hook and emitted step indices use different domains. A comment gives
    // identical concurrent commands distinct wire identities without extra execution.
    format!("# agy-native-command-{}\n{command}", uuid::Uuid::new_v4())
}

fn lease_path(store: &Store, thread: &Thread, id: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id).context("invalid command lease")?;
    Ok(store
        .native_root(&thread.id)
        .join("command-leases")
        .join(id))
}
fn revoke_lease(store: &Store, thread: &Thread, receipt: &Value) -> Result<()> {
    if let Some(id) = receipt["leaseId"].as_str() {
        match std::fs::remove_file(lease_path(store, thread, id)?) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn process_identity(pid: u32) -> Option<(u32, u32, u32, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields = stat
        .rsplit_once(") ")?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    Some((
        fields.get(1)?.parse().ok()?,
        fields.get(2)?.parse().ok()?,
        fields.get(3)?.parse().ok()?,
        fields.get(19)?.parse().ok()?,
    ))
}
fn terminate_leased_group(
    store: &Store,
    thread: &Thread,
    receipt: &Value,
    owner: Option<u32>,
) -> Result<()> {
    let (Some(id), Some(owner)) = (receipt["leaseId"].as_str(), owner) else {
        return Ok(());
    };
    let pid_path = lease_path(store, thread, id)?.with_extension("pid");
    let Ok(identity) = std::fs::read_to_string(&pid_path) else {
        return Ok(());
    };
    let mut parts = identity.split_whitespace();
    let pid = parts.next().and_then(|s| s.parse::<u32>().ok());
    let birth = parts.next().and_then(|s| s.parse::<u64>().ok());
    if let (Some(pid), Some(birth)) = (pid, birth)
        && let Some((mut parent, group, session, started)) = process_identity(pid)
        && pid > 1
        && group == pid
        && session == pid
        && birth == started
    {
        // Never trust a PID file alone: bind both its start time and ancestry to
        // this native runtime, so stale or altered files cannot kill other work.
        for _ in 0..128 {
            if parent == owner {
                unsafe extern "C" {
                    fn kill(pid: std::ffi::c_int, signal: std::ffi::c_int) -> std::ffi::c_int;
                }
                // SAFETY: verified live session leader belonging to this runtime.
                unsafe {
                    kill(-(pid as std::ffi::c_int), 9);
                }
                break;
            }
            if parent <= 1 {
                break;
            }
            let Some((next, _, _, _)) = process_identity(parent) else {
                break;
            };
            parent = next;
        }
    }
    let _ = std::fs::remove_file(pid_path);
    Ok(())
}
fn leased_line(store: &Store, thread: &Thread, command: &str) -> Result<(String, String)> {
    let id = uuid::Uuid::new_v4().to_string();
    let path = lease_path(store, thread, &id)?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    let lease =
        permissions::shell_quote(path.to_str().context("command lease path must be UTF-8")?)?;
    // Native halt can leave its shell running. A separate session and revocable
    // lease cancel the whole foreground process group, including late launches.
    let script = format!(
        "lease={lease}\ntrap 'kill \"${{watcher:-}}\" 2>/dev/null; /bin/rm -f -- \"$lease\" \"$lease.pid\"' EXIT\nread -r -a identity < /proc/$$/stat\nprintf '%s %s\\n' \"$$\" \"${{identity[21]}}\" > \"$lease.pid\"\n[ -f \"$lease\" ] || exit 130\n(while [ -f \"$lease\" ] && kill -0 {} 2>/dev/null; do /bin/sleep 0.05; done; kill -KILL -- -$$) &\nwatcher=$!\n{command}",
        std::process::id()
    );
    Ok((
        format!(
            "/usr/bin/setsid --wait /bin/bash --noprofile --norc -c {}",
            permissions::shell_quote(&script)?
        ),
        id,
    ))
}

fn step_receipt(
    receipts: &BTreeMap<String, Value>,
    step: &Value,
    turn_id: &Value,
) -> Result<Option<String>> {
    let index = step["stepIndex"]
        .as_u64()
        .context("native command step index missing")?;
    let command = &step["runCommand"]["commandLine"];
    let bound = receipts
        .iter()
        .filter(|(_, receipt)| {
            receipt["trajectoryId"] == step["trajectoryId"]
                && receipt["turnId"] == *turn_id
                && receipt["actualStepIndex"].as_u64() == Some(index)
        })
        .collect::<Vec<_>>();
    ensure!(bound.len() <= 1, "ambiguous native command step binding");
    if let Some((id, receipt)) = bound.first() {
        ensure!(
            command.is_null() || *command == receipt["modified"]["CommandLine"],
            "native command changed after step binding"
        );
        return Ok(Some((*id).clone()));
    }
    let matching = receipts
        .iter()
        .filter(|(_, receipt)| {
            receipt["trajectoryId"] == step["trajectoryId"]
                && receipt["turnId"] == *turn_id
                && receipt["actualStepIndex"].is_null()
                && matches!(
                    receipt["state"].as_str(),
                    Some("running" | "denied" | "unknown")
                )
                && command.is_string()
                && *command == receipt["modified"]["CommandLine"]
        })
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    ensure!(
        matching.len() <= 1,
        "ambiguous native command execution identity"
    );
    Ok(matching.into_iter().next())
}

pub(crate) fn mark_unknown(thread: &mut Thread, reason: &str) -> Result<Vec<Value>> {
    let calls = thread
        .native_command_receipts
        .iter()
        .filter(|(_, r)| {
            matches!(
                r["state"].as_str(),
                Some("hookReady" | "awaitingApproval" | "running")
            )
        })
        .map(|(id, r)| (id.clone(), r.clone()))
        .collect::<Vec<_>>();
    let mut notices = Vec::new();
    for (id, receipt) in calls {
        let stored = thread.native_command_receipts.get_mut(&id).unwrap();
        stored["state"] = json!("unknown");
        stored["interruption"] = json!(reason);
        if receipt["trajectoryId"] != thread.id {
            continue;
        }
        if let Some(turn) = &mut thread.turn {
            if turn["id"] != receipt["turnId"] {
                continue;
            }
            let mut item = json!({"id":item_id(&receipt),"type":"commandExecution","status":"failed","state":"STATE_ERROR",
                "command":receipt["original"]["CommandLine"],"cwd":receipt["original"]["Cwd"],"commandActions":[],
                "aggregatedOutput":reason,"exitCode":null,"nativeCommandOutcome":"unknown"});
            if turn["items"].is_null() {
                turn["items"] = json!([]);
            }
            let items = turn["items"].as_array_mut().context("turn items")?;
            if let Some(previous) = items.iter_mut().find(|i| i["id"] == item["id"]) {
                if let Some(output) = previous["aggregatedOutput"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                {
                    item["aggregatedOutput"] = json!(format!("{output}\n{reason}"));
                }
                *previous = item.clone();
            } else {
                items.push(item.clone());
            }
            notices.push(json!({"threadId":thread.id,"turnId":turn["id"],"item":item}));
        }
    }
    thread.checkpoint_turn();
    Ok(notices)
}

impl Server {
    pub(crate) async fn native_hook(&mut self, thread: &mut Thread, hook: &Value) -> Result<()> {
        let request_id = text(hook, "requestId")?;
        let Some(call) = hook.get("preToolArgs") else {
            return self
                .send(
                    &thread.id,
                    json!({"callHookResponse":{"requestId":request_id,"emptyResult":{}}}),
                )
                .await;
        };
        if call["toolName"] != "run_command"
            || !call["serverName"].as_str().unwrap_or("").is_empty()
        {
            return self.send(&thread.id,json!({"callHookResponse":{"requestId":request_id,"preToolResult":{"decision":"ALLOW"}}})).await;
        }
        let prepared = (|| -> Result<(String, Value)> {
            let context = live_context(self, thread)?;
            let call_id = text(call, "callId")?.to_owned();
            ensure!(
                !thread.native_command_receipts.contains_key(&call_id),
                "native command call ID reused; not replayed"
            );
            ensure!(
                text(call, "trajectoryId")? == thread.id,
                "native commands from child trajectories are disabled"
            );
            ensure!(
                call["stepIndex"].as_u64().is_some(),
                "native command step index missing"
            );
            let original = arguments(call)?;
            validate_native_arguments(&original)?;
            let policy = thread.permissions()?;
            let full = policy.mode == "dangerFullAccess";
            if !full {
                ensure!(
                    original.get("RunPersistent").is_none_or(|v| v == false),
                    "persistent terminals are disabled for restricted commands"
                );
                ensure!(
                    original
                        .get("RequestedTerminalID")
                        .is_none_or(|v| v.as_str() == Some("")),
                    "persistent terminal reuse is disabled for restricted commands"
                );
            }
            ensure!(
                !thread.is_plan() || !permissions::requests_bypass(&original),
                "Plan mode forbids sandbox bypass"
            );
            let mut modified = original.clone();
            let command = permissions::native_command_line(&policy, &thread.cwd, &original)?;
            let (command, lease) = if original["RunPersistent"] == true
                || original["RequestedTerminalID"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
            {
                (command, None)
            } else {
                let (line, id) = leased_line(&self.store, thread, &command)?;
                (line, Some(id))
            };
            modified["CommandLine"] = json!(execution_line(&command));
            // The native sandbox is not the permission boundary for wrapped commands.
            // This prevents its restricted outer environment from blocking the broker.
            modified["BypassSandbox"] = json!(true);
            let receipt = json!({"leaseId":lease,"originalCall":call,"original":original,"modified":modified,
                "callId":call_id,"trajectoryId":call["trajectoryId"],"stepIndex":call["stepIndex"],
                "turnId":context["turnId"],"revision":context["revision"],"generation":context["generation"],
                "context":context,"state":"hookReady","fullAccess":full,"hookRequestId":request_id});
            Ok((call_id, receipt))
        })();
        match prepared {
            Ok((call_id, receipt)) => {
                let modified = genai_struct(&receipt["modified"])?;
                thread.native_command_receipts.insert(call_id, receipt);
                self.store.save(thread)?;
                self.send(&thread.id,json!({"callHookResponse":{"requestId":request_id,"preToolResult":{"decision":"ALLOW","modifiedArgs":modified}}})).await
            }
            Err(error) => self.send(&thread.id,json!({"callHookResponse":{"requestId":request_id,"preToolResult":{"decision":"DENY","reason":error.to_string()}}})).await,
        }
    }

    pub(crate) async fn native_command_policy(
        &mut self,
        thread: &mut Thread,
        request: &Value,
    ) -> Result<()> {
        let request_id = text(request, "requestId")?;
        let call = &request["toolArgs"];
        let prepared = (|| -> Result<(String, Value)> {
            let call_id = text(call, "callId")?.to_owned();
            let receipt = thread
                .native_command_receipts
                .get(&call_id)
                .context("native command missing pre-tool authorization")?;
            validate_policy(
                receipt,
                call,
                &arguments(call)?,
                &live_context(self, thread)?,
            )?;
            Ok((call_id, receipt.clone()))
        })();
        let (call_id, receipt) = match prepared {
            Ok(value) => value,
            Err(error) => return self.send(&thread.id,json!({"policyDecisionResponse":{"requestId":request_id,"outcome":"POLICY_EVALUATION_OUTCOME_DENY","denyReason":error.to_string()}})).await,
        };
        let mut original_request = request.clone();
        original_request["toolArgs"] = receipt["originalCall"].clone();
        let target = json!({"nativeCommand":{"callId":call_id,"requestId":request_id,"generation":receipt["generation"]},"nativeRequest":original_request});
        let stored = thread.native_command_receipts.get_mut(&call_id).unwrap();
        stored["state"] = json!("awaitingApproval");
        stored["policyRequestId"] = json!(request_id);
        stored["target"] = target.clone();
        self.store.save(thread)?;
        let bypass = permissions::requests_bypass(&receipt["original"]);
        if !bypass || receipt["fullAccess"] == true {
            return self
                .native_command_approval(thread, &target, true, false, "")
                .await;
        }
        if thread.is_plan() || thread.approval_policy == "never" {
            return self
                .native_command_approval(
                    thread,
                    &target,
                    false,
                    false,
                    "Sandbox bypass is disabled for this turn",
                )
                .await;
        }
        if thread.tool_grants.contains(&grant_key(&receipt)?) {
            return self
                .native_command_approval(thread, &target, true, false, "")
                .await;
        }
        if thread.approvals_reviewer == "auto_review" {
            if let Err(error) = self.start_review(thread, target.clone()) {
                return self
                    .native_command_approval(
                        thread,
                        &target,
                        false,
                        false,
                        &format!("Automatic review unavailable: {error}"),
                    )
                    .await;
            }
            return Ok(());
        }
        let rpc_id = format!("approval:{}", uuid::Uuid::new_v4());
        self.approvals
            .insert(rpc_id.clone(), (thread.id.clone(), target.clone()));
        let _ = self.output.send(json!({"jsonrpc":"2.0","id":rpc_id,"method":"item/commandExecution/requestApproval","params":{
            "threadId":thread.id,"turnId":receipt["turnId"],"itemId":item_id(&receipt),"startedAtMs":now()*1000,
            "cwd":receipt["original"]["Cwd"],"command":receipt["original"]["CommandLine"],
            "reason":"Execute this command outside the filesystem/network sandbox. Session approval applies only to identical arguments under the current permissions.",
            "availableDecisions":["accept","acceptForSession","decline","cancel"],"approvalScope":"identicalToolArguments","native":original_request}}));
        Ok(())
    }

    pub(crate) async fn native_command_approval(
        &mut self,
        thread: &mut Thread,
        target: &Value,
        allow: bool,
        session: bool,
        reason: &str,
    ) -> Result<()> {
        let call_id = text(&target["nativeCommand"], "callId")?;
        let request_id = text(&target["nativeCommand"], "requestId")?;
        let Some(receipt) = thread.native_command_receipts.get(call_id).cloned() else {
            return Ok(());
        };
        // Never deliver an old request ID to a replacement native connection.
        if self
            .connections
            .get(&thread.id)
            .is_none_or(|c| receipt["generation"] != c.generation)
        {
            return Ok(());
        }
        if receipt["state"] != "awaitingApproval"
            || receipt["target"] != *target
            || receipt["policyRequestId"] != request_id
        {
            return self.send(&thread.id,json!({"policyDecisionResponse":{"requestId":request_id,"outcome":"POLICY_EVALUATION_OUTCOME_DENY","denyReason":"Native command approval is stale or already consumed"}})).await;
        }
        let fresh =
            live_context(self, thread).is_ok_and(|context| matches_context(&receipt, &context));
        let bypass = permissions::requests_bypass(&receipt["original"]);
        let eligible = !bypass
            || receipt["fullAccess"] == true
            || (!thread.is_plan() && thread.approval_policy != "never");
        let allowed = allow && fresh && eligible;
        if allowed && session && bypass && receipt["fullAccess"] != true {
            thread.tool_grants.insert(grant_key(&receipt)?);
        }
        if !allowed {
            revoke_lease(&self.store, thread, &receipt)?;
        }
        let stored = thread.native_command_receipts.get_mut(call_id).unwrap();
        stored["state"] = json!(if allowed { "running" } else { "denied" });
        stored["approval"] =
            json!({"allowed":allowed,"session":allowed && session,"reason":reason});
        self.store.save(thread)?;
        self.send(&thread.id,json!({"policyDecisionResponse":{"requestId":request_id,"outcome":if allowed {"POLICY_EVALUATION_OUTCOME_ALLOW"}else{"POLICY_EVALUATION_OUTCOME_DENY"},
            "denyReason":if allowed {""}else if !fresh || !eligible {"Native command approval became stale or is forbidden by current permissions"}else{reason}}})).await
    }

    pub(crate) fn cancel_native_commands(
        &mut self,
        thread: &mut Thread,
        reason: &str,
    ) -> Result<()> {
        for receipt in thread.native_command_receipts.values() {
            revoke_lease(&self.store, thread, receipt)?;
            terminate_leased_group(
                &self.store,
                thread,
                receipt,
                self.connections.get(&thread.id).and_then(|c| c.pid),
            )?;
        }
        for notice in mark_unknown(thread, reason)? {
            notify(&self.output, "item/completed", notice);
        }
        self.approvals
            .retain(|_, (id, target)| id != &thread.id || target.get("nativeCommand").is_none());
        thread.checkpoint_turn();
        self.store.save(thread)?;
        Ok(())
    }

    pub(crate) fn native_command_step(
        &mut self,
        thread: &mut Thread,
        step: &Value,
        item: &mut Value,
    ) -> Result<()> {
        if !step["runCommand"].is_object() {
            return Ok(());
        }
        let turn_id = thread
            .turn
            .as_ref()
            .map(|turn| turn["id"].clone())
            .unwrap_or(Value::Null);
        let Some(call_id) = step_receipt(&thread.native_command_receipts, step, &turn_id)? else {
            // A native command without a matching authorization must not expose an
            // unrelated receipt or raw broker command as a Viewer command item.
            item["type"] = json!("nativeStep");
            return Ok(());
        };
        let receipt = thread.native_command_receipts.get_mut(&call_id).unwrap();
        let newly_bound = receipt["actualStepIndex"].is_null();
        receipt["actualStepIndex"] = step["stepIndex"].clone();
        if receipt["state"] == "unknown"
            || thread
                .turn
                .as_ref()
                .is_none_or(|turn| turn["id"] != receipt["turnId"])
        {
            item["type"] = json!("nativeStep");
            if newly_bound {
                self.store.save(thread)?;
            }
            return Ok(());
        }
        item["command"] = receipt["original"]["CommandLine"].clone();
        item["cwd"] = receipt["original"]["Cwd"].clone();
        if item["native"]["runCommand"].is_object() {
            item["native"]["runCommand"]["commandLine"] =
                receipt["original"]["CommandLine"].clone();
        }
        if matches!(step["state"].as_str(), Some("STATE_DONE" | "STATE_ERROR"))
            && receipt["state"] == "running"
        {
            receipt["state"] = json!("completed");
            receipt["result"] =
                json!({"exitCode":step["runCommand"]["exitCode"],"state":step["state"]});
        }
        self.store.save(thread)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Value, Value, Value, Value) {
        let context = json!({"generation":"g","turnId":"t","revision":7,"permissions":{"type":"workspaceWrite"},"approvalPolicy":"on-request","reviewer":"user","plan":false});
        let args =
            json!({"CommandLine":"sandbox-helper -- original","Cwd":"/work","BypassSandbox":true});
        let call = json!({"toolName":"run_command","serverName":"","callId":"c","trajectoryId":"t","stepIndex":4});
        let receipt = json!({"state":"hookReady","context":context,"modified":args,"original":{"CommandLine":"original","Cwd":"/work","BypassSandbox":false},"callId":"c","trajectoryId":"t","stepIndex":4});
        (receipt, call, args, context)
    }
    #[test]
    fn recovery_closes_visible_items_without_inventing_command_results() {
        let (mut server, mut thread, _) = crate::tests::fixture();
        thread.status = "running".into();
        let id = format!("{}:3", thread.id);
        thread.turn = Some(
            json!({"id":"turn","status":"inProgress","items":[{"id":id,"type":"commandExecution","status":"inProgress","command":"old wire","aggregatedOutput":"partial output"}]}),
        );
        let wire = execution_line("sleep 10");
        thread.native_command_receipts.insert("call".into(),json!({"callId":"call","trajectoryId":thread.id,"stepIndex":6,"actualStepIndex":3,"turnId":"turn","state":"running","original":{"CommandLine":"sleep 10","Cwd":thread.cwd},"modified":{"CommandLine":wire}}));
        let notices = mark_unknown(&mut thread, "Process restarted; unknown").unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(thread.native_command_receipts["call"]["state"], "unknown");
        assert!(thread.native_command_receipts["call"]["result"].is_null());
        let item = &thread.turn.as_ref().unwrap()["items"][0];
        assert_eq!(item["id"], id);
        assert_eq!(item["status"], "failed");
        assert_eq!(item["command"], "sleep 10");
        assert_eq!(
            item["aggregatedOutput"],
            "partial output\nProcess restarted; unknown"
        );
        assert!(item["exitCode"].is_null());
        assert_eq!(item["nativeCommandOutcome"], "unknown");
        assert_eq!(thread.turns[0]["items"][0], *item);
        assert!(
            mark_unknown(&mut thread, "duplicate disconnect")
                .unwrap()
                .is_empty()
        );
        let step = json!({"trajectoryId":thread.id,"stepIndex":3,"state":"STATE_DONE","runCommand":{"commandLine":wire,"exitCode":0}});
        let mut late = compat::tool_item(&step, &id).unwrap();
        server
            .native_command_step(&mut thread, &step, &mut late)
            .unwrap();
        assert_eq!(late["type"], "nativeStep");
        assert_eq!(thread.native_command_receipts["call"]["state"], "unknown");
        let root = server.store.root.clone();
        drop(server);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_steps_bind_execution_identity_not_hook_indices() {
        let (mut cat, _, _, _) = fixture();
        cat["state"] = json!("running");
        cat["turnId"] = json!("turn");
        cat["stepIndex"] = json!(6);
        cat["modified"]["CommandLine"] = json!(execution_line("cat file.txt"));
        let mut curl = cat.clone();
        curl["callId"] = json!("curl-call");
        curl["stepIndex"] = json!(12);
        curl["modified"]["CommandLine"] = json!(execution_line("curl localhost"));
        let mut receipts =
            BTreeMap::from([("cat-call".into(), cat), ("curl-call".into(), curl.clone())]);
        let step = json!({"trajectoryId":"t","stepIndex":6,"runCommand":{"commandLine":curl["modified"]["CommandLine"]}});
        assert_eq!(
            step_receipt(&receipts, &step, &json!("turn")).unwrap(),
            Some("curl-call".into())
        );
        receipts.get_mut("curl-call").unwrap()["actualStepIndex"] = json!(6);
        assert_eq!(item_id(&receipts["curl-call"]), "t:6");
        assert_eq!(item_id(&receipts["cat-call"]), "native-command:c");
        let mut changed = step.clone();
        changed["runCommand"]["commandLine"] = json!("different command");
        assert!(step_receipt(&receipts, &changed, &json!("turn")).is_err());
        assert_eq!(
            step_receipt(&receipts, &step, &json!("next-turn")).unwrap(),
            None
        );
    }

    #[test]
    fn identical_concurrent_commands_have_unique_nonexecuting_wire_identity() {
        let first = execution_line("printf same");
        let second = execution_line("printf same");
        assert_ne!(first, second);
        for line in [&first, &second] {
            assert!(line.starts_with("# agy-native-command-"));
            assert_eq!(line.split_once('\n').unwrap().1, "printf same");
        }
        let (mut receipt, _, _, _) = fixture();
        receipt["state"] = json!("running");
        receipt["turnId"] = json!("turn");
        receipt["modified"]["CommandLine"] = json!(first);
        let mut other = receipt.clone();
        other["modified"]["CommandLine"] = json!(second);
        let mut receipts = BTreeMap::from([
            ("first".into(), receipt.clone()),
            ("second".into(), other.clone()),
        ]);
        let step = json!({"trajectoryId":"t","stepIndex":3,"runCommand":{"commandLine":other["modified"]["CommandLine"]}});
        assert_eq!(
            step_receipt(&receipts, &step, &json!("turn")).unwrap(),
            Some("second".into())
        );
        receipts.insert("duplicate".into(), other);
        assert!(step_receipt(&receipts, &step, &json!("turn")).is_err());
    }

    #[test]
    fn modified_arguments_use_genai_struct_and_reject_unsupported_values() {
        assert_eq!(
            genai_struct(
                &json!({"CommandLine":"printf safe","BypassSandbox":true,"WaitMsBeforeAsync":1000})
            )
            .unwrap(),
            json!({"fields":[
                {"name":"BypassSandbox","value":{"boolValue":true}},
                {"name":"CommandLine","value":{"stringValue":"printf safe"}},
                {"name":"WaitMsBeforeAsync","value":{"numberValue":1000}}
            ]})
        );
        for value in [Value::Null, json!([]), json!({}), json!(["x"])] {
            assert!(genai_struct(&json!({"CommandLine":value})).is_err());
        }
        for value in [Value::Null, json!(true), json!("x"), json!(4), json!([])] {
            assert!(genai_struct(&value).is_err());
        }
    }
    #[test]
    fn command_arguments_do_not_accept_external_tool_permission_aliases() {
        let args = json!({"CommandLine":"printf harmless","Cwd":"/work","BypassSandbox":false});
        validate_native_arguments(&args).unwrap();
        let mut injected = args.clone();
        injected["sandbox_permissions"] = json!("require_escalated");
        assert!(validate_native_arguments(&injected).is_err());
        let mut malformed = args;
        malformed["BypassSandbox"] = json!("false");
        assert!(validate_native_arguments(&malformed).is_err());
    }
    #[test]
    fn policy_requires_exact_single_use_binding() {
        let (receipt, call, args, context) = fixture();
        validate_policy(&receipt, &call, &args, &context).unwrap();
        for field in ["callId", "trajectoryId", "stepIndex"] {
            let mut changed = call.clone();
            changed[field] = json!("different");
            assert!(validate_policy(&receipt, &changed, &args, &context).is_err());
        }
        let mut changed = args.clone();
        changed["CommandLine"] = json!("unrestricted command");
        assert!(validate_policy(&receipt, &call, &changed, &context).is_err());
        for state in [
            "running",
            "awaitingApproval",
            "completed",
            "unknown",
            "denied",
        ] {
            let mut used = receipt.clone();
            used["state"] = json!(state);
            assert!(validate_policy(&used, &call, &args, &context).is_err());
        }
        for field in [
            "generation",
            "turnId",
            "revision",
            "permissions",
            "approvalPolicy",
            "reviewer",
            "plan",
        ] {
            let mut stale = context.clone();
            stale[field] = Value::Null;
            assert!(validate_policy(&receipt, &call, &args, &stale).is_err());
        }
    }
    #[test]
    fn session_grants_bind_original_arguments_and_permission_generation() {
        let (receipt, _, _, _) = fixture();
        let key = grant_key(&receipt).unwrap();
        let mut later = receipt.clone();
        later["context"]["revision"] = json!(8);
        later["context"]["turnId"] = json!("next-turn");
        assert_eq!(grant_key(&later).unwrap(), key);
        for field in [
            "generation",
            "permissions",
            "approvalPolicy",
            "reviewer",
            "plan",
        ] {
            let mut changed = receipt.clone();
            changed["context"][field] = Value::Null;
            assert_ne!(grant_key(&changed).unwrap(), key);
        }
        let mut changed = receipt.clone();
        changed["original"]["CommandLine"] = json!("another command");
        assert_ne!(grant_key(&changed).unwrap(), key);
    }
}

#[cfg(test)]
mod lease_tests {
    use super::*;
    #[tokio::test]
    async fn revocation_stops_foreground_group_and_prevents_a_late_launch() {
        let (server, thread, _) = crate::tests::fixture();
        let started = thread.cwd.join("lease-started");
        let late = thread.cwd.join("lease-late");
        let command = format!(
            "printf started > {}; /bin/sleep 0.3; printf late > {}",
            permissions::shell_quote(started.to_str().unwrap()).unwrap(),
            permissions::shell_quote(late.to_str().unwrap()).unwrap()
        );
        let (line, id) = leased_line(&server.store, &thread, &command).unwrap();
        let mut process = tokio::process::Command::new("/bin/bash")
            .args(["--noprofile", "--norc", "-c", &line])
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !started.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        revoke_lease(&server.store, &thread, &json!({"leaseId":id})).unwrap();
        assert!(
            !tokio::time::timeout(std::time::Duration::from_secs(2), process.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
        tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        assert!(!late.exists());
        let (line, id) = leased_line(&server.store, &thread, &command).unwrap();
        revoke_lease(&server.store, &thread, &json!({"leaseId":id})).unwrap();
        let output = tokio::process::Command::new("/bin/bash")
            .args(["--noprofile", "--norc", "-c", &line])
            .output()
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(130));
        assert!(!late.exists());
    }
    #[tokio::test]
    async fn normal_exit_preserves_output_and_status_and_removes_lease() {
        let (server, thread, _) = crate::tests::fixture();
        let (line, id) =
            leased_line(&server.store, &thread, "printf exact-output; exit 7").unwrap();
        let output = tokio::process::Command::new("/bin/bash")
            .args(["--noprofile", "--norc", "-c", &line])
            .output()
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout, b"exact-output");
        assert!(!lease_path(&server.store, &thread, &id).unwrap().exists());
    }
}
