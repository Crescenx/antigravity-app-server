use crate::*;

pub fn usage(current: &Value, previous: &Value) -> Value {
    let count = |v: &Value, k: &str| {
        v["total"][k]
            .as_u64()
            .or_else(|| v["total"][k].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(0)
    };
    let mut total = json!({});
    let mut last = json!({});
    for (native, key) in [
        ("promptTokenCount", "inputTokens"),
        ("cachedContentTokenCount", "cachedInputTokens"),
        ("candidatesTokenCount", "outputTokens"),
        ("thoughtsTokenCount", "reasoningOutputTokens"),
        ("totalTokenCount", "totalTokens"),
    ] {
        let n = count(current, native);
        total[key] = json!(n);
        last[key] = json!(n.saturating_sub(count(previous, native)));
    }
    json!({"total":total,"last":last,"modelContextWindow":null,"lastScope":"nativeUsageUpdateDelta"})
}

pub fn tool_item(step: &Value, id: &str) -> Option<Value> {
    let status = match step["state"].as_str() {
        Some("STATE_DONE")
            if step["runCommand"]["exitCode"]
                .as_i64()
                .is_some_and(|code| code != 0) =>
        {
            "failed"
        }
        Some("STATE_DONE") => "completed",
        Some("STATE_ERROR") => "failed",
        _ => "inProgress",
    };
    let mut item = json!({"id":id,"status":status,"state":step["state"],"native":step});
    if let Some(command) = step.get("runCommand") {
        item["type"] = json!("commandExecution");
        item["command"] = command["commandLine"].clone();
        item["cwd"] = command["workingDir"].clone();
        item["aggregatedOutput"] = command["combinedOutput"].clone();
        item["exitCode"] = command["exitCode"].clone();
        item["commandActions"] = json!([]);
    } else if let Some(edit) = step.get("editFile") {
        let path = edit["filePath"]
            .as_str()
            .unwrap_or("")
            .strip_prefix("file://")
            .unwrap_or(edit["filePath"].as_str().unwrap_or(""));
        let mut diff = String::new();
        if let Some(blocks) = edit["diffBlock"].as_array() {
            for block in blocks {
                if let Some(lines) = block["lines"].as_array() {
                    for line in lines {
                        diff.push(match line["action"].as_str() {
                            Some("LINE_ACTION_INSERT") => '+',
                            Some("LINE_ACTION_DELETE") => '-',
                            _ => ' ',
                        });
                        diff.push_str(line["text"].as_str().unwrap_or(""));
                        diff.push('\n');
                    }
                }
            }
        }
        item["type"] = json!("fileChange");
        item["changes"] =
            json!([{"path":path,"kind":{"type":"update","move_path":null},"diff":diff}]);
    } else if let Some(read) = step.get("viewFile") {
        item["type"] = json!("dynamicToolCall");
        item["tool"] = json!("view_file");
        item["namespace"] = Value::Null;
        item["durationMs"] = Value::Null;
        item["arguments"] = read.clone();
        let path = read["filePath"].as_str().unwrap_or("");
        let path = path.strip_prefix("file://").unwrap_or(path);
        let summary = json!({"path":path,"startLine":read["startLine"].as_u64().map(|n|n.saturating_add(1)),"endLine":read["endLine"].as_u64().map(|n|n.saturating_add(1)),
            "contentOffset":read["contentOffset"],"summary":step["text"].as_str().unwrap_or("")});
        item["contentItems"] = json!([{"type":"inputText","text":summary.to_string()}]);
        // Older clients render result; standard clients consume contentItems.
        item["result"] = summary;
        item["success"] = match status {
            "completed" => json!(true),
            "failed" => json!(false),
            _ => Value::Null,
        };
    } else {
        let tool = step.get("mcpTool")?;
        item["type"] = json!("mcpToolCall");
        item["server"] = tool["serverName"].clone();
        item["tool"] = tool["toolName"].clone();
        item["arguments"] = serde_json::from_str(tool["argumentsJson"].as_str().unwrap_or("null"))
            .unwrap_or(Value::Null);
        item["result"] =
            json!({"content":[{"type":"text","text":step["text"].as_str().unwrap_or("")}]});
    }
    Some(item)
}

pub fn turn(value: &Value) -> Value {
    let mut turn = value.clone();
    if !turn["items"].is_array() {
        turn["items"] = json!([]);
    }
    if turn["status"] == "unknown" {
        turn["status"] = json!("failed");
        turn["error"] = json!({"message":"Native outcome unknown; input was not replayed","codexErrorInfo":null,"additionalDetails":null});
    } else if turn["error"].is_null() && !turn["nativeStop"]["error"].is_null() {
        let error = &turn["nativeStop"]["error"];
        let message = error
            .as_str()
            .or_else(|| error["message"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| error.to_string());
        turn["error"] = json!({"message":message,"codexErrorInfo":null,"additionalDetails":null});
    } else if turn.get("error").is_none() {
        turn["error"] = Value::Null;
    }
    turn
}

pub fn thread(thread: &Thread) -> Value {
    let mut turns = thread.turns.clone();
    if let Some(current) = &thread.turn {
        if let Some(existing) = turns.iter_mut().find(|t| t["id"] == current["id"]) {
            *existing = current.clone();
        } else {
            turns.push(current.clone());
        }
    }
    let status = match thread.status.as_str() {
        "running" => json!({"type":"active","activeFlags":[]}),
        "unknown" => json!({"type":"systemError"}),
        _ => json!({"type":"idle"}),
    };
    json!({"id":thread.id,"cwd":thread.cwd,"model":models::split(&thread.model).0,"modelProvider":"google","revision":thread.revision,
        "status":status,"tokenUsage":thread.token_usage,"turns":turns.iter().map(turn).collect::<Vec<_>>(),
        "collaborationMode":{"mode":thread.collaboration_mode,"settings":{"model":models::split(&thread.model).0,"reasoning_effort":models::split(&thread.model).1,"developer_instructions":thread.options.developer_instructions}},
        "createdAt":thread.created_at,"updatedAt":thread.updated_at,"preview":"",
        "path":null,"name":thread.name,"ephemeral":thread.options.ephemeral,"source":crate::subagents::source(thread),"cliVersion":env!("CARGO_PKG_VERSION"),
        "agentNickname":thread.subagents.path.as_deref().and_then(|p|p.rsplit('/').next()),"agentRole":if thread.subagents.parent_id.is_some(){json!("default")}else{Value::Null},"gitInfo":null})
}

pub fn response(thread: &Thread, native: Value) -> Value {
    let response = json!({"thread":self::thread(thread),"native":native,"model":models::split(&thread.model).0,
        "modelProvider":"google","cwd":thread.cwd,"approvalPolicy":thread.approval_policy,
        "approvalsReviewer":thread.approvals_reviewer,"guardian":thread.review_config,"reasoningEffort":models::split(&thread.model).1,"collaborationMode":{"mode":thread.collaboration_mode,"settings":{"model":models::split(&thread.model).0,"reasoning_effort":models::split(&thread.model).1,"developer_instructions":thread.options.developer_instructions}},
        "sandbox":thread.permissions().map(|p|p.effective()).unwrap_or(Value::Null)});
    response
}

// Reject policies we cannot enforce before creating a native session.
pub fn policy(params: &Value) -> Result<()> {
    permissions::PermissionPolicy::from_rpc(params, &std::env::current_dir()?)?;
    if let Some(reviewer) = params.get("approvalsReviewer").filter(|v| !v.is_null()) {
        ensure!(
            reviewer == "user" || reviewer == "auto_review",
            "unsupported approvals reviewer"
        );
    }
    crate::session_options::validate(params)?;
    if let Some(mode) = params.get("collaborationMode").filter(|v| !v.is_null()) {
        ensure!(
            mode["mode"] == "default" || mode["mode"] == "plan",
            "unsupported collaboration mode"
        );
        text(&mode["settings"], "model")?;
    }
    if let Some(config) = params.get("config").filter(|v| !v.is_null()) {
        ensure!(
            config.as_object().is_some_and(|o| o.keys().all(|k| [
                "mcp_servers",
                "guardian",
                "tools_disabled",
                "project_doc_max_bytes",
                "project_doc_fallback_filenames",
                "project_root_markers"
            ]
            .contains(&k.as_str()))),
            "unsupported config override"
        );
    }
    Ok(())
}
