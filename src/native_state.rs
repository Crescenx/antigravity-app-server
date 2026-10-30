//! App state streams contain indexed, complete replacements, not text deltas.
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Default)]
pub struct NativeState {
    public_id: String,
    native_id: String,
    steps: BTreeMap<usize, Value>,
    generators: BTreeMap<usize, Value>,
    generator_count: usize,
    executors: BTreeMap<usize, Value>,
    executor_count: usize,
    executor_start: usize,
    current_executor: Value,
    children: BTreeMap<String, BTreeMap<usize, Value>>,
    last_error: Value,
    last_usage: Value,
    turn_start: Option<usize>,
    historical_errors: Vec<Value>,
}

impl NativeState {
    pub fn new(public_id: impl Into<String>) -> Self {
        Self {
            public_id: public_id.into(),
            ..Self::default()
        }
    }

    /// Start a public turn without carrying the native trajectory's last historical error.
    /// Keep this reducer across stream reconnects: initial full pages replace its caches.
    pub fn begin_turn(&mut self, response: &Value) {
        let mut historical = Vec::new();
        if !self.last_error.is_null() {
            historical.push(self.last_error.clone());
        }
        self.snapshot(response);
        if let Some(error) = self.steps.values().rev().find_map(step_error) {
            let error = normalize_error(error);
            if !historical.contains(&error) {
                historical.push(error);
            }
        }
        let trajectory = response.get("trajectory").unwrap_or(response);
        self.turn_start = Some(
            response["numTotalSteps"]
                .as_u64()
                .map(|n| n as usize)
                .unwrap_or_else(|| trajectory["steps"].as_array().map_or(0, Vec::len)),
        );
        self.historical_errors = historical;
        self.last_error = Value::Null;
        self.executor_start = self.executor_count;
        self.current_executor = Value::Null;
    }

    pub fn apply(&mut self, update: &Value) -> Vec<Value> {
        let update = update.get("update").unwrap_or(update);
        let mut events = Vec::new();
        if let Some(id) = update["trajectoryId"].as_str() {
            if self.native_id != id {
                self.steps.clear();
                self.generators.clear();
                self.generator_count = 0;
                self.executors.clear();
                self.executor_count = 0;
                self.executor_start = 0;
                self.current_executor = Value::Null;
                self.children.clear();
                self.last_error = Value::Null;
                self.last_usage = Value::Null;
                self.historical_errors.clear();
                self.turn_start = self.turn_start.map(|_| 0);
            }
            self.native_id = id.into();
        }
        if let Some(trajectory) = update.get("mainTrajectoryUpdate") {
            if let Some(error) = trajectory.get("lastStepError") {
                let error = normalize_error(error);
                if error.is_null() || error.as_object().is_some_and(|o| o.is_empty()) {
                    self.last_error = Value::Null;
                } else if !self.historical_errors.contains(&error) {
                    self.last_error = error;
                }
            }
            if let Some(steps) = trajectory.get("stepsUpdate") {
                for (index, step) in indexed_update(&mut self.steps, steps, "steps") {
                    if self.turn_start.is_some_and(|start| index >= start)
                        && let Some(error) = step_error(&step)
                    {
                        // A new failing step is evidence even when its text matches an old failure.
                        self.last_error = normalize_error(error);
                    }
                    events.push(json!({"stepUpdate": normalize_step(&step, &self.public_id, &self.native_id, index)}));
                }
            }
            if let Some(generators) = trajectory.get("generatorMetadatasUpdate") {
                self.generator_count = count(&generators["totalLength"]) as usize;
                indexed_update(&mut self.generators, generators, "generatorMetadatas");
                let usage = self.usage();
                if usage != self.last_usage {
                    self.last_usage = usage.clone();
                    events.push(json!({"usageUpdate":usage}));
                }
            }
            if let Some(executors) = trajectory.get("executorMetadatasUpdate") {
                self.executor_count = count(&executors["totalLength"]) as usize;
                indexed_update(&mut self.executors, executors, "executorMetadatas");
                self.current_executor = self
                    .executor_count
                    .checked_sub(1)
                    .filter(|index| *index >= self.executor_start)
                    .and_then(|index| self.executors.get(&index))
                    .cloned()
                    .unwrap_or(Value::Null);
            }
        }
        if let Some(executor) = update.get("executorMetadata")
            && executor["lastStepIdx"]
                .as_u64()
                .is_some_and(|index| self.turn_start.is_some_and(|start| index >= start as u64))
        {
            self.current_executor = executor.clone();
        }
        if let Some(children) = update["subtrajectoryUpdates"].as_object() {
            for (id, child) in children {
                if let Some(steps) = child.get("stepsUpdate") {
                    for (index, step) in
                        indexed_update(self.children.entry(id.clone()).or_default(), steps, "steps")
                    {
                        events.push(json!({"stepUpdate":normalize_step(&step,id,id,index)}));
                    }
                }
            }
        }
        // Keep richer native state available without pretending it is a legacy event.
        events.push(json!({"appStateUpdate":update}));
        if update.get("fullyIdle").is_some() || update.get("status").is_some() {
            let reason = &self.current_executor["terminationReason"];
            let state = if update["fullyIdle"] == true {
                if is_enum(reason, "EXECUTOR_TERMINATION_REASON_USER_CANCELED", 2) {
                    "STATE_CANCELLED"
                } else {
                    "STATE_FULLY_IDLE"
                }
            } else if is_enum(&update["status"], "CASCADE_RUN_STATUS_CANCELING", 3) {
                "STATE_CANCELING"
            } else {
                "STATE_RUNNING"
            };
            let mut error = self.last_error.clone();
            let mut stop_reason = Value::Null;
            if update["fullyIdle"] == true {
                if is_enum(reason, "EXECUTOR_TERMINATION_REASON_NO_TOOL_CALL", 4)
                    || is_enum(reason, "EXECUTOR_TERMINATION_REASON_TERMINAL_STEP_TYPE", 8)
                    || is_enum(reason, "EXECUTOR_TERMINATION_REASON_USER_CANCELED", 2)
                {
                    error = Value::Null;
                } else if is_enum(reason, "EXECUTOR_TERMINATION_REASON_ERROR", 1) && error.is_null()
                {
                    error = json!({"message":self.current_executor["executionError"].as_str().filter(|s|!s.is_empty()).unwrap_or("Native executor failed")});
                } else if is_enum(reason, "EXECUTOR_TERMINATION_REASON_MAX_INVOCATIONS", 3) {
                    stop_reason = json!("MODEL_CALLS_EXCEEDED");
                    error = Value::Null;
                } else if is_enum(
                    reason,
                    "EXECUTOR_TERMINATION_REASON_MAX_TOKEN_BUDGET_EXCEEDED",
                    11,
                ) {
                    stop_reason = json!("TOKEN_BUDGET_EXCEEDED");
                    error = Value::Null;
                }
            }
            events.push(json!({"trajectoryStateUpdate":{
                "trajectoryId":self.public_id,"nativeTrajectoryId":self.native_id,
                "state":state,"error":error,"stopReason":stop_reason,"nativeTerminationReason":reason,
                "nativeStatus":update["status"],"fullyIdle":update["fullyIdle"]
            }}));
        }
        for event in &mut events {
            sanitize_export(event, false);
        }
        events
    }

    /// Seed a newly connected reducer from GetCascadeTrajectory, without a turn-end event.
    pub fn snapshot(&mut self, response: &Value) -> Vec<Value> {
        let trajectory = response.get("trajectory").unwrap_or(response);
        let steps = trajectory["steps"].as_array().cloned().unwrap_or_default();
        let generators = trajectory["generatorMetadata"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let executors = trajectory["executorMetadatas"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        self.apply(&json!({"trajectoryId":trajectory["trajectoryId"],"mainTrajectoryUpdate":{
            "stepsUpdate":{"totalLength":steps.len(),"indices":(0..steps.len()).collect::<Vec<_>>(),"steps":steps},
            "generatorMetadatasUpdate":{"totalLength":generators.len(),"indices":(0..generators.len()).collect::<Vec<_>>(),"generatorMetadatas":generators},
            "executorMetadatasUpdate":{"totalLength":executors.len(),"indices":(0..executors.len()).collect::<Vec<_>>(),"executorMetadatas":executors}
        }}))
    }

    fn usage(&self) -> Value {
        let mut usage = self.cumulative_usage();
        // Never reuse an older invocation's window when the newest model has none.
        if self.generators.len() == self.generator_count
            && let Some(model) = self
                .generators
                .values()
                .rev()
                .find_map(|g| g.get("chatModel").filter(|m| m.is_object()))
        {
            usage["contextWindow"] = model["chatStartMetadata"]["contextWindowMetadata"].clone();
            usage["lastInvocationUsage"] = model["usage"].clone();
        }
        usage
    }

    fn cumulative_usage(&self) -> Value {
        if self.generators.len() != self.generator_count {
            return json!({"total":null,"modelCallCount":null,"source":"appGeneratorMetadata","incomplete":true});
        }
        let mut input = 0u64;
        let mut output = 0u64;
        let mut thinking = 0u64;
        let mut cached = 0u64;
        let mut known = false;
        let mut incomplete_usage = false;
        let mut model_calls = 0u64;
        for generator in self.generators.values() {
            // One record is one logical generation, not one step or retry attempt.
            if generator.get("chatModel").is_some_and(Value::is_object) {
                model_calls += 1;
                incomplete_usage |= !generator["chatModel"]
                    .get("usage")
                    .is_some_and(Value::is_object);
            }
            if let Some(usage) = generator["chatModel"].get("usage") {
                known = true;
                // Match the core's interaction translator: cached reads are
                // part of the prompt, while outputTokens already includes thinking.
                input = input
                    .saturating_add(count(&usage["inputTokens"]))
                    .saturating_add(count(&usage["cacheReadTokens"]));
                output = output.saturating_add(count(&usage["outputTokens"]));
                thinking = thinking.saturating_add(count(&usage["thinkingOutputTokens"]));
                cached = cached.saturating_add(count(&usage["cacheReadTokens"]));
            }
        }
        if !known || incomplete_usage {
            return json!({"total":null,"modelCallCount":model_calls,"modelCallCountScope":"appRootTrajectoryGenerations",
                "source":"appGeneratorMetadata","incomplete":incomplete_usage});
        }
        json!({"total":{"promptTokenCount":input,"candidatesTokenCount":output.saturating_sub(thinking),
            "thoughtsTokenCount":thinking,"cachedContentTokenCount":cached,
            "totalTokenCount":input.saturating_add(output),"modelCallCount":model_calls},
            "modelCallCount":model_calls,"modelCallCountScope":"appRootTrajectoryGenerations","source":"appGeneratorMetadata"})
    }
}

fn step_error(step: &Value) -> Option<&Value> {
    step.get("error")
        .or_else(|| step["errorMessage"].get("error"))
        .filter(|error| error.as_object().is_some_and(|o| !o.is_empty()))
}

fn normalize_error(error: &Value) -> Value {
    let mut result = error.clone();
    if !result.is_object() {
        return result;
    }
    if let Some(message) = ["userErrorMessage", "shortError", "message", "fullError"]
        .iter()
        .filter_map(|key| error[*key].as_str())
        .find(|s| !s.trim().is_empty())
    {
        result["message"] = json!(message);
    }
    result
}

/// Redact exported configuration copies, never the reducer's native state or tool text.
fn sanitize_export(value: &mut Value, in_config: bool) {
    match value {
        Value::Array(values) => {
            for value in values {
                sanitize_export(value, in_config);
            }
        }
        Value::Object(fields) => {
            for (key, value) in fields {
                let key = key.replace('_', "").to_ascii_lowercase();
                let in_config = in_config
                    || key.ends_with("config")
                    || key.ends_with("configuration")
                    || matches!(key.as_str(), "mcpservers" | "mcpserverspec" | "credentials");
                if in_config
                    && (matches!(
                        key.as_str(),
                        "env"
                            | "environment"
                            | "headers"
                            | "httpheaders"
                            | "authorization"
                            | "auth"
                            | "oauth"
                            | "credentials"
                            | "password"
                            | "apikey"
                            | "secret"
                            | "token"
                    ) || key.ends_with("token")
                        || key.ends_with("secret")
                        || key.ends_with("password")
                        || (key == "url" && value.as_str().is_some_and(|s| s.contains("/budget/"))))
                {
                    *value = json!("[REDACTED]");
                } else {
                    sanitize_export(value, in_config);
                }
            }
        }
        _ => {}
    }
}

fn count(value: &Value) -> u64 {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}

fn is_enum(value: &Value, name: &str, number: u64) -> bool {
    value.as_str() == Some(name) || value.as_u64() == Some(number)
}

fn indexed_update(
    previous: &mut BTreeMap<usize, Value>,
    update: &Value,
    field: &str,
) -> Vec<(usize, Value)> {
    let total = count(&update["totalLength"]) as usize;
    previous.retain(|index, _| *index < total);
    let mut changed = Vec::new();
    if let (Some(indices), Some(values)) = (update["indices"].as_array(), update[field].as_array())
    {
        for (index, value) in indices.iter().zip(values) {
            let Some(index) = index.as_u64().map(|i| i as usize).filter(|i| *i < total) else {
                continue;
            };
            if previous.get(&index) != Some(value) {
                previous.insert(index, value.clone());
                changed.push((index, value.clone()));
            }
        }
    }
    changed
}

fn local_path(value: &Value) -> Value {
    value
        .as_str()
        .and_then(|s| reqwest::Url::parse(s).ok())
        .and_then(|u| u.to_file_path().ok())
        .map(|p| json!(p))
        .unwrap_or_else(|| value.clone())
}

pub fn normalize_step(step: &Value, public_id: &str, native_id: &str, index: usize) -> Value {
    // Protobuf JSON expands Any payloads; tools may be nested inside generic results.
    if step["generic"]["result"]["payload"]["@type"]
        .as_str()
        .is_some_and(|s| s.ends_with("/gemini_coder.Step"))
    {
        let mut inner = step["generic"]["result"]["payload"].clone();
        for key in ["status", "metadata", "requestedInteraction", "error"] {
            if let Some(value) = step.get(key) {
                inner[key] = value.clone();
            }
        }
        let mut result = normalize_step(&inner, public_id, native_id, index);
        result["rawNativeStep"] = step.clone();
        sanitize_export(&mut result, false);
        return result;
    }
    let mut result = step.clone();
    if !result.is_object() {
        result = json!({});
    }
    result["rawNativeStep"] = step.clone();
    result["trajectoryId"] = json!(public_id);
    result["nativeTrajectoryId"] = json!(native_id);
    result["stepIndex"] = json!(index);
    let status = &step["status"];
    result["state"] = json!(if is_enum(status, "CORTEX_STEP_STATUS_DONE", 3) {
        "STATE_DONE"
    } else if is_enum(status, "CORTEX_STEP_STATUS_ERROR", 7) {
        "STATE_ERROR"
    } else if is_enum(status, "CORTEX_STEP_STATUS_WAITING", 9) {
        "STATE_WAITING_FOR_USER"
    } else if is_enum(status, "CORTEX_STEP_STATUS_CANCELED", 6)
        || is_enum(status, "CORTEX_STEP_STATUS_INTERRUPTED", 12)
    {
        "STATE_CANCELLED"
    } else {
        "STATE_RUNNING"
    });
    if let Some(user) = step.get("userInput") {
        result["source"] = json!("SOURCE_USER");
        result["text"] = json!(
            user["items"]
                .as_array()
                .map(|items| items
                    .iter()
                    .filter_map(|i| i["text"].as_str())
                    .collect::<String>())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| user["userResponse"]
                    .as_str()
                    .or(user["query"].as_str())
                    .unwrap_or("")
                    .into())
        );
        result["nativeMessageTags"] = user["tags"].clone();
    } else if let Some(planner) = step.get("plannerResponse") {
        result["source"] = json!("SOURCE_MODEL");
        result["target"] = json!("TARGET_USER");
        result["text"] = planner
            .get("modifiedResponse")
            .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
            .or(planner.get("response"))
            .cloned()
            .unwrap_or(json!(""));
        result["thinking"] = planner["thinking"].clone();
    } else if let Some(finish) = step.get("finish") {
        result["source"] = json!("SOURCE_MODEL");
        result["target"] = json!("TARGET_USER");
        result["text"] = finish.get("outputString").cloned().unwrap_or(json!(""));
    } else if let Some(notify) = step.get("notifyUser") {
        result["source"] = json!("SOURCE_MODEL");
        result["target"] = json!("TARGET_USER");
        result["text"] = notify
            .get("notificationContent")
            .cloned()
            .unwrap_or(json!(""));
    }
    if let Some(edit) = step
        .get("writeToFile")
        .or(step.get("fileChange"))
        .or(step["codeAction"]["actionResult"].get("edit"))
    {
        let lines = edit["diff"]["unifiedDiff"]["lines"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|line| {
                let action = if is_enum(&line["type"], "UNIFIED_DIFF_LINE_TYPE_INSERT", 1) {
                    "LINE_ACTION_INSERT"
                } else if is_enum(&line["type"], "UNIFIED_DIFF_LINE_TYPE_DELETE", 2) {
                    "LINE_ACTION_DELETE"
                } else {
                    "LINE_ACTION_UNCHANGED"
                };
                json!({"text":line["text"],"action":action})
            })
            .collect::<Vec<_>>();
        result["editFile"] = json!({"filePath":local_path(edit.get("targetFileUri").or(edit.get("absoluteUri")).unwrap_or(&edit["absolutePathUri"])),"fileCreated":edit.get("fileCreated").unwrap_or(&edit["createFile"]),"diffBlock":[{"lines":lines}]});
    }
    if let Some(read) = step.get("viewFile") {
        result["viewFile"]["filePath"] = local_path(&read["absolutePathUri"]);
        result["viewFile"]["startLine"] = json!(read["startLine"].as_u64().unwrap_or(0));
        result["text"] = read.get("content").cloned().unwrap_or(json!(""));
    }
    if let Some(command) = step.get("runCommand") {
        result["runCommand"]["workingDir"] = command["cwd"].clone();
        result["runCommand"]["combinedOutput"] = command["combinedOutput"]["full"]
            .as_str()
            .or(command["combinedOutput"]["truncated"].as_str())
            .map(|s| json!(s))
            .unwrap_or_else(|| {
                json!(format!(
                    "{}{}",
                    command["stdout"].as_str().unwrap_or(""),
                    command["stderr"].as_str().unwrap_or("")
                ))
            });
    }
    if let Some(error) = step.get("error") {
        result["error"] = normalize_error(error);
    }
    sanitize_export(&mut result, false);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_file_payloads_preserve_paths_content_and_diffs() {
        let read = normalize_step(
            &json!({"status":3,"viewFile":{"absolutePathUri":"file:///tmp/demo%20file.txt","content":"FIRST","endLine":0}}),
            "root",
            "native",
            1,
        );
        let item = crate::compat::tool_item(&read, "read").unwrap();
        assert_eq!(item["result"]["path"], "/tmp/demo file.txt");
        assert_eq!(item["result"]["startLine"], 1);
        assert_eq!(item["result"]["summary"], "FIRST");
        let write = normalize_step(
            &json!({"status":3,"writeToFile":{"targetFileUri":"file:///tmp/demo%20file.txt","fileCreated":true,"diff":{"unifiedDiff":{"lines":[{"text":"SECOND","type":"UNIFIED_DIFF_LINE_TYPE_INSERT"}]}}}}),
            "root",
            "native",
            2,
        );
        let item = crate::compat::tool_item(&write, "write").unwrap();
        assert_eq!(item["changes"][0]["path"], "/tmp/demo file.txt");
        assert_eq!(item["changes"][0]["kind"]["type"], "add");
        assert_eq!(item["changes"][0]["diff"], "+SECOND\n");
        let action = normalize_step(
            &json!({"status":3,"codeAction":{"actionResult":{"edit":{"absoluteUri":"file:///tmp/native.txt","createFile":true,"diff":{"unifiedDiff":{"lines":[{"text":"FIRST","type":1}]}}}}}}),
            "root",
            "native",
            3,
        );
        let item = crate::compat::tool_item(&action, "action").unwrap();
        assert_eq!(item["changes"][0]["path"], "/tmp/native.txt");
        assert_eq!(item["changes"][0]["diff"], "+FIRST\n");
    }

    #[test]
    fn sparse_updates_replace_deduplicate_and_truncate() {
        let mut state = NativeState::new("public");
        let initial = json!({"trajectoryId":"native","mainTrajectoryUpdate":{"stepsUpdate":{"totalLength":3,"indices":[2],"steps":[{"status":"CORTEX_STEP_STATUS_GENERATING","plannerResponse":{"response":"hi"},"error":{"old":true}}]}}});
        assert_eq!(
            state.apply(&initial)[0]["stepUpdate"]["trajectoryId"],
            "public"
        );
        assert!(
            !state
                .apply(&initial)
                .iter()
                .any(|e| e.get("stepUpdate").is_some())
        );
        let changed = json!({"mainTrajectoryUpdate":{"stepsUpdate":{"totalLength":3,"indices":[2],"steps":[{"status":3,"plannerResponse":{"response":"hi there"}}]}}});
        let events = state.apply(&changed);
        assert_eq!(events[0]["stepUpdate"]["text"], "hi there");
        assert!(events[0]["stepUpdate"].get("error").is_none());
        state.apply(&json!({"mainTrajectoryUpdate":{"stepsUpdate":{"totalLength":1}}}));
        assert!(state.steps.is_empty());
    }

    #[test]
    fn usage_counts_generations_once_and_unknown_is_not_zero() {
        let mut state = NativeState::new("public");
        assert!(state.usage()["total"].is_null());
        let update = json!({"mainTrajectoryUpdate":{"generatorMetadatasUpdate":{"totalLength":1,"indices":[0],"generatorMetadatas":[{"stepIndices":[0,1,2],"chatModel":{"usage":{"inputTokens":"10","outputTokens":"7","thinkingOutputTokens":"3"}}}]}}});
        state.apply(&update);
        state.apply(&update);
        assert_eq!(state.usage()["total"]["totalTokenCount"], 17);
    }

    #[test]
    fn context_tracks_latest_invocation_before_usage_and_never_reuses_an_old_window() {
        let mut state = NativeState::new("public");
        let model = json!({"chatModel":{"chatStartMetadata":{"contextWindowMetadata":{
            "estimatedTokensUsed":18792,"maxContextTokens":840000,
            "tokenBreakdown":{"totalTokens":18792}
        }}}});
        let update = json!({"mainTrajectoryUpdate":{"generatorMetadatasUpdate":{
            "totalLength":1,"indices":[0],"generatorMetadatas":[model]
        }}});
        let events = state.apply(&update);
        let usage = &events[0]["usageUpdate"];
        assert!(usage["total"].is_null());
        assert_eq!(usage["contextWindow"]["estimatedTokensUsed"], 18792);
        assert!(
            state
                .apply(&update)
                .iter()
                .all(|event| event.get("usageUpdate").is_none())
        );

        let newer = json!({"chatModel":{"usage":{"inputTokens":"549","cacheReadTokens":"6481",
            "outputTokens":"116","thinkingOutputTokens":"100"},
            "chatStartMetadata":{"contextWindowMetadata":{"estimatedTokensUsed":7000,"maxContextTokens":256000}}}});
        state.apply(&json!({"mainTrajectoryUpdate":{"generatorMetadatasUpdate":{
            "totalLength":2,"indices":[1],"generatorMetadatas":[newer]
        }}}));
        // Replaying an earlier generator must not change the latest context.
        state.apply(&json!({"mainTrajectoryUpdate":{"generatorMetadatasUpdate":{
            "totalLength":2,"indices":[0],"generatorMetadatas":[model]
        }}}));
        let usage = state.usage();
        assert_eq!(usage["contextWindow"]["maxContextTokens"], 256000);
        assert_eq!(usage["lastInvocationUsage"]["inputTokens"], "549");
        assert!(usage["total"].is_null()); // The first generation has no usage yet.

        state.apply(&json!({"mainTrajectoryUpdate":{"generatorMetadatasUpdate":{
            "totalLength":3,"indices":[2],"generatorMetadatas":[{"chatModel":{}}]
        }}}));
        assert!(state.usage()["contextWindow"].is_null());
        state.apply(&json!({"mainTrajectoryUpdate":{"generatorMetadatasUpdate":{
            "totalLength":5,"indices":[4],"generatorMetadatas":[model]
        }}}));
        assert!(state.usage()["contextWindow"].is_null());
    }

    #[test]
    fn usage_includes_cached_prompt_and_counts_thinking_once() {
        let mut state = NativeState::new("public");
        state.apply(&json!({"mainTrajectoryUpdate":{"generatorMetadatasUpdate":{
            "totalLength":1,"indices":[0],"generatorMetadatas":[{"chatModel":{"usage":{
                "inputTokens":"549","cacheReadTokens":"6481",
                "outputTokens":"116","responseOutputTokens":"16","thinkingOutputTokens":"100"
            }}}]
        }}}));
        let usage = state.usage();
        assert_eq!(usage["total"]["promptTokenCount"], 7030);
        assert_eq!(usage["total"]["candidatesTokenCount"], 16);
        assert_eq!(usage["total"]["thoughtsTokenCount"], 100);
        assert_eq!(usage["total"]["totalTokenCount"], 7146);
    }

    #[test]
    fn idle_requires_explicit_fully_idle_and_empty_error_clears() {
        let mut state = NativeState::new("public");
        let events = state.apply(&json!({"status":"CASCADE_RUN_STATUS_IDLE"}));
        assert_ne!(
            events.last().unwrap()["trajectoryStateUpdate"]["state"],
            "STATE_FULLY_IDLE"
        );
        state.apply(&json!({"mainTrajectoryUpdate":{"lastStepError":{"shortError":"bad"}}}));
        let events =
            state.apply(&json!({"fullyIdle":true,"mainTrajectoryUpdate":{"lastStepError":{}}}));
        assert!(events.last().unwrap()["trajectoryStateUpdate"]["error"].is_null());
    }

    #[test]
    fn child_steps_keep_ownership_and_any_preserves_approval_address() {
        let mut state = NativeState::new("root-public");
        let events = state.apply(&json!({"subtrajectoryUpdates":{"child-native":{"stepsUpdate":{
            "totalLength":1,"indices":[0],"steps":[{"status":9,"requestedInteraction":{"permission":{"actionDescription":"run"}},
            "generic":{"result":{"payload":{"@type":"type.googleapis.com/gemini_coder.Step","runCommand":{"commandLine":"pwd","cwd":"/tmp","combinedOutput":{"truncated":"/tmp"}}}}}}]
        }}}}));
        let step = &events[0]["stepUpdate"];
        assert_eq!(step["trajectoryId"], "child-native");
        assert_eq!(step["nativeTrajectoryId"], "child-native");
        assert_eq!(step["state"], "STATE_WAITING_FOR_USER");
        assert_eq!(step["runCommand"]["combinedOutput"], "/tmp");
        assert_eq!(
            step["requestedInteraction"]["permission"]["actionDescription"],
            "run"
        );
    }

    #[test]
    fn partial_generator_page_cannot_claim_cumulative_usage() {
        let mut state = NativeState::new("root");
        state.apply(&json!({"mainTrajectoryUpdate":{"generatorMetadatasUpdate":{
            "totalLength":100,"indices":[99],"generatorMetadatas":[{"chatModel":{"usage":{"inputTokens":"5"}}}]
        }}}));
        assert_eq!(state.usage()["incomplete"], true);
        assert!(state.usage()["total"].is_null());
    }

    #[test]
    fn logical_model_calls_exclude_injection_and_repeated_steps_and_retries() {
        let mut state = NativeState::new("root");
        let update = json!({"mainTrajectoryUpdate":{"generatorMetadatasUpdate":{
            "totalLength":3,"indices":[0,1,2],"generatorMetadatas":[
                {"stepIndices":[0,1,2],"chatModel":{"retries":4,"usage":{"inputTokens":"5","outputTokens":"2"}}},
                {"injected":{}},{"chatModel":{}}
            ]
        }}});
        state.apply(&update);
        state.apply(&update);
        assert_eq!(state.usage()["modelCallCount"], 2);
        assert!(state.usage()["total"].is_null());
        state.apply(&json!({"mainTrajectoryUpdate":{"generatorMetadatasUpdate":{
            "totalLength":3,"indices":[2],"generatorMetadatas":[{"chatModel":{"usage":{"inputTokens":"10","outputTokens":"3"}}}]
        }}}));
        assert_eq!(state.usage()["total"]["modelCallCount"], 2);
        assert_eq!(state.usage()["total"]["totalTokenCount"], 20);
    }

    #[test]
    fn credential_redaction_covers_both_exports_but_preserves_native_state() {
        let config = json!({"plannerConfig":{"declarativeMixinConfig":{"providers":[{"url":"http://127.0.0.1/budget/secret-route"}]},"customizationDiscoveryConfig":{"mcp":{"servers":[{
            "serverName":"local","headers":{"Authorization":"secret-header"},"env":{"API_KEY":"secret-env"},
            "oauth":{"clientSecret":"secret-auth"}
        }]}}}});
        let update = json!({"mainTrajectoryUpdate":{"stepsUpdate":{"totalLength":1,"indices":[0],"steps":[{
            "status":3,"userInput":{"items":[{"text":"ordinary user text"}],"userConfig":config,"lastUserConfig":config}
        }]}}});
        let original = update.clone();
        let mut state = NativeState::new("root");
        let events = state.apply(&update);
        let serialized = serde_json::to_string(&events).unwrap();
        for secret in ["secret-header", "secret-env", "secret-auth", "secret-route"] {
            assert!(!serialized.contains(secret));
        }
        assert!(serialized.contains("ordinary user text"));
        assert!(serialized.contains("local"));
        assert_eq!(state.steps[&0]["userInput"]["userConfig"], config);
        assert_eq!(update, original);
        assert!(
            !state
                .apply(&update)
                .iter()
                .any(|e| e.get("stepUpdate").is_some())
        );
    }

    #[test]
    fn trajectory_replacement_resets_child_cache_and_usage_deduplication() {
        let mut state = NativeState::new("root");
        let mut update = json!({"trajectoryId":"a","mainTrajectoryUpdate":{"generatorMetadatasUpdate":{
            "totalLength":1,"indices":[0],"generatorMetadatas":[{"chatModel":{"usage":{"inputTokens":"5"}}}]
        }},"subtrajectoryUpdates":{"child":{"stepsUpdate":{"totalLength":1,"indices":[0],"steps":[{"userInput":{}}]}}}});
        state.apply(&update);
        update["trajectoryId"] = json!("b");
        let events = state.apply(&update);
        assert!(events.iter().any(|e| e.get("usageUpdate").is_some()));
        assert!(
            events
                .iter()
                .any(|e| e["stepUpdate"]["trajectoryId"] == "child")
        );
        let mut reconnected = NativeState::new("root");
        assert_eq!(events, reconnected.apply(&update));
    }

    #[test]
    fn native_errors_have_readable_message_without_losing_details() {
        let mut state = NativeState::new("root");
        let events = state.apply(
            &json!({"fullyIdle":true,"mainTrajectoryUpdate":{"lastStepError":{
                "shortError":"Model unavailable","fullError":"internal stack","errorCode":7
            }}}),
        );
        let error = &events.last().unwrap()["trajectoryStateUpdate"]["error"];
        assert_eq!(error["message"], "Model unavailable");
        assert_eq!(error["errorCode"], 7);
    }

    #[test]
    fn denied_turn_does_not_fail_later_success_or_reconnect_replay() {
        let mut state = NativeState::new("public");
        let denied = json!({"shortError":"User denied sandbox bypass"});
        let failed_step = json!({"status":7,"error":denied,"runCommand":{"commandLine":"denied"}});
        state.begin_turn(&json!({"trajectory":{"trajectoryId":"native","steps":[]}}));
        let events=state.apply(&json!({"trajectoryId":"native","fullyIdle":true,"mainTrajectoryUpdate":{
            "lastStepError":denied,"stepsUpdate":{"totalLength":1,"indices":[0],"steps":[failed_step]}
        }}));
        assert_eq!(
            events.last().unwrap()["trajectoryStateUpdate"]["error"]["message"],
            "User denied sandbox bypass"
        );
        state.begin_turn(&json!({"trajectory":{"trajectoryId":"native","steps":[failed_step]}}));
        let success =
            json!({"status":3,"plannerResponse":{"response":"accepted command finished"}});
        let events = state.apply(&json!({"fullyIdle":true,"mainTrajectoryUpdate":{
            "stepsUpdate":{"totalLength":2,"indices":[1],"steps":[success]}
        }}));
        assert!(events.last().unwrap()["trajectoryStateUpdate"]["error"].is_null());
        // Native reconnect sends its stale trajectory error alongside a complete page.
        let replay = json!({"trajectoryId":"native","fullyIdle":true,"mainTrajectoryUpdate":{
            "lastStepError":denied,"stepsUpdate":{"totalLength":2,"indices":[0,1],"steps":[failed_step,success]}
        }});
        let events = state.apply(&replay);
        assert!(events.last().unwrap()["trajectoryStateUpdate"]["error"].is_null());
        assert!(!events.iter().any(|e| e.get("stepUpdate").is_some()));
    }

    #[test]
    fn new_step_with_same_error_is_not_mistaken_for_historical_replay() {
        let mut state = NativeState::new("public");
        let error = json!({"shortError":"denied"});
        let failed = json!({"status":7,"error":error});
        state.begin_turn(&json!({"trajectory":{"trajectoryId":"native","steps":[failed]}}));
        let events=state.apply(&json!({"fullyIdle":true,"mainTrajectoryUpdate":{
            "lastStepError":error,"stepsUpdate":{"totalLength":2,"indices":[0,1],"steps":[failed,failed]}
        }}));
        assert_eq!(
            events.last().unwrap()["trajectoryStateUpdate"]["error"]["message"],
            "denied"
        );
        // Current-turn failures survive a reconnect replay too.
        let events =
            state.apply(&json!({"fullyIdle":true,"mainTrajectoryUpdate":{"lastStepError":error}}));
        assert_eq!(
            events.last().unwrap()["trajectoryStateUpdate"]["error"]["message"],
            "denied"
        );
    }

    #[test]
    fn distinct_summary_only_failure_in_new_turn_is_preserved() {
        let mut state = NativeState::new("public");
        state.begin_turn(&json!({"trajectory":{"trajectoryId":"native","steps":[{"error":{"shortError":"old"}}]}}));
        let events=state.apply(&json!({"fullyIdle":true,"mainTrajectoryUpdate":{"lastStepError":{"shortError":"new model failure"}}}));
        assert_eq!(
            events.last().unwrap()["trajectoryStateUpdate"]["error"]["message"],
            "new model failure"
        );
    }

    #[test]
    fn recovered_tool_error_follows_current_executor_success() {
        let mut state = NativeState::new("public");
        state.begin_turn(
            &json!({"trajectory":{"trajectoryId":"native","steps":[],"executorMetadatas":[]}}),
        );
        state.apply(&json!({"mainTrajectoryUpdate":{"lastStepError":{"shortError":"goal tool failed"},"stepsUpdate":{
            "totalLength":12,"indices":[11],"steps":[{"status":7,"error":{"shortError":"goal tool failed"}}]
        }}}));
        state.apply(&json!({"mainTrajectoryUpdate":{"stepsUpdate":{"totalLength":13,"indices":[12],"steps":[{"status":3,"plannerResponse":{"response":"finished"}}]},
            "executorMetadatasUpdate":{"totalLength":1,"indices":[0],"executorMetadatas":[{"executionId":"current","lastStepIdx":12,"terminationReason":"EXECUTOR_TERMINATION_REASON_NO_TOOL_CALL"}]}
        }}));
        let events = state.apply(&json!({"fullyIdle":true}));
        assert!(events.last().unwrap()["trajectoryStateUpdate"]["error"].is_null());
        assert_eq!(state.last_error["message"], "goal tool failed");
        // A full reconnect page with the same old tool error does not change the outcome.
        let events=state.apply(&json!({"fullyIdle":true,"mainTrajectoryUpdate":{"lastStepError":{"shortError":"goal tool failed"},
            "executorMetadatasUpdate":{"totalLength":1,"indices":[0],"executorMetadatas":[{"executionId":"current","lastStepIdx":12,"terminationReason":4}]}
        }}));
        assert!(events.last().unwrap()["trajectoryStateUpdate"]["error"].is_null());
    }

    #[test]
    fn prior_executor_success_does_not_hide_new_turn_failure() {
        let mut state = NativeState::new("public");
        let old = json!({"executionId":"old","lastStepIdx":0,"terminationReason":4});
        state.begin_turn(&json!({"trajectory":{"trajectoryId":"native","steps":[{"status":3}],"executorMetadatas":[old]}}));
        let events=state.apply(&json!({"fullyIdle":true,"executorMetadata":old,"mainTrajectoryUpdate":{
            "lastStepError":{"shortError":"current error"},"executorMetadatasUpdate":{"totalLength":1,"indices":[0],"executorMetadatas":[old]}
        }}));
        assert_eq!(
            events.last().unwrap()["trajectoryStateUpdate"]["error"]["message"],
            "current error"
        );
        let events=state.apply(&json!({"fullyIdle":true,"mainTrajectoryUpdate":{"executorMetadatasUpdate":{
            "totalLength":2,"indices":[1],"executorMetadatas":[{"executionId":"current","lastStepIdx":1,"terminationReason":1}]
        }}}));
        assert_eq!(
            events.last().unwrap()["trajectoryStateUpdate"]["error"]["message"],
            "current error"
        );
    }

    #[test]
    fn current_executor_cancel_and_budget_have_distinct_terminal_outcomes() {
        for (reason, expected_state, stop) in [
            (2, "STATE_CANCELLED", Value::Null),
            (3, "STATE_FULLY_IDLE", json!("MODEL_CALLS_EXCEEDED")),
            (8, "STATE_FULLY_IDLE", Value::Null),
        ] {
            let mut state = NativeState::new("public");
            state.begin_turn(&json!({"trajectory":{"trajectoryId":"native","steps":[]}}));
            let events=state.apply(&json!({"fullyIdle":true,"executorMetadata":{"lastStepIdx":0,"terminationReason":reason},"mainTrajectoryUpdate":{"lastStepError":{"shortError":"recovered tool error"}}}));
            let terminal = &events.last().unwrap()["trajectoryStateUpdate"];
            assert_eq!(terminal["state"], expected_state);
            assert_eq!(terminal["stopReason"], stop);
            assert!(terminal["error"].is_null());
        }
        let mut state = NativeState::new("public");
        state.begin_turn(&json!({"trajectory":{"trajectoryId":"native","steps":[]}}));
        let events=state.apply(&json!({"fullyIdle":true,"executorMetadata":{"lastStepIdx":0,"terminationReason":1,"executionError":"model failed"}}));
        assert_eq!(
            events.last().unwrap()["trajectoryStateUpdate"]["error"]["message"],
            "model failed"
        );
    }
}
