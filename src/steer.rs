use crate::*;

pub fn native_text(parts: &[Value]) -> String {
    parts.iter().filter_map(|p| p["text"].as_str()).collect()
}

impl Server {
    pub async fn steer(&mut self, p: &Value) -> Result<Value> {
        let id = text(p, "threadId")?;
        let mut thread = self.store.load(id)?;
        let turn_id = text(p, "expectedTurnId")?;
        let key = text(p, "clientUserMessageId")?;
        ensure!(key.len() <= 256, "clientUserMessageId too long");
        if let Some(receipt) = thread.steer_receipts.get(key) {
            ensure!(
                receipt["turnId"] == turn_id
                    && receipt["input"] == p["input"]
                    && receipt["additionalContext"] == p["additionalContext"],
                "clientUserMessageId reused with different steering input"
            );
            return Ok(json!({"turnId":turn_id,"deliveryStatus":receipt["state"]}));
        }
        ensure!(
            !thread.receipts.contains_key(key),
            "clientUserMessageId already used for turn/start"
        );
        ensure!(thread.status == "running", "turn is not running");
        let turn = thread.turn.as_ref().context("missing active turn")?;
        ensure!(turn["id"] == turn_id, "expectedTurnId mismatch");
        ensure!(
            turn["interruptRequested"] != true,
            "turn is being interrupted"
        );
        ensure!(
            self.connections.contains_key(id),
            "native connection unavailable"
        );
        let mut parts = input::context(p)?;
        parts.extend(input::parts(&self.store, &thread, &p["input"])?);
        thread.revision += 1;
        thread.steer_receipts.insert(
            key.into(),
            json!({"turnId":turn_id,"input":p["input"],
            "additionalContext":p["additionalContext"],"nativeText":native_text(&parts),
            "sequence":thread.revision,"state":"queued"}),
        );
        // Persist before dispatch. A lost response never licenses a second send.
        self.store.save(&thread)?;
        if let Err(error) = self.send(id, json!({"userInput":{"parts":parts}})).await {
            thread.steer_receipts.get_mut(key).unwrap()["state"] = json!("unknown");
            self.store.save(&thread)?;
            return Err(error);
        }
        Ok(json!({"turnId":turn_id,"deliveryStatus":"queued"}))
    }
}

// The native stream has no client message ID. Correlate its ordered root user
// steps with the initial input and durable FIFO, never with child trajectories.
pub fn user_item(thread: &mut Thread, step: &Value, item: &mut Value) {
    if step["trajectoryId"] != thread.id || step["state"] != "STATE_DONE" {
        return;
    }
    let Some(turn) = &mut thread.turn else {
        return;
    };
    let item_id = item["id"].clone();
    let is_initial = turn["nativeUserItemId"] == item_id
        || (turn["nativeUserItemId"].is_null()
            && (turn["nativeInputText"].is_null() || turn["nativeInputText"] == step["text"]));
    if is_initial {
        turn["nativeUserItemId"] = item_id;
        if turn["input"].is_array() {
            item["content"] = turn["input"].clone();
        }
        item["clientId"] = turn["clientUserMessageId"].clone();
        return;
    }
    if turn["nativeUserItemId"].is_null() {
        return;
    }
    let turn_id = turn["id"].clone();
    let duplicate = thread
        .steer_receipts
        .iter()
        .find(|(_, r)| r["turnId"] == turn_id && r["nativeItemId"] == item_id)
        .map(|(k, _)| k.clone());
    let key = duplicate.or_else(|| {
        thread
            .steer_receipts
            .iter()
            .filter(|(_, r)| r["turnId"] == turn_id && r["state"] != "consumed")
            .min_by_key(|(_, r)| r["sequence"].as_u64().unwrap_or(0))
            .filter(|(_, r)| r["nativeText"] == step["text"])
            .map(|(k, _)| k.clone())
    });
    if let Some(key) = key {
        let receipt = thread.steer_receipts.get_mut(&key).unwrap();
        receipt["state"] = json!("consumed");
        receipt["nativeItemId"] = item_id;
        item["content"] = receipt["input"].clone();
        item["clientId"] = json!(key);
    }
}

pub fn mark_unknown(thread: &mut Thread) {
    for receipt in thread.steer_receipts.values_mut() {
        if receipt["state"] == "queued" {
            receipt["state"] = json!("unknown");
        }
    }
}
