//! Persistent per-cascade limits. The caller serializes access with the native session.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

const TOKEN_LIMITS: [&str; 3] = ["maxInputTokens", "maxOutputTokens", "maxTotalTokens"];

#[derive(Default, Serialize, Deserialize)]
struct State {
    config: Option<Value>,
    model_calls: u64,
    tool_calls: u64,
    model_baseline: u64,
    tool_baseline: u64,
    tool_ids: BTreeSet<String>,
    #[serde(default)]
    tokens: [u64; 3],
    #[serde(default)]
    token_baseline: [u64; 3],
}

pub struct Budget {
    path: PathBuf,
    state: State,
    model_usage_known: bool,
    token_usage_known: bool,
}

impl Budget {
    /// `usage` must describe the whole root trajectory; `historical_tools` counts
    /// its existing tool calls. Read both from the initial native snapshot.
    pub fn open(
        data: &Path,
        config: Option<&Value>,
        usage: &Value,
        historical_tools: u64,
    ) -> Result<Self> {
        validate(config)?;
        std::fs::create_dir_all(data)?;
        let path = data.join("budget.json");
        let mut state: State = match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("invalid persisted native budget")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e.into()),
        };
        let calls = model_calls(usage);
        state.model_calls = state.model_calls.max(calls.unwrap_or(0));
        state.tool_calls = state.tool_calls.max(historical_tools);
        let tokens = token_counts(usage);
        if let Some(tokens) = tokens {
            for (saved, observed) in state.tokens.iter_mut().zip(tokens) {
                *saved = (*saved).max(observed);
            }
        }
        let config = config.filter(|v| !v.is_null()).cloned();
        if state.config != config {
            if config
                .as_ref()
                .is_some_and(|b| b["scope"] == "BUDGET_SCOPE_FORWARD_LOOKING")
            {
                ensure!(
                    calls.is_some() || config.as_ref().unwrap().get("maxModelCalls").is_none(),
                    "cannot establish forward-looking model budget from incomplete native usage"
                );
                state.model_baseline = state.model_calls;
                state.tool_baseline = state.tool_calls;
                ensure!(
                    !TOKEN_LIMITS
                        .iter()
                        .any(|key| config.as_ref().unwrap().get(*key).is_some())
                        || tokens.is_some()
                        || calls == Some(0) && state.model_calls == 0,
                    "cannot establish forward-looking token budget from incomplete native usage"
                );
                state.token_baseline = state.tokens;
            } else {
                state.model_baseline = 0;
                state.tool_baseline = 0;
                state.token_baseline = [0; 3];
            }
            state.config = config;
        }
        let token_usage_known = tokens.is_some() && calls == Some(state.model_calls);
        let budget = Self {
            path,
            state,
            model_usage_known: calls.is_some(),
            token_usage_known,
        };
        budget.save()?;
        Ok(budget)
    }

    /// Install the returned cap as executorConfig.maxGeneratorInvocations only
    /// at a new invocation, never for a steered message in the active invocation.
    pub fn prepare_turn(&self) -> Result<Option<u32>> {
        if let Some(reason) = self.token_stop_reason() {
            bail!("{reason}");
        }
        if let Some(limit) = self.limit("maxToolCalls") {
            ensure!(
                self.state
                    .tool_calls
                    .saturating_sub(self.state.tool_baseline)
                    < limit,
                "maxToolCalls budget exhausted"
            );
        }
        let Some(limit) = self.limit("maxModelCalls") else {
            return Ok(None);
        };
        ensure!(
            self.model_usage_known,
            "cannot enforce model budget with incomplete native usage"
        );
        let used = self
            .state
            .model_calls
            .saturating_sub(self.state.model_baseline);
        ensure!(used < limit, "maxModelCalls budget exhausted");
        Ok(Some((limit - used) as u32))
    }

    /// Feed cumulative usageUpdate events, including when no budget is enabled.
    pub fn observe_usage(&mut self, usage: &Value) -> Result<()> {
        self.model_usage_known = model_calls(usage).is_some();
        let mut changed = false;
        if let Some(calls) = model_calls(usage)
            && calls > self.state.model_calls
        {
            self.state.model_calls = calls;
            changed = true;
        }
        let tokens = token_counts(usage);
        self.token_usage_known =
            tokens.is_some() && model_calls(usage) == Some(self.state.model_calls);
        if let Some(tokens) = tokens {
            for (saved, observed) in self.state.tokens.iter_mut().zip(tokens) {
                if observed > *saved {
                    *saved = observed;
                    changed = true;
                }
            }
        }
        if changed {
            self.save()?;
        }
        Ok(())
    }

    /// Evaluate after synchronous post-invocation usage refresh. A generation may
    /// cross the limit; the hook must stop before the next generation begins.
    pub fn token_stop_reason(&self) -> Option<String> {
        let mut enabled = false;
        for (index, key) in TOKEN_LIMITS.iter().enumerate() {
            if let Some(limit) = self.limit(key) {
                enabled = true;
                if self.state.tokens[index].saturating_sub(self.state.token_baseline[index])
                    >= limit
                {
                    return Some(format!(
                        "{key} budget exhausted at model invocation boundary"
                    ));
                }
            }
        }
        if enabled
            && !self.token_usage_known
            && !(self.model_usage_known && self.state.model_calls == 0)
        {
            return Some("token budget cannot continue with incomplete native usage".into());
        }
        None
    }

    /// Call before allowing a PreToolUse request. Use native trajectory + call ID
    /// as `id`; repeated delivery of the same request does not spend twice.
    /// Persist before authorization so a crash cannot restore spent allowance.
    pub fn claim_tool(&mut self, id: &str) -> Result<()> {
        ensure!(!id.is_empty(), "native tool call has no budget identity");
        if self.state.tool_ids.contains(id) {
            return Ok(());
        }
        if let Some(limit) = self.limit("maxToolCalls") {
            ensure!(
                self.state
                    .tool_calls
                    .saturating_sub(self.state.tool_baseline)
                    < limit,
                "maxToolCalls budget exhausted"
            );
        }
        self.state.tool_calls = self
            .state
            .tool_calls
            .checked_add(1)
            .context("tool counter overflow")?;
        self.state.tool_ids.insert(id.into());
        if let Err(error) = self.save() {
            self.state.tool_ids.remove(id);
            self.state.tool_calls -= 1;
            return Err(error);
        }
        Ok(())
    }

    fn limit(&self, key: &str) -> Option<u64> {
        self.state.config.as_ref().and_then(|v| v[key].as_u64())
    }

    fn save(&self) -> Result<()> {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new_in(self.path.parent().unwrap())?;
        file.write_all(&serde_json::to_vec(&self.state)?)?;
        file.as_file().sync_all()?;
        file.persist(&self.path).map_err(|e| e.error)?;
        Ok(())
    }
}

fn model_calls(usage: &Value) -> Option<u64> {
    usage["modelCallCount"]
        .as_u64()
        .or_else(|| usage["total"]["modelCallCount"].as_u64())
}

fn token_counts(usage: &Value) -> Option<[u64; 3]> {
    if usage["incomplete"] == true {
        return None;
    }
    let count = |key: &str| {
        let value = &usage["total"][key];
        value
            .as_u64()
            .or_else(|| value.as_str()?.parse::<u64>().ok())
    };
    Some([
        count("promptTokenCount")?,
        count("candidatesTokenCount")?.saturating_add(count("thoughtsTokenCount")?),
        count("totalTokenCount")?,
    ])
}

/// Token limits stop at model invocation boundaries, not within a generation.
pub fn validate(config: Option<&Value>) -> Result<()> {
    let Some(config) = config.filter(|v| !v.is_null()) else {
        return Ok(());
    };
    for (key, value) in config.as_object().context("budget must be an object")? {
        match key.as_str() {
            "scope" => ensure!(
                matches!(
                    value.as_str(),
                    Some("BUDGET_SCOPE_FORWARD_LOOKING" | "BUDGET_SCOPE_LIFETIME")
                ),
                "invalid budget scope"
            ),
            "maxModelCalls" | "maxToolCalls" => ensure!(
                value
                    .as_u64()
                    .is_some_and(|n| n > 0 && n <= i32::MAX as u64),
                "invalid {key} budget"
            ),
            "maxInputTokens" | "maxOutputTokens" | "maxTotalTokens" => ensure!(
                value
                    .as_u64()
                    .is_some_and(|n| n > 0 && n <= i64::MAX as u64),
                "invalid {key} budget"
            ),
            _ => bail!("unsupported budget field: {key}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn lifetime_reopen_and_duplicates_preserve_limits() {
        let dir = tempfile::tempdir().unwrap();
        let config = json!({"maxModelCalls":3,"maxToolCalls":2});
        let mut b =
            Budget::open(dir.path(), Some(&config), &json!({"modelCallCount":1}), 0).unwrap();
        assert_eq!(b.prepare_turn().unwrap(), Some(2));
        b.claim_tool("a").unwrap();
        b.claim_tool("a").unwrap();
        b.claim_tool("b").unwrap();
        assert!(b.claim_tool("c").is_err());
        b.observe_usage(&json!({"modelCallCount":3})).unwrap();
        let mut b =
            Budget::open(dir.path(), Some(&config), &json!({"modelCallCount":2}), 0).unwrap();
        assert!(b.prepare_turn().is_err());
        assert!(b.claim_tool("c").is_err());
        b.claim_tool("a").unwrap();
    }
    #[test]
    fn forward_baseline_survives_reload_and_changes_only_with_config() {
        let dir = tempfile::tempdir().unwrap();
        let config =
            json!({"scope":"BUDGET_SCOPE_FORWARD_LOOKING","maxModelCalls":2,"maxToolCalls":2});
        let mut b =
            Budget::open(dir.path(), Some(&config), &json!({"modelCallCount":10}), 5).unwrap();
        assert_eq!(b.prepare_turn().unwrap(), Some(2));
        b.claim_tool("a").unwrap();
        b.observe_usage(&json!({"modelCallCount":11})).unwrap();
        let mut b =
            Budget::open(dir.path(), Some(&config), &json!({"modelCallCount":11}), 6).unwrap();
        assert_eq!(b.prepare_turn().unwrap(), Some(1));
        b.claim_tool("b").unwrap();
        assert!(b.prepare_turn().is_err());
        assert!(b.claim_tool("c").is_err());
        Budget::open(dir.path(), None, &json!({"modelCallCount":11}), 6).unwrap();
        let b = Budget::open(dir.path(), Some(&config), &json!({"modelCallCount":11}), 6).unwrap();
        assert_eq!(b.prepare_turn().unwrap(), Some(2));
    }
    #[test]
    fn unknown_usage_and_invalid_limits_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let config = json!({"maxModelCalls":2});
        let b = Budget::open(dir.path(), Some(&config), &Value::Null, 0).unwrap();
        assert!(b.prepare_turn().is_err());
        assert!(validate(Some(&json!({"maxTotalTokens":100}))).is_ok());
        assert!(validate(Some(&json!({"maxTotalTokens":0}))).is_err());
        assert!(validate(Some(&json!({"maxToolCalls":0}))).is_err());
        std::fs::write(dir.path().join("budget.json"), "bad").unwrap();
        assert!(Budget::open(dir.path(), None, &Value::Null, 0).is_err());
    }

    fn usage(calls: u64, input: u64, response: u64, thinking: u64) -> Value {
        json!({"modelCallCount":calls,"total":{"promptTokenCount":input,
            "candidatesTokenCount":response,"thoughtsTokenCount":thinking,
            "totalTokenCount":input+response+thinking}})
    }

    #[test]
    fn token_limits_stop_after_generation_and_persist_across_reopen() {
        for (key, limit) in [
            ("maxInputTokens", 10),
            ("maxOutputTokens", 5),
            ("maxTotalTokens", 15),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let config = json!({key:limit});
            let mut b =
                Budget::open(dir.path(), Some(&config), &json!({"modelCallCount":0}), 0).unwrap();
            assert!(b.prepare_turn().is_ok());
            b.observe_usage(&usage(1, 11, 2, 4)).unwrap();
            assert!(b.token_stop_reason().unwrap().starts_with(key));
            assert!(b.prepare_turn().is_err());
            // A stale snapshot must not restore allowance already spent.
            let b = Budget::open(dir.path(), Some(&config), &usage(1, 1, 1, 0), 0).unwrap();
            assert!(b.token_stop_reason().unwrap().starts_with(key));
        }
    }

    #[test]
    fn forward_token_baseline_survives_reopen_and_resets_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let config = json!({"maxTotalTokens":20,"scope":"BUDGET_SCOPE_FORWARD_LOOKING"});
        let mut b = Budget::open(dir.path(), Some(&config), &usage(10, 100, 20, 30), 0).unwrap();
        assert!(b.prepare_turn().is_ok());
        b.observe_usage(&usage(11, 110, 22, 33)).unwrap();
        let mut b = Budget::open(dir.path(), Some(&config), &usage(11, 110, 22, 33), 0).unwrap();
        assert!(b.token_stop_reason().is_none());
        b.observe_usage(&usage(12, 115, 22, 33)).unwrap();
        assert!(b.token_stop_reason().is_some());
        let changed = json!({"maxTotalTokens":21,"scope":"BUDGET_SCOPE_FORWARD_LOOKING"});
        let b = Budget::open(dir.path(), Some(&changed), &usage(12, 115, 22, 33), 0).unwrap();
        assert!(b.token_stop_reason().is_none());
    }

    #[test]
    fn unknown_token_usage_stops_except_confirmed_virgin_trajectory() {
        let dir = tempfile::tempdir().unwrap();
        let config = json!({"maxTotalTokens":100});
        let mut b = Budget::open(dir.path(), Some(&config), &Value::Null, 0).unwrap();
        assert!(b.token_stop_reason().is_some());
        b.observe_usage(&json!({"modelCallCount":0})).unwrap();
        assert!(b.token_stop_reason().is_none());
        b.observe_usage(&json!({"modelCallCount":1})).unwrap();
        assert!(b.token_stop_reason().is_some());
        b.observe_usage(&usage(1, 10, 5, 2)).unwrap();
        assert!(b.token_stop_reason().is_none());
        let forward = json!({"maxTotalTokens":100,"scope":"BUDGET_SCOPE_FORWARD_LOOKING"});
        assert!(Budget::open(dir.path(), Some(&forward), &json!({"modelCallCount":1}), 0).is_err());
    }

    #[test]
    fn legacy_state_without_token_fields_loads_and_accounts_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("budget.json"), r#"{"config":null,"model_calls":1,"tool_calls":0,"model_baseline":0,"tool_baseline":0,"tool_ids":[]}"#).unwrap();
        let config = json!({"maxInputTokens":10});
        let b = Budget::open(dir.path(), Some(&config), &usage(1, 10, 2, 1), 0).unwrap();
        assert!(b.token_stop_reason().unwrap().starts_with("maxInputTokens"));
    }
}
