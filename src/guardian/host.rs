use crate::*;

pub(crate) struct PendingReview {
    pub thread_id: String,
    revision: u64,
    turn_id: Value,
    target: Value,
    pub job: tokio::task::JoinHandle<()>,
}
impl PendingReview {
    pub(crate) fn matches_tool(&self, thread: &str, call: &str) -> bool {
        self.thread_id == thread && self.target["externalTool"]["id"] == call
    }
}

fn evidence(thread: &Thread, target: &Value) -> Value {
    let mut transcript = Vec::new();
    let mut turns = thread.turns.clone();
    if let Some(turn) = &thread.turn {
        turns.retain(|t| t["id"] != turn["id"]);
        turns.push(turn.clone());
    }
    let mut steering = thread.steer_receipts.values().collect::<Vec<_>>();
    steering.sort_by_key(|s| s["sequence"].as_u64().unwrap_or(0));
    for turn in turns {
        transcript.push(json!({"kind":"user_input","turnId":turn["id"],"content":turn["input"]}));
        transcript.push(json!({"kind":"observations","turnId":turn["id"],"content":turn["items"]}));
        if !turn["injectedContext"].is_null() {
            transcript.push(json!({"kind":"application_context","turnId":turn["id"],"content":turn["injectedContext"]}));
        }
        for s in steering.iter().filter(|s| s["turnId"] == turn["id"]) {
            transcript.push(json!({"kind":"user_input","turnId":s["turnId"],"content":s["input"],"deliveryState":s["state"]}));
        }
    }
    json!({"action":target,"cwd":thread.cwd,"permissions":thread.permissions().ok().map(|p|p.effective()),"collaborationMode":thread.collaboration_mode,"transcript":transcript})
}

impl Server {
    pub(crate) fn cancel_reviews(&mut self, id: &str) {
        let keys = self
            .reviews
            .iter()
            .filter(|(_, r)| r.thread_id == id)
            .map(|(k, _)| k.clone())
            .collect::<Vec<_>>();
        for key in keys {
            if let Some(review) = self.reviews.remove(&key) {
                review.job.abort();
                let result = json!({"error":"Review cancelled; action not approved"});
                if let Err(error) = self.store.event(
                    id,
                    &json!({"guardianReviewCompleted":{"id":key,"result":result,"allowed":false}}),
                ) {
                    eprintln!("cannot record cancelled review: {error}");
                }
                notify(
                    &self.output,
                    "item/autoApprovalReview/completed",
                    json!({"threadId":id,"turnId":review.turn_id,"reviewId":key,"result":result,"allowed":false}),
                );
            }
        }
    }
    pub(crate) fn start_review(&mut self, thread: &Thread, target: Value) -> Result<()> {
        // Identical runtime events must never start another paid review.
        if self
            .reviews
            .values()
            .any(|p| p.thread_id == thread.id && p.target == target)
        {
            return Ok(());
        }
        let key = uuid::Uuid::new_v4().to_string();
        let generation = self
            .connections
            .get(&thread.id)
            .context("thread disconnected")?
            .generation
            .clone();
        let id = thread.id.clone();
        let sender = self.events.clone();
        let config = thread.review_config.clone();
        let evidence = evidence(thread, &target);
        self.store.event(&id, &json!({"guardianReviewStarted":{"id":key,"model":config.model,"provider":config.provider,"revision":thread.revision,"target":target}}))?;
        notify(
            &self.output,
            "item/autoApprovalReview/started",
            json!({"threadId":id,"turnId":thread.turn.as_ref().map(|t|t["id"].clone()),"reviewId":key,"target":target,"model":config.model}),
        );
        let review_key = key.clone();
        let job = tokio::spawn(async move {
            let result = guardian::review(&config, &evidence)
                .await
                .unwrap_or_else(|e| json!({"error":format!("{e:#}")}));
            let _ = sender.send(RuntimeEvent {
                id,
                generation,
                event: Ok(json!({"guardianReviewResult":{"id":review_key,"result":result}})),
            });
        });
        self.reviews.insert(
            key,
            PendingReview {
                thread_id: thread.id.clone(),
                revision: thread.revision,
                turn_id: thread
                    .turn
                    .as_ref()
                    .map(|t| t["id"].clone())
                    .unwrap_or(Value::Null),
                target,
                job,
            },
        );
        Ok(())
    }

    pub(crate) async fn review_result(&mut self, thread: &mut Thread, event: &Value) -> Result<()> {
        let key = text(event, "id")?;
        let Some(pending) = self.reviews.remove(key) else {
            return Ok(());
        };
        ensure!(pending.thread_id == thread.id, "review thread mismatch");
        let mut result = event["result"].clone();
        let fresh = thread.revision == pending.revision
            && thread.status == "running"
            && thread
                .turn
                .as_ref()
                .is_some_and(|t| t["id"] == pending.turn_id && t["interruptRequested"] != true)
            && thread.approval_policy != "never"
            && thread.approvals_reviewer == "auto_review"
            && !thread.is_plan();
        if !fresh {
            result = json!({"error":"Review authorization became stale; action not approved"});
        }
        let allow =
            fresh && result["error"].is_null() && result["assessment"]["outcome"] == "allow";
        self.store.event(&thread.id,&json!({"guardianReviewCompleted":{"id":key,"target":pending.target,"result":result,"allowed":allow}}))?;
        notify(
            &self.output,
            "item/autoApprovalReview/completed",
            json!({"threadId":thread.id,"turnId":pending.turn_id,"reviewId":key,"target":pending.target,"result":result,"allowed":allow}),
        );
        if pending.target.get("nativeCommand").is_some() {
            self.native_command_approval(
                thread,
                &pending.target,
                allow,
                false,
                if allow {
                    ""
                } else {
                    "Automatic approval review did not allow this action"
                },
            )
            .await
        } else if let Some(call) = pending.target.get("externalTool") {
            if let Some(receipt) = thread.tool_receipts.get_mut(text(call, "id")?) {
                receipt["review"] = result.clone();
            }
            if allow {
                self.run_tool(thread, call, true)
            } else {
                self.tool_result(thread,call,&json!({"success":false,"error":format!("Automatic approval review did not allow this action: {}",result["error"].as_str().or(result["assessment"]["rationale"].as_str()).unwrap_or("denied")),"review":result})).await
            }
        } else {
            self.send(&thread.id,json!({"policyDecisionResponse":{"requestId":pending.target["nativeRequest"]["requestId"],"outcome":if allow{"POLICY_EVALUATION_OUTCOME_ALLOW"}else{"POLICY_EVALUATION_OUTCOME_DENY"},"denyReason":if allow{Value::Null}else{json!(format!("Automatic approval review denied: {result}"))}}})).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn older_steering_cannot_be_ordered_after_newer_user_authorization() {
        let (_, mut thread, _) = crate::tests::fixture();
        thread.turns = vec![json!({"id":"old","input":"first","items":[]})];
        thread.turn = Some(json!({"id":"new","input":"latest","items":[]}));
        thread.steer_receipts.insert(
            "s".into(),
            json!({"turnId":"old","sequence":1,"input":"older correction","state":"consumed"}),
        );
        let e = evidence(&thread, &json!({"externalTool":{}}));
        let users = e["transcript"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|x| x["kind"] == "user_input")
            .map(|x| x["content"].clone())
            .collect::<Vec<_>>();
        assert_eq!(
            users,
            vec![json!("first"), json!("older correction"), json!("latest")]
        );
    }

    #[tokio::test]
    async fn native_review_is_bound_to_request_and_denial_is_returned() {
        let (mut server, mut thread, _) = crate::tests::fixture();
        thread.status = "running".into();
        thread.approvals_reviewer = "auto_review".into();
        thread.turn = Some(json!({"id":"t","items":[]}));
        let (commands, mut rx) = mpsc::channel(1);
        server.connections.insert(
            thread.id.clone(),
            Connection {
                pid: None,
                generation: "g".into(),
                commands,
            },
        );
        let receive = tokio::spawn(async move {
            let Some(Command::Send(value, reply)) = rx.recv().await else {
                panic!("missing native response")
            };
            let _ = reply.send(Ok(()));
            value
        });
        server.reviews.insert("r".into(),PendingReview{thread_id:thread.id.clone(),revision:thread.revision,turn_id:json!("t"),target:json!({"nativeRequest":{"requestId":"bound-request","toolArgs":{"toolName":"test"}}}),job:tokio::spawn(async {})});
        server.review_result(&mut thread,&json!({"id":"r","result":{"assessment":{"outcome":"deny","rationale":"unauthorized"}}})).await.unwrap();
        let response = receive.await.unwrap();
        assert_eq!(
            response["policyDecisionResponse"]["requestId"],
            "bound-request"
        );
        assert_eq!(
            response["policyDecisionResponse"]["outcome"],
            "POLICY_EVALUATION_OUTCOME_DENY"
        );
    }
    #[tokio::test]
    async fn only_fresh_allow_executes_and_late_results_are_ignored() {
        for case in [
            "allow",
            "deny",
            "error",
            "steer",
            "plan",
            "never",
            "interrupt",
            "other-turn",
        ] {
            let (mut server, mut thread, _) = crate::tests::fixture();
            thread.status = "running".into();
            thread.approvals_reviewer = "auto_review".into();
            thread.turn =
                Some(json!({"id":"turn-1","items":[],"input":[{"type":"text","text":"Run true"}]}));
            let call = json!({"id":"call-1","name":"run_command","arguments":{"CommandLine":"true","BypassSandbox":true}});
            thread
                .tool_receipts
                .insert("call-1".into(), json!({"call":call,"state":"pending"}));
            let (commands, mut rx) = mpsc::channel(8);
            let ack = tokio::spawn(async move {
                while let Some(Command::Send(_, tx)) = rx.recv().await {
                    let _ = tx.send(Ok(()));
                }
            });
            server.connections.insert(
                thread.id.clone(),
                Connection {
                    pid: None,
                    generation: "g".into(),
                    commands,
                },
            );
            server.reviews.insert(
                "review-1".into(),
                PendingReview {
                    thread_id: thread.id.clone(),
                    revision: thread.revision,
                    turn_id: json!("turn-1"),
                    target: json!({"externalTool":call}),
                    job: tokio::spawn(async {}),
                },
            );
            match case {
                "steer" => thread.revision += 1,
                "plan" => thread.collaboration_mode = "plan".into(),
                "never" => thread.approval_policy = "never".into(),
                "interrupt" => thread.turn.as_mut().unwrap()["interruptRequested"] = json!(true),
                "other-turn" => thread.turn.as_mut().unwrap()["id"] = json!("turn-2"),
                _ => {}
            }
            server.store.save(&thread).unwrap();
            let result = if case == "error" {
                json!({"error":"timeout"})
            } else {
                json!({"assessment":{"outcome":if case == "deny"{"deny"}else{"allow"}}})
            };
            let event = json!({"id":"review-1","result":result});
            server.review_result(&mut thread, &event).await.unwrap();
            assert_eq!(
                server.tool_jobs.len(),
                usize::from(case == "allow"),
                "{case}"
            );
            server.review_result(&mut thread, &event).await.unwrap();
            assert!(server.reviews.is_empty());
            server.abort_tools(&thread.id);
            ack.abort();
        }
    }

    #[tokio::test]
    async fn cancellation_removes_review_and_preserves_legacy_configuration() {
        let (mut server, mut thread, _) = crate::tests::fixture();
        let job = tokio::spawn(std::future::pending());
        let handle = job.abort_handle();
        server.reviews.insert(
            "review".into(),
            PendingReview {
                thread_id: thread.id.clone(),
                revision: 0,
                turn_id: Value::Null,
                target: Value::Null,
                job,
            },
        );
        server.abort_tools(&thread.id);
        tokio::task::yield_now().await;
        assert!(handle.is_finished());
        assert!(server.reviews.is_empty());
        let mut value = serde_json::to_value(&thread).unwrap();
        value.as_object_mut().unwrap().remove("approvalsReviewer");
        value.as_object_mut().unwrap().remove("reviewConfig");
        let old: Thread = serde_json::from_value(value).unwrap();
        assert_eq!(old.approvals_reviewer, "user");
        configure(&mut thread,&json!({"approvalsReviewer":"auto_review","config":{"guardian":{"model":"other-model","provider":"custom"}}})).unwrap();
        assert_eq!(thread.review_config.model, "other-model");
        assert_eq!(
            compat::response(&thread, Value::Null)["approvalsReviewer"],
            "auto_review"
        );
        assert!(configure(&mut thread, &json!({"config":{"guardian":{"model":""}}})).is_err());
    }
}
