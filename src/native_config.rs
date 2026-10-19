//! Configuration for the App language server's native Cascade RPCs.
use crate::*;

pub fn build(store: &Store, thread: &Thread, mcp_servers: Value) -> Result<Value> {
    let sections = instructions::load(&thread.cwd, &thread.options.instruction_config)?;
    build_with_sections(store, thread, mcp_servers, sections)
}

fn build_with_sections(
    store: &Store,
    thread: &Thread,
    mcp_servers: Value,
    mut sections: Vec<Value>,
) -> Result<Value> {
    let options = &thread.options;
    let plan = thread.is_plan();
    let full_access = thread.permissions()?.mode == "dangerFullAccess";
    if subagents::enabled(thread) {
        sections.push(subagents::guidance(thread));
    }
    if let Some(text) = &options.developer_instructions {
        sections.push(json!({"title":"Application instructions","content":text}));
    }
    if !options.tools_disabled && (!full_access || plan) {
        sections.push(json!({"title":"Command execution","content":"Use run_command for noninteractive commands. Commands run under the configured filesystem and network sandbox. Request BypassSandbox only when the specific command needs approval outside that sandbox. Persistent terminals and daemon commands are unavailable in restricted sessions. Use the provided sandboxed file tools for file writes and replacements."}));
    }
    if plan {
        sections.push(json!({"title":"Collaboration mode: Plan","content":"You are in Plan mode. Investigate with read-only tools, clarify requirements, and develop a concrete implementation plan. Do not implement changes, write files, run mutating commands, access the network through tools, bypass sandbox restrictions, invoke external MCP tools or child agents, or activate autonomous goals. Present the plan for user review. This mode remains active until the client explicitly switches to Default."}));
    }
    if let Some(schema) = &options.output_schema {
        sections.push(json!({"title":"Structured final output","content":format!("Return your final answer as a JSON object matching this schema. Return only the JSON object, without Markdown fences or explanatory prose.\n{}",schema)}));
    }
    let enabled = !options.tools_disabled;
    let servers = if enabled && !plan {
        mcp_servers
            .as_array()
            .context("native MCP servers must be an array")?
            .iter()
            .map(|server| -> Result<Value> {
                Ok(json!({
                    "serverName":text(server,"name")?,
                    "serverUrl":text(&server["http"],"url")?,
                    "headers":server["http"]["headers"].as_object().cloned().unwrap_or_default(),
                    "disableStandaloneSse":true,
                    "forceAllToolsEager":true
                }))
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        vec![]
    };
    let skill_paths = if enabled {
        skills::discover(store, &thread.cwd)?["skills"]
            .as_array()
            .context("invalid discovered skills")?
            .iter()
            .filter(|skill| skill["enabled"] == true)
            .map(|skill| json!({"absolute":skill["path"]}))
            .collect::<Vec<_>>()
    } else {
        vec![]
    };
    let discovery = json!({
        "skills":{"inheritUser":false,"skillsPaths":skill_paths},
        "mcp":{"inheritUser":false,"servers":servers},
        "rules":{"inheritUser":false,"rulesPaths":[]},
        "hooks":{"inheritUser":false,"hooksPaths":[]},
        "plugins":{"inheritUser":false,"pluginsPaths":[],"builtinPluginNames":[]},
        "agents":{"inheritUser":false,"agents":[],"agentsPaths":[],"builtinAgentNames":[]},
        "workflows":{"inheritUser":false,"workflowsPaths":[],"builtinWorkflowNames":[]}
    });
    let mut planner = json!({
        "modelName":thread.model,
        "retryConfig":{"apiRetry":{"maxRetries":0},"modelOutputRetry":{"maxRetries":0}},
        "customizationDiscoveryConfig":discovery,
        "toolConfig":{
            "forceDisableToolCalling":!enabled,
            "invokeSubagent":{"enabled":false},
            "code":{"forceDisable":!full_access || plan || !enabled},
            "writeToFile":{"forceDisable":!full_access || plan || !enabled},
            "mcp":{"forceDisable":!enabled || plan,"enableLazyLoad":false,"nonBlockingTurnLoad":false},
            "runCommand":{"forceDisable":!enabled,"maxTimeoutMs":120000,"enablePersistentMode":full_access && !plan,"enableDaemonCommands":full_access && !plan,"readOnlySandbox":plan}
        }
    });
    let mut prompt_sections = Vec::new();
    if let Some(base) = &options.base_instructions {
        prompt_sections.push(json!({"custom":{"title":"System instructions","content":base}}));
    } else {
        prompt_sections.extend(
            [
                "identity",
                "terminal_sandbox",
                "artifacts",
                "conversation_transcript",
                "guidelines",
                "communication_style",
                "slash_commands",
                "messaging",
                "subagents",
                "skills",
                "user_information",
            ]
            .into_iter()
            .map(|name| json!({"builtinName":name})),
        );
    }
    prompt_sections.extend(
        sections
            .into_iter()
            .map(|section| json!({"custom":section})),
    );
    let mut tools = Vec::new();
    if enabled {
        tools.extend(["view_file", "run_command", "ask_question"]);
        if full_access && !plan {
            tools.extend(["write_to_file", "replace_file_content", "manage_task"]);
        }
        if !plan {
            tools.extend(["read_url_content", "search_web"]);
        }
    }
    let named = |names: &[&str]| {
        names
            .iter()
            .map(|name| json!({"name":name,"builtin":true}))
            .collect::<Vec<_>>()
    };
    // The default fundamental set adds manage_task. Restore only the required
    // execution hooks/decorators so tool selection remains explicit.
    planner["declarativeMixinConfig"] = json!({
        "promptSections":prompt_sections,
        "tools":named(&tools),
        "forceDisableFundamentalComponents":true,
        "preInvocationHooks":named(&["message_delivery","battle_mode_max_turns","context_summary"]),
        "postInvocationHooks":named(&["message_continue_check","no_tool_call_check","max_generator_invocations_check"]),
        "preToolHooks":named(&["battle_mode_tool_args"]),
        "toolDecorators":named(&["output_truncation","augmented_intent"])
    });
    let mut executor = json!({});
    if let Some(limit) = thread
        .budget
        .as_ref()
        .and_then(|b| b["maxModelCalls"].as_u64())
    {
        ensure!(
            limit > 0 && limit <= i32::MAX as u64,
            "invalid maxModelCalls budget"
        );
        executor["maxGeneratorInvocations"] = json!(limit);
    }
    let mut external = json!({"tools":if enabled && !full_access && !plan {permissions::file_tool_schemas()} else {vec![]}});
    subagents::native(thread, &mut external);
    let workspace_uris = std::iter::once(&thread.cwd)
        .chain(&thread.additional_directories)
        .map(|path| {
            reqwest::Url::from_directory_path(path)
                .map(|url| url.to_string())
                .map_err(|_| {
                    anyhow::anyhow!(
                        "workspace must be an absolute directory: {}",
                        path.display()
                    )
                })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({
        "cascadeId":thread.id,
        "workspaceUris":workspace_uris,
        "cwd":thread.cwd,
        "modelName":thread.model,
        "cascadeConfig":{"plannerConfig":planner,"executorConfig":executor},
        "budget":thread.budget,
        "externalTools":external["tools"]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_mcp_and_replaces_system_without_sdk_keys() {
        let (server, mut thread, _) = crate::tests::fixture();
        thread.options.base_instructions = Some("BASE".into());
        thread.options.developer_instructions = Some("APP".into());
        let config = build_with_sections(&server.store, &thread,
            json!([{"name":"sample","http":{"url":"http://127.0.0.1:1234/mcp","headers":{"Authorization":"Bearer example"}}}]),
            vec![json!({"title":"Rules","content":"RULES"})]).unwrap();
        let planner = &config["cascadeConfig"]["plannerConfig"];
        assert_eq!(
            planner["declarativeMixinConfig"]["promptSections"][0]["custom"]["content"],
            "BASE"
        );
        assert_eq!(
            planner["declarativeMixinConfig"]["promptSections"][1]["custom"]["content"],
            "RULES"
        );
        assert!(
            planner["declarativeMixinConfig"]["promptSections"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["custom"]["content"] == "APP")
        );
        assert!(planner.get("promptSectionCustomizationConfig").is_none());
        let mcp = &planner["customizationDiscoveryConfig"]["mcp"];
        assert_eq!(mcp["inheritUser"], false);
        assert_eq!(mcp["servers"][0]["serverName"], "sample");
        assert_eq!(
            mcp["servers"][0]["headers"]["Authorization"],
            "Bearer example"
        );
        assert!(config.get("systemInstructions").is_none());
        assert!(config.get("harnessSideTools").is_none());
        assert!(
            config["workspaceUris"][0]
                .as_str()
                .unwrap()
                .starts_with("file:///")
        );
    }

    #[test]
    fn tools_disabled_removes_bridges_and_structured_output_is_plain_json() {
        let (server, mut thread, _) = crate::tests::fixture();
        thread.options.tools_disabled = true;
        thread.options.output_schema = Some(json!({"type":"object"}));
        let config = build_with_sections(&server.store, &thread, json!([]), vec![]).unwrap();
        let planner = &config["cascadeConfig"]["plannerConfig"];
        assert_eq!(planner["toolConfig"]["forceDisableToolCalling"], true);
        assert_eq!(
            planner["customizationDiscoveryConfig"]["mcp"]["servers"],
            json!([])
        );
        assert_eq!(
            planner["customizationDiscoveryConfig"]["skills"]["skillsPaths"],
            json!([])
        );
        assert_eq!(config["externalTools"], json!([]));
        let sections = planner["declarativeMixinConfig"]["promptSections"].to_string();
        assert!(sections.contains("JSON object"));
        assert!(!sections.contains("finish tool"));
    }
    #[test]
    fn model_budget_installs_native_stop_hook_and_preserves_cumulative_limits() {
        let (server, mut thread, _) = crate::tests::fixture();
        thread.budget = Some(
            json!({"maxModelCalls":2,"maxToolCalls":3,"maxTotalTokens":1000,"scope":"BUDGET_SCOPE_LIFETIME"}),
        );
        let config = build_with_sections(&server.store, &thread, json!([]), vec![]).unwrap();
        assert_eq!(config["budget"], thread.budget.clone().unwrap());
        assert_eq!(
            config["cascadeConfig"]["executorConfig"]["maxGeneratorInvocations"],
            2
        );
        let planner = &config["cascadeConfig"]["plannerConfig"];
        let mixin = &planner["declarativeMixinConfig"];
        assert!(
            mixin["postInvocationHooks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|h| h["name"] == "max_generator_invocations_check")
        );
        assert_eq!(mixin["promptSections"][0]["builtinName"], "identity");
        assert!(mixin["tools"].as_array().unwrap().iter().all(|t| !matches!(
            t["name"].as_str(),
            Some("invoke_subagent" | "manage_task" | "write_to_file" | "replace_file_content")
        )));
        assert_eq!(planner["retryConfig"]["apiRetry"]["maxRetries"], 0);
        assert_eq!(planner["retryConfig"]["modelOutputRetry"]["maxRetries"], 0);
        let root = server.store.root.clone();
        drop(server);
        std::fs::remove_dir_all(root).unwrap();
    }
}
