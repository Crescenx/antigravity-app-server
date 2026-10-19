use crate::*;

pub fn supported(name: &str) -> bool {
    matches!(
        name,
        "run_command" | "write_to_file" | "replace_file_content"
    )
}
fn arguments(call: &Value) -> Result<Value> {
    if let Some(raw) = call["argumentsJson"].as_str() {
        Ok(serde_json::from_str(raw)?)
    } else {
        Ok(call["arguments"].clone())
    }
}
fn grant(call: &Value) -> Result<String> {
    Ok(serde_json::to_string(
        &json!({"name":call["name"],"arguments":arguments(call)?}),
    )?)
}
fn item(call: &Value, result: Option<&Value>) -> Result<Value> {
    let args = arguments(call)?;
    let mut item = json!({"id":format!("tool:{}",text(call,"id")?),"status":if let Some(r)=result {if r["success"]==true{"completed"}else{"failed"}}else{"inProgress"},"nativeTool":call});
    if call["name"] == "run_command" {
        item["type"] = json!("commandExecution");
        item["command"] = args["CommandLine"].clone();
        item["cwd"] = args["Cwd"].clone();
        item["commandActions"] = json!([]);
        if let Some(result) = result {
            item["aggregatedOutput"] = json!(format!(
                "{}{}",
                result["output"].as_str().unwrap_or(""),
                result["error"].as_str().unwrap_or("")
            ));
            item["exitCode"] = result["exitCode"].clone();
        }
    } else {
        item["type"] = json!("fileChange");
        item["changes"] = result
            .and_then(|r| r.get("fileChange"))
            .filter(|c| c.is_object())
            .map(|c| json!([c]))
            .unwrap_or(json!([]));
    }
    Ok(item)
}

impl Server {
    fn tool_notice(&self, thread: &mut Thread, call: &Value, result: Option<&Value>) -> Result<()> {
        if call["trajectoryId"]
            .as_str()
            .is_some_and(|id| id != thread.id)
        {
            return Ok(());
        }
        let item = item(call, result)?;
        if let Some(turn) = &mut thread.turn {
            let items = turn["items"].as_array_mut().context("turn items")?;
            if let Some(previous) = items.iter_mut().find(|i| i["id"] == item["id"]) {
                *previous = item.clone();
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
    pub async fn tool_call(&mut self, thread: &mut Thread, call: &Value) -> Result<()> {
        let id = text(call, "id")?;
        if thread.options.tools_disabled {
            return self.send(&thread.id,json!({"toolResponse":{"id":id,"errorMessage":"Tools disabled for this session"}})).await;
        }
        if thread
            .turn
            .as_ref()
            .is_some_and(|t| t["interruptRequested"] == true)
        {
            self.send(&thread.id, json!({"haltRequest":true})).await?;
            return self.send(&thread.id, json!({"toolResponse":{"id":id,"errorMessage":"Turn interrupted; tool not executed"}})).await;
        }
        ensure!(supported(text(call, "name")?), "unknown external tool");
        if let Some(receipt) = thread.tool_receipts.get(id) {
            ensure!(
                receipt["call"] == *call,
                "tool call ID reused with changed arguments"
            );
            if !receipt["result"].is_null() {
                return self.send(&thread.id,json!({"toolResponse":{"id":id,"responseJson":receipt["result"].to_string()}})).await;
            }
            if self.tool_jobs.contains_key(&(thread.id.clone(), id.into()))
                || self
                    .reviews
                    .values()
                    .any(|r| r.matches_tool(&thread.id, id))
                || self
                    .approvals
                    .values()
                    .any(|(t, e)| t == &thread.id && e["externalTool"]["id"] == id)
            {
                return Ok(());
            }
            return self.send(&thread.id,json!({"toolResponse":{"id":id,"errorMessage":"Previous execution outcome unknown; tool was not replayed"}})).await;
        }
        thread
            .tool_receipts
            .insert(id.into(), json!({"call":call,"state":"pending"}));
        self.tool_notice(thread, call, None)?;
        self.store.save(thread)?;
        let args = arguments(call)?;
        if thread.is_plan()
            && (permissions::requests_bypass(&args) || call["name"] != "run_command")
        {
            return self.tool_result(thread, call, &json!({"success":false,"error":"Plan mode permits read-only sandboxed commands only; file changes and sandbox bypass are disabled"})).await;
        }
        if permissions::requests_bypass(&args)
            && !thread.unrestricted
            && !thread.tool_grants.contains(&grant(call)?)
        {
            if thread.approval_policy == "never" {
                return self.tool_result(thread,call,&json!({"success":false,"error":"Sandbox bypass denied: approvals are disabled"})).await;
            }
            if thread.approvals_reviewer == "auto_review" {
                return self.start_review(thread, json!({"externalTool":call}));
            }
            let rpc_id = format!("approval:{}", uuid::Uuid::new_v4());
            self.approvals.insert(
                rpc_id.clone(),
                (thread.id.clone(), json!({"externalTool":call})),
            );
            let file = call["name"] != "run_command";
            let _=self.output.send(json!({"jsonrpc":"2.0","id":rpc_id,"method":if file{"item/fileChange/requestApproval"}else{"item/commandExecution/requestApproval"},"params":{"threadId":thread.id,"turnId":thread.turn.as_ref().map(|t|t["id"].clone()),"itemId":format!("tool:{id}"),"cwd":thread.cwd,"command":args["CommandLine"],"reason":format!("Execute {} outside the filesystem/network sandbox. {}",call["name"].as_str().unwrap(),args["TargetFile"].as_str().unwrap_or("")),"availableDecisions":["accept","acceptForSession","decline","cancel"],"approvalScope":"identicalToolArguments","native":call}}));
            return Ok(());
        }
        self.run_tool(thread, call, true)
    }
    pub(crate) fn run_tool(
        &mut self,
        thread: &mut Thread,
        call: &Value,
        approved: bool,
    ) -> Result<()> {
        let call = call.clone();
        let args = arguments(&call)?;
        let name = text(&call, "name")?.to_owned();
        let call_id = text(&call, "id")?.to_owned();
        let policy = thread.permissions()?;
        let cwd = thread.cwd.clone();
        thread
            .tool_receipts
            .get_mut(&call_id)
            .context("missing tool receipt")?["state"] = json!("running");
        self.store.save(thread)?;
        let generation = self
            .connections
            .get(&thread.id)
            .context("thread disconnected")?
            .generation
            .clone();
        let id = thread.id.clone();
        let sender = self.events.clone();
        let job = tokio::spawn(async move {
            let result = permissions::execute(&policy, &cwd, &name, &args, approved)
                .await
                .unwrap_or_else(|e| json!({"success":false,"error":e.to_string()}));
            let _ = sender.send(RuntimeEvent {
                id,
                generation,
                event: Ok(json!({"wrapperToolResult":{"call":call,"result":result}})),
            });
        });
        self.tool_jobs.insert((thread.id.clone(), call_id), job);
        Ok(())
    }
    pub async fn tool_result(
        &mut self,
        thread: &mut Thread,
        call: &Value,
        result: &Value,
    ) -> Result<()> {
        let id = text(call, "id")?;
        if thread
            .tool_receipts
            .get(id)
            .is_some_and(|r| r["state"] == "unknown" && !r["interruption"].is_null())
        {
            return Ok(());
        }
        self.tool_jobs.remove(&(thread.id.clone(), id.into()));
        thread.tool_receipts.insert(
            id.into(),
            json!({"call":call,"state":"completed","result":result}),
        );
        self.tool_notice(thread, call, Some(result))?;
        self.store.save(thread)?;
        self.send(
            &thread.id,
            json!({"toolResponse":{"id":id,"responseJson":result.to_string()}}),
        )
        .await
    }
    pub async fn tool_approval(&mut self, id: &str, call: &Value, message: &Value) -> Result<()> {
        let mut thread = self.store.load(id)?;
        let decision = message["result"]["decision"].as_str().unwrap_or("");
        if message.get("error").is_none() && ["accept", "acceptForSession"].contains(&decision) {
            if decision == "acceptForSession" {
                thread.tool_grants.insert(grant(call)?);
            }
            self.run_tool(&mut thread, call, true)
        } else {
            self.tool_result(
                &mut thread,
                call,
                &json!({"success":false,"error":"User denied sandbox bypass"}),
            )
            .await
        }
    }
    pub fn abort_tools(&mut self, id: &str) {
        self.cancel_reviews(id);
        let keys = self
            .tool_jobs
            .keys()
            .filter(|(t, _)| t == id)
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            if let Some(job) = self.tool_jobs.remove(&key) {
                job.abort();
            }
        }
    }
    pub fn cancel_tools(&mut self, thread: &mut Thread, reason: &str) -> Result<()> {
        self.abort_tools(&thread.id);
        self.cancel_agent_tools(thread, reason)?;
        self.cancel_native_commands(thread, reason)?;
        let calls = thread
            .tool_receipts
            .values()
            .filter(|receipt| matches!(receipt["state"].as_str(), Some("pending" | "running")))
            .map(|receipt| receipt["call"].clone())
            .collect::<Vec<_>>();
        for call in calls {
            let id = text(&call, "id")?;
            // No result is stored: interrupted work can have partial side effects
            // and a replay must not execute it again or report invented success.
            thread.tool_receipts.insert(
                id.into(),
                json!({"call":call,"state":"unknown","interruption":reason}),
            );
            self.tool_notice(
                thread,
                &call,
                Some(&json!({"success":false,"error":reason,"exitCode":null})),
            )?;
        }
        self.approvals.retain(|_, (id, _)| id != &thread.id);
        self.store.save(thread)?;
        Ok(())
    }
}
