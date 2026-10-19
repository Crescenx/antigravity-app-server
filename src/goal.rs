use crate::*;

pub struct Action {
    pub thread_id: String,
    pub params: Value,
    pub reply: oneshot::Sender<Result<Value>>,
}

pub fn tool() -> Value {
    json!({"name":"goal_update","description":"Explicitly report the current goal complete or blocked, with evidence. Complete only when all authorized work is done; do not infer completion from a budget or idle state.",
        "inputSchema":{"type":"object","properties":{"status":{"type":"string","enum":["complete","blocked"]},"reason":{"type":"string"}},"required":["status","reason"],"additionalProperties":false}})
}

pub fn mutate(thread: &mut Thread, params: &Value, clear: bool) -> Result<()> {
    if clear {
        thread.goal = None;
        return Ok(());
    }
    let objective = params.get("objective").filter(|v| !v.is_null());
    if let Some(value) = objective {
        ensure!(
            value.as_str().is_some_and(|s| !s.trim().is_empty()),
            "objective must be nonempty"
        );
    }
    if let Some(budget) = params.get("tokenBudget").filter(|v| !v.is_null()) {
        ensure!(
            budget
                .as_u64()
                .is_some_and(|n| n > 0 && n <= 9_007_199_254_740_991),
            "invalid token budget"
        );
    }
    if let Some(status) = params.get("status").filter(|v| !v.is_null()) {
        ensure!(
            ["active", "paused", "complete", "blocked"].contains(&status.as_str().unwrap_or("")),
            "unsupported goal status"
        );
    }
    ensure!(
        params.get("limits").is_none_or(Value::is_null),
        "extended goal limits are unsupported"
    );
    ensure!(
        params.get("verification").is_none_or(Value::is_null),
        "registered goal verifiers are unsupported"
    );
    let replacing =
        objective.is_some_and(|o| thread.goal.as_ref().is_none_or(|g| g["objective"] != *o));
    let mut goal = if replacing || thread.goal.is_none() {
        json!({"id":uuid::Uuid::new_v4().to_string(),"threadId":thread.id,"objective":objective.context("objective required")?,
            "status":"active","tokenBudget":null,"tokensUsed":0,"timeUsedSeconds":0,
            "createdAt":now(),"updatedAt":now(),"clockAt":now(),"revision":0})
    } else {
        thread.goal.clone().unwrap()
    };
    for field in ["status", "tokenBudget"] {
        if let Some(value) = params.get(field)
            && (field == "tokenBudget" || !value.is_null())
        {
            goal[field] = value.clone();
        }
    }
    if goal["status"] == "active" {
        ensure!(
            !thread.is_plan(),
            "active goals are unavailable in Plan mode"
        );
        ensure!(
            thread.status != "unknown",
            "reconcile unknown execution before activating goal"
        );
    }
    goal["updatedAt"] = json!(now());
    goal["revision"] = json!(goal["revision"].as_u64().unwrap_or(0) + 1);
    limit(&mut goal);
    thread.goal = Some(goal);
    Ok(())
}

fn number(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}
pub fn charge(thread: &mut Thread, usage: &Value) {
    let old = number(&thread.usage["total"]["totalTokenCount"]).unwrap_or(0);
    let new = number(&usage["total"]["totalTokenCount"]);
    if let Some(goal) = &mut thread.goal
        && thread
            .turn
            .as_ref()
            .is_some_and(|t| t["goalId"] == goal["id"])
    {
        if let Some(new) = new.filter(|n| *n >= old) {
            if let Some(used) = goal["tokensUsed"].as_u64() {
                goal["tokensUsed"] = json!(used.saturating_add(new - old));
            }
        } else {
            goal["tokensUsed"] = Value::Null;
        }
        limit(goal);
    }
}
fn limit(goal: &mut Value) {
    if goal["status"] == "active"
        && let Some(budget) = goal["tokenBudget"].as_u64()
    {
        match goal["tokensUsed"].as_u64() {
            Some(used) if used >= budget => goal["status"] = json!("budgetLimited"),
            None => {
                goal["status"] = json!("blocked");
                goal["reason"] = json!("Native usage unknown; automatic continuation stopped");
            }
            _ => {}
        }
    }
}
pub fn clock(thread: &mut Thread, timestamp: u64) -> bool {
    if thread.status != "running" {
        return false;
    }
    let Some(goal) = &mut thread.goal else {
        return false;
    };
    let Some(turn) = &thread.turn else {
        return false;
    };
    if turn["goalId"] != goal["id"] || turn["status"] != "inProgress" {
        return false;
    }
    let from = goal["clockAt"]
        .as_u64()
        .unwrap_or(timestamp)
        .max(turn["startedAt"].as_u64().unwrap_or(timestamp));
    if timestamp <= from {
        return false;
    }
    goal["timeUsedSeconds"] = json!(
        goal["timeUsedSeconds"]
            .as_u64()
            .unwrap_or(0)
            .saturating_add(timestamp - from)
    );
    goal["clockAt"] = json!(timestamp);
    goal["updatedAt"] = json!(timestamp);
    true
}
pub fn notify(output: &Output, thread: &Thread) {
    crate::notify(
        output,
        if thread.goal.is_some() {
            "thread/goal/updated"
        } else {
            "thread/goal/cleared"
        },
        json!({"threadId":thread.id,"goal":thread.goal,"turnId":null}),
    );
}

impl Server {
    pub fn goal_action(&mut self, id: &str, params: &Value) -> Result<Value> {
        let mut thread = self.store.load(id)?;
        ensure!(
            !thread
                .turn
                .as_ref()
                .is_some_and(|t| t["interruptRequested"] == true),
            "turn interrupted"
        );
        ensure!(!thread.is_plan(), "goal execution is disabled in Plan mode");
        let goal = thread.goal.as_ref().context("no current goal")?;
        ensure!(goal["status"] == "active", "goal is not active");
        ensure!(
            thread.status == "running"
                && thread
                    .turn
                    .as_ref()
                    .is_some_and(|t| t["goalId"] == goal["id"]),
            "stale goal execution"
        );
        ensure!(
            ["complete", "blocked"].contains(&text(params, "status")?),
            "invalid agent goal status"
        );
        let reason = text(params, "reason")?.to_owned();
        let mut update = params.clone();
        update["threadId"] = json!(id);
        mutate(&mut thread, &update, false)?;
        let goal = thread.goal.as_mut().unwrap();
        goal["reason"] = json!(reason);
        goal["completionSource"] = json!("agent");
        self.store.save(&thread)?;
        notify(&self.output, &thread);
        Ok(json!({"content":[{"type":"text","text":serde_json::to_string(&thread.goal)?}]}))
    }
    pub async fn advance_goals(&mut self) -> Result<()> {
        // The server event loop serializes starts, user input and pause acknowledgments.
        for id in self.connections.keys().cloned().collect::<Vec<_>>() {
            let mut thread = self.store.load(&id)?;
            if thread.is_plan() {
                continue;
            }
            if clock(&mut thread, now()) {
                self.store.save(&thread)?;
                notify(&self.output, &thread);
            }
            let Some(goal) = &thread.goal else {
                continue;
            };
            if goal["status"] != "active"
                || thread.status != "idle"
                || !self.connections.contains_key(&thread.id)
            {
                continue;
            }
            let params = json!({"threadId":thread.id,"requestId":format!("goal:{}:{}",goal["id"].as_str().unwrap_or(""),thread.revision),
                "input":[{"type":"text","text":format!("Continue working toward the authorized goal: {}. When the authorized work is complete, call harness_goal goal_update with status complete and concrete evidence; if blocked, report blocked with the reason.",goal["objective"].as_str().unwrap_or(""))}]});
            if let Err(error) = self.request("turn/start", params).await {
                let mut latest = self.store.load(&thread.id)?;
                if let Some(goal) = &mut latest.goal {
                    goal["status"] = json!("blocked");
                    goal["reason"] = json!(error.to_string());
                }
                self.store.save(&latest)?;
                notify(&self.output, &latest);
            }
        }
        Ok(())
    }
}
