use crate::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Options {
    pub ephemeral: bool,
    pub base_instructions: Option<String>,
    pub developer_instructions: Option<String>,
    pub output_schema: Option<Value>,
    pub tools_disabled: bool,
    pub instruction_config: Value,
}

pub fn validate(params: &Value) -> Result<()> {
    instructions::overrides(&params["config"])?;
    if let Some(v) = params.get("ephemeral").filter(|v| !v.is_null()) {
        ensure!(v.is_boolean(), "ephemeral must be boolean");
    }
    for v in [
        params.get("baseInstructions"),
        params.get("developerInstructions"),
        params["collaborationMode"]["settings"].get("developer_instructions"),
    ]
    .into_iter()
    .flatten()
    {
        ensure!(
            v.is_null() || v.as_str().is_some_and(|s| s.len() <= 128 * 1024),
            "instructions must be text up to 128 KiB or null"
        );
    }
    if let Some(schema) = params.get("outputSchema").filter(|v| !v.is_null()) {
        ensure!(
            schema.is_object() && schema["type"] == "object",
            "outputSchema requires an object root"
        );
        ensure!(
            schema.to_string().len() <= 64 * 1024,
            "outputSchema exceeds 64 KiB"
        );
        jsonschema::validator_for(schema)
            .map_err(|e| anyhow::anyhow!("invalid outputSchema: {e}"))?;
    }
    if let Some(v) = params["config"].get("tools_disabled") {
        ensure!(v.is_boolean(), "tools_disabled must be boolean");
    }
    Ok(())
}

pub fn configure(thread: &mut Thread, params: &Value) -> Result<()> {
    validate(params)?;
    let overrides = instructions::overrides(&params["config"])?;
    if !overrides.as_object().unwrap().is_empty() {
        if !thread.options.instruction_config.is_object() {
            thread.options.instruction_config = json!({});
        }
        for (key, value) in overrides.as_object().unwrap() {
            thread.options.instruction_config[key] = value.clone();
        }
    }
    if let Some(v) = params.get("ephemeral").filter(|v| !v.is_null()) {
        ensure!(
            v == thread.options.ephemeral,
            "cannot change ephemeral lifetime"
        );
    }
    for (key, dest) in [
        ("baseInstructions", &mut thread.options.base_instructions),
        (
            "developerInstructions",
            &mut thread.options.developer_instructions,
        ),
    ] {
        if let Some(v) = params.get(key) {
            *dest = v.as_str().map(str::to_owned);
        }
    }
    if let Some(v) = params["collaborationMode"]["settings"]
        .get("developer_instructions")
        .filter(|v| !v.is_null())
    {
        thread.options.developer_instructions = v.as_str().map(str::to_owned);
    }
    if let Some(v) = params.get("outputSchema") {
        thread.options.output_schema = (!v.is_null()).then(|| v.clone());
    }
    if let Some(v) = params["config"]["tools_disabled"].as_bool() {
        thread.options.tools_disabled = v;
    }
    Ok(())
}

pub fn native(thread: &Thread, config: &mut Value, guidance: &[Value]) {
    let options = &thread.options;
    let mut sections = guidance.to_vec();
    if let Some(text) = &options.developer_instructions {
        sections.push(json!({"title":"Application instructions", "content":text}));
    }
    if !options.tools_disabled && (!thread.unrestricted || thread.is_plan()) {
        sections.push(json!({"title":"Command execution","content":"Use run_command for noninteractive commands. Commands run under the configured filesystem and network sandbox. Request BypassSandbox only when the specific command needs approval outside that sandbox. Persistent terminals, daemon commands and manage_task input are unavailable in restricted sessions; leave RunPersistent false and do not reuse RequestedTerminalID. Shell variables do not persist between calls. File writes and replacements use the provided sandboxed file tools."}));
    }
    if thread.is_plan() {
        sections.push(json!({"title":"Collaboration mode: Plan","content":"You are in Plan mode. Investigate with read-only tools, clarify requirements, and develop a concrete implementation plan. Do not implement changes, write files, run mutating commands, access the network through tools, bypass sandbox restrictions, invoke external MCP tools or child agents, or activate autonomous goals. Present the plan for user review. This mode remains active until the client explicitly switches to Default."}));
    }
    if let Some(schema) = &options.output_schema {
        config["finishToolSchemaJson"] = json!(schema.to_string());
        sections.push(json!({"title":"Structured final output","content":"Finish by calling the finish tool with an object matching its schema. Do not substitute a prose answer or Markdown code block."}));
    }
    if let Some(base) = &options.base_instructions {
        let mut parts = vec![json!({"text":base})];
        parts.extend(sections.iter().map(|s| json!({"text":format!("{}\n{}",s["title"].as_str().unwrap(),s["content"].as_str().unwrap())})));
        config["systemInstructions"] = json!({"custom":{"part":parts}});
    } else if !sections.is_empty() {
        config["systemInstructions"] = json!({"appended":{"appendedSections":sections}});
    }
    if options.tools_disabled {
        config["tools"] = json!([]);
        config["skillsConfig"] = json!({"enabled":false});
        for tool in [
            "find",
            "runCommand",
            "subagents",
            "userQuestions",
            "fileEdit",
            "viewFile",
            "writeToFile",
            "grepSearch",
            "listDir",
            "generateImage",
            "searchWeb",
            "readUrlContent",
            "manageTask",
            "schedule",
        ] {
            config["harnessSideTools"][tool] = json!({"enabled":false});
        }
        config["policyConfig"]["rules"] =
            json!([{"tool":"finish","decision":"POLICY_DECISION_ALLOW"}]);
    }
}

pub fn validate_output(schema: &Value, output: &str) -> Result<()> {
    let value: Value = serde_json::from_str(output).context("structured result is not JSON")?;
    let validator = jsonschema::validator_for(schema)
        .map_err(|e| anyhow::anyhow!("invalid outputSchema: {e}"))?;
    validator
        .validate(&value)
        .map_err(|e| anyhow::anyhow!("structured result does not match outputSchema: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruction_layers_and_tool_disabling_are_explicit() {
        let (server, mut thread, _) = crate::tests::fixture();
        let root = server.store.root.clone();
        configure(&mut thread, &json!({"baseInstructions":"BASE", "developerInstructions":"APP", "config":{"tools_disabled":true}})).unwrap();
        thread.collaboration_mode = "plan".into();
        let mut config =
            json!({"tools":[{"name":"run_command"}],"harnessSideTools":{},"policyConfig":{}});
        native(&thread, &mut config, &[]);
        let parts = &config["systemInstructions"]["custom"]["part"];
        assert_eq!(parts[0]["text"], "BASE");
        assert!(parts[1]["text"].as_str().unwrap().contains("APP"));
        assert!(parts[2]["text"].as_str().unwrap().contains("Plan mode"));
        assert_eq!(config["tools"], json!([]));
        assert_eq!(config["harnessSideTools"]["viewFile"]["enabled"], false);
        assert_eq!(config["skillsConfig"]["enabled"], false);
        configure(&mut thread, &json!({"baseInstructions":null})).unwrap();
        native(&thread, &mut config, &[]);
        assert!(config["systemInstructions"].get("custom").is_none());
        assert_eq!(
            config["systemInstructions"]["appended"]["appendedSections"][0]["content"],
            "APP"
        );
        drop(server);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema_rejects_invalid_outputs_and_external_references() {
        let schema = json!({"type":"object","properties":{"title":{"type":"string","maxLength":40}},"required":["title"],"additionalProperties":false});
        validate(&json!({"outputSchema":schema})).unwrap();
        validate_output(&schema, r#"{"title":"测试标题"}"#).unwrap();
        for text in [
            "not json",
            "{}",
            r#"{"title":3}"#,
            r#"{"title":"ok","extra":true}"#,
        ] {
            assert!(validate_output(&schema, text).is_err());
        }
        assert!(validate(&json!({"outputSchema":{"type":"object","properties":{"x":{"$ref":"https://example.invalid/schema"}}}})).is_err());
        assert!(validate(&json!({"outputSchema":{"type":"object","properties":{"x":{"$ref":"file:///etc/passwd"}}}})).is_err());
    }

    #[test]
    fn ephemeral_state_and_native_files_are_not_in_persistent_history() {
        let (server, mut thread, _) = crate::tests::fixture();
        let root = server.store.root.clone();
        thread.id = uuid::Uuid::new_v4().to_string();
        thread.options.ephemeral = true;
        server.store.save(&thread).unwrap();
        let temporary = server.store.native_root(&thread.id);
        assert_ne!(temporary, root);
        std::fs::write(temporary.join("native-test"), "private").unwrap();
        server
            .store
            .event(&thread.id, &json!({"text":"private"}))
            .unwrap();
        assert_eq!(server.store.load(&thread.id).unwrap().id, thread.id);
        assert_eq!(server.store.list().unwrap().len(), 1);
        assert!(
            !root
                .join("threads")
                .join(format!("{}.json", thread.id))
                .exists()
        );
        assert!(
            !root
                .join("threads")
                .join(format!("{}.jsonl", thread.id))
                .exists()
        );
        server.store.forget_ephemeral(&thread.id);
        assert!(!temporary.exists());
        assert!(server.store.load(&thread.id).is_err());
        drop(server);
        std::fs::remove_dir_all(root).unwrap();
    }
}
