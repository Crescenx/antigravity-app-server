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
