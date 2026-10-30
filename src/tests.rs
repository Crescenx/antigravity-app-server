use super::*;

#[test]
fn reopening_a_stopped_cancelled_thread_preserves_unknown_history_without_replay() {
    let (_, mut thread, _) = fixture();
    thread.status = "unknown".into();
    thread.turn =
        Some(json!({"id":"cancelled","status":"unknown","interruptRequested":true,"items":[]}));
    let revision = thread.revision;
    assert!(recover_interrupted_thread(&mut thread));
    assert_eq!(thread.status, "idle");
    assert_eq!(thread.revision, revision + 1);
    assert_eq!(thread.turn.as_ref().unwrap()["status"], "unknown");
    assert_eq!(
        compat::turn(thread.turn.as_ref().unwrap())["status"],
        "failed"
    );
    assert!(!recover_interrupted_thread(&mut thread));
    thread.status = "unknown".into();
    thread.turn.as_mut().unwrap()["interruptRequested"] = json!(false);
    assert!(!recover_interrupted_thread(&mut thread));
}

#[test]
fn native_nonzero_exit_is_failed() {
    for (code, status) in [(0, "completed"), (1, "failed"), (-1, "failed")] {
        let item = compat::tool_item(
            &json!({"state":"STATE_DONE","runCommand":{"commandLine":"false","exitCode":code}}),
            "command:1",
        )
        .unwrap();
        assert_eq!(item["status"], status);
    }
}

#[test]
fn native_file_reads_and_errors_have_standard_items() {
    let step = json!({"state":"STATE_DONE","viewFile":{"filePath":"/tmp/lecture.pdf","startLine":1},"text":"read file"});
    let item = compat::tool_item(&step, "read:1").unwrap();
    assert_eq!(item["type"], "dynamicToolCall");
    assert_eq!(item["arguments"]["filePath"], "/tmp/lecture.pdf");
    assert_eq!(item["success"], true);
    assert_eq!(item["result"]["path"], "/tmp/lecture.pdf");
    assert!(
        item["contentItems"][0]["text"]
            .as_str()
            .unwrap()
            .contains("lecture.pdf")
    );
    for error in [json!("broken pipe"), json!({"message":"broken pipe"})] {
        let turn =
            compat::turn(&json!({"status":"failed","error":null,"nativeStop":{"error":error}}));
        assert_eq!(turn["error"]["message"], "broken pipe");
    }
    assert!(
        compat::turn(&json!({"status":"completed","nativeStop":{"error":null}}))["error"].is_null()
    );
}

#[tokio::test]
async fn injected_history_is_durable_and_delivered_with_first_goal_turn_only() {
    let (mut server, thread, _) = fixture();
    let items = json!([{"type":"message","role":"developer","content":[{"type":"input_text","text":"previous provider reference"}]}]);
    server
        .request(
            "thread/inject_items",
            json!({"threadId":thread.id,"items":items}),
        )
        .await
        .unwrap();
    assert_eq!(
        server.store.load(&thread.id).unwrap().pending_context,
        items.as_array().unwrap().clone()
    );
    assert!(server.store.load(&thread.id).unwrap().turn.is_none());
    let (commands, mut rx) = mpsc::channel(4);
    server.connections.insert(
        thread.id.clone(),
        Connection {
            pid: None,
            generation: "test".into(),
            commands,
        },
    );
    let worker = tokio::spawn(async move {
        let Some(Command::Send(value, reply)) = rx.recv().await else {
            panic!("goal input missing")
        };
        reply.send(Ok(())).unwrap();
        (value, rx)
    });
    server
        .request(
            "thread/goal/set",
            json!({"threadId":thread.id,"objective":"finish authorized goal","tokenBudget":100}),
        )
        .await
        .unwrap();
    server.advance_goals().await.unwrap();
    server.advance_goals().await.unwrap();
    let (value, mut rx) = worker.await.unwrap();
    assert!(value.to_string().contains("previous provider reference"));
    assert!(value.to_string().contains("finish authorized goal"));
    assert!(rx.try_recv().is_err());
    let saved = server.store.load(&thread.id).unwrap();
    assert!(saved.pending_context.is_empty());
    assert_eq!(
        saved.turn.as_ref().unwrap()["goalId"],
        saved.goal.as_ref().unwrap()["id"]
    );
    assert_eq!(saved.turn.as_ref().unwrap()["injectedContext"], items);
    assert!(
        !saved.turn.unwrap()["input"]
            .to_string()
            .contains("previous provider reference")
    );
    assert!(
        server
            .request(
                "thread/inject_items",
                json!({"threadId":thread.id,"items":items})
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn tool_receipts_never_repeat_completed_or_unknown_side_effects() {
    let (mut server, mut thread, _) = fixture();
    let (commands, mut rx) = mpsc::channel(4);
    server.connections.insert(
        thread.id.clone(),
        Connection {
            pid: None,
            generation: "test".into(),
            commands,
        },
    );
    let call = json!({"id":"same","name":"run_command","argumentsJson":"{\"CommandLine\":\"touch must-not-execute\"}"});
    thread.tool_receipts.insert(
        "same".into(),
        json!({"call":call,"state":"completed","result":{"success":true,"output":"saved"}}),
    );
    let worker = tokio::spawn(async move {
        let mut values = Vec::new();
        for _ in 0..2 {
            let Some(Command::Send(v, reply)) = rx.recv().await else {
                panic!("missing response")
            };
            values.push(v);
            reply.send(Ok(())).unwrap();
        }
        values
    });
    server.tool_call(&mut thread, &call).await.unwrap();
    thread
        .tool_receipts
        .get_mut("same")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("result");
    server.tool_call(&mut thread, &call).await.unwrap();
    let values = worker.await.unwrap();
    assert!(
        values[0]["toolResponse"]["responseJson"]
            .as_str()
            .unwrap()
            .contains("saved")
    );
    assert!(
        values[1]["toolResponse"]["errorMessage"]
            .as_str()
            .unwrap()
            .contains("not replayed")
    );
    assert!(server.tool_jobs.is_empty());
    let mut changed = call;
    changed["argumentsJson"] = json!("{}");
    assert!(server.tool_call(&mut thread, &changed).await.is_err());
}

#[tokio::test]
async fn viewer_tools_and_cache_usage_have_standard_events() {
    let (mut server, mut thread, mut output) = fixture();
    thread.turn = Some(json!({"id":"turn","status":"inProgress","items":[]}));
    let wire_command = "# agy-native-command-fixture\npwd";
    thread.native_command_receipts.insert(
        "viewer-command".into(),
        json!({
            "callId":"viewer-command","trajectoryId":thread.id,"stepIndex":7,"turnId":"turn",
            "generation":"test","revision":thread.revision,"state":"running",
            "original":{"CommandLine":"pwd","Cwd":thread.cwd,"BypassSandbox":false},
            "modified":{"CommandLine":wire_command,"Cwd":thread.cwd,"BypassSandbox":true}
        }),
    );
    server.store.save(&thread).unwrap();
    let (commands, _rx) = mpsc::channel(4);
    server.connections.insert(
        thread.id.clone(),
        Connection {
            pid: None,
            generation: "test".into(),
            commands,
        },
    );
    for event in [
        json!({"stepUpdate":{"trajectoryId":thread.id,"stepIndex":1,"source":"SOURCE_MODEL","target":"TARGET_ENVIRONMENT","state":"STATE_DONE","runCommand":{"commandLine":wire_command,"combinedOutput":"/workspace","exitCode":0}}}),
        json!({"stepUpdate":{"trajectoryId":thread.id,"stepIndex":2,"source":"SOURCE_MODEL","target":"TARGET_ENVIRONMENT","state":"STATE_DONE","editFile":{"filePath":"file:///workspace/a"}}}),
        json!({"stepUpdate":{"trajectoryId":thread.id,"stepIndex":3,"source":"SOURCE_MODEL","target":"TARGET_ENVIRONMENT","state":"STATE_DONE","mcpTool":{"serverName":"fixture","toolName":"echo","argumentsJson":"{}"}}}),
        json!({"usageUpdate":{"total":{"promptTokenCount":"100","cachedContentTokenCount":"70","candidatesTokenCount":"10","thoughtsTokenCount":"5","totalTokenCount":"115"}}}),
        json!({"usageUpdate":{"total":{"promptTokenCount":"220","cachedContentTokenCount":"160","candidatesTokenCount":"20","thoughtsTokenCount":"10","totalTokenCount":"250"}}}),
    ] {
        server
            .event(RuntimeEvent {
                id: thread.id.clone(),
                generation: "test".into(),
                event: Ok(event),
            })
            .await
            .unwrap();
    }
    let saved = server.store.load(&thread.id).unwrap();
    assert_eq!(
        saved.native_command_receipts["viewer-command"]["actualStepIndex"],
        1
    );
    assert_eq!(
        saved.native_command_receipts["viewer-command"]["state"],
        "completed"
    );
    assert_eq!(
        compat::thread(&saved)["tokenUsage"]["total"]["cachedInputTokens"],
        160
    );
    let items = saved.turn.unwrap()["items"].as_array().unwrap().clone();
    assert_eq!(items[0]["command"], "pwd");
    assert_eq!(
        items
            .iter()
            .map(|v| v["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["commandExecution", "fileChange", "mcpToolCall"]
    );
    let mut usage = None;
    while let Ok(e) = output.try_recv() {
        if e["method"] == "thread/tokenUsage/updated" {
            usage = Some(e["params"]["tokenUsage"].clone());
        }
    }
    let usage = usage.expect("standard usage event");
    assert_eq!(usage["total"]["cachedInputTokens"], 160);
    assert_eq!(usage["last"]["cachedInputTokens"], 90);
    assert_eq!(usage["last"]["totalTokens"], 135);
    assert_eq!(usage["modelContextWindow"], Value::Null);
}

#[test]
fn captured_app_context_replays_into_viewer_usage() {
    // App 2.24.0 GetCascadeTrajectory capture, restricted to token metadata.
    let response: Value =
        serde_json::from_str(include_str!("testdata/native-context-window.json")).unwrap();
    let mut state = native_state::NativeState::new("replay");
    let events = state.snapshot(&response);
    let usage = events.iter().find_map(|e| e.get("usageUpdate")).unwrap();
    let public = compat::usage(usage, &Value::Null);
    assert_eq!(public["modelContextWindow"], 840000);
    assert_eq!(public["contextWindow"]["estimatedTokensUsed"], 18792);
    assert_eq!(
        public["contextWindow"]["tokenBreakdown"]["groups"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(public["last"]["inputTokens"], 20292);
    assert_eq!(public["last"]["totalTokens"], 20309);
    assert_eq!(public["total"]["totalTokens"], 20309);
    assert_eq!(public["lastScope"], "nativeModelInvocation");
    assert!(
        state
            .snapshot(&response)
            .iter()
            .all(|event| event.get("usageUpdate").is_none())
    );
}

#[test]
fn viewer_usage_separates_context_from_cumulative_cost() {
    let previous = json!({"total":{"promptTokenCount":20000,"totalTokenCount":21000}});
    let mut usage = json!({"total":null,"contextWindow":{
        "estimatedTokensUsed":18792,"maxContextTokens":840000,
        "tokenBreakdown":{"totalTokens":18792},"truncationReason":"TRUNCATION_REASON_UNSPECIFIED"
    }});
    let estimate = compat::usage(&usage, &previous);
    assert_eq!(estimate["last"]["totalTokens"], 18792);
    assert_eq!(estimate["total"]["totalTokens"], 21000);
    assert_eq!(estimate["modelContextWindow"], 840000);
    assert_eq!(estimate["lastScope"], "nativeContextWindowEstimate");
    assert_eq!(estimate["contextWindow"], usage["contextWindow"]);
    usage["lastInvocationUsage"] = json!({"inputTokens":"549","cacheReadTokens":"6481",
        "outputTokens":"116","thinkingOutputTokens":"100"});
    usage["total"] = json!({"promptTokenCount":27030,"totalTokenCount":28146});
    let actual = compat::usage(&usage, &previous);
    assert_eq!(
        actual["last"],
        json!({"inputTokens":7030,"cachedInputTokens":6481,
        "outputTokens":16,"reasoningOutputTokens":100,"totalTokens":7146})
    );
    assert_eq!(actual["total"]["totalTokens"], 28146);
    assert_eq!(actual["lastScope"], "nativeModelInvocation");
    for invalid in [
        Value::Null,
        json!({"estimatedTokensUsed":-1,"maxContextTokens":840000}),
        json!({"estimatedTokensUsed":10,"maxContextTokens":0}),
    ] {
        usage["contextWindow"] = invalid;
        let result = compat::usage(&usage, &previous);
        assert!(result["modelContextWindow"].is_null());
        assert_eq!(result["last"]["totalTokens"], 7146);
        assert_eq!(result["lastScope"], "nativeUsageUpdateDelta");
    }
}

#[tokio::test]
async fn viewer_context_is_published_and_persisted_before_billing_usage() {
    let (mut server, mut thread, mut output) = fixture();
    thread.turn = Some(json!({"id":"turn","status":"inProgress","items":[]}));
    thread.usage = json!({"total":{"totalTokenCount":21000}});
    server.store.save(&thread).unwrap();
    let (commands, _rx) = mpsc::channel(4);
    server.connections.insert(
        thread.id.clone(),
        Connection {
            pid: None,
            generation: "test".into(),
            commands,
        },
    );
    let mut state = native_state::NativeState::new(&thread.id);
    let events = state.snapshot(&json!({"generatorMetadata":[{"chatModel":{
        "chatStartMetadata":{"contextWindowMetadata":{"estimatedTokensUsed":18792,"maxContextTokens":840000}}
    }}]}));
    for event in events {
        server
            .event(RuntimeEvent {
                id: thread.id.clone(),
                generation: "test".into(),
                event: Ok(event),
            })
            .await
            .unwrap();
    }
    let saved = server.store.load(&thread.id).unwrap();
    assert_eq!(saved.usage, thread.usage);
    assert_eq!(
        compat::thread(&saved)["tokenUsage"]["modelContextWindow"],
        840000
    );
    assert_eq!(saved.token_usage["last"]["totalTokens"], 18792);
    let mut updates = Vec::new();
    while let Ok(event) = output.try_recv() {
        if event["method"] == "thread/tokenUsage/updated" {
            updates.push(event);
        }
    }
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0]["params"]["tokenUsage"], saved.token_usage);
}

#[test]
fn viewer_policy_does_not_silently_accept_unenforced_sandbox() {
    for policy in [
        json!({"type":"notAPolicy"}),
        json!({"type":"workspaceWrite","networkAccess":"false"}),
    ] {
        assert!(compat::policy(&json!({"sandboxPolicy":policy})).is_err());
    }
    let (_, mut thread, _) = fixture();
    configure(&mut thread, &json!({"sandboxPolicy":{"type":"readOnly"}})).unwrap();
    assert_eq!(
        compat::response(&thread, Value::Null)["sandbox"]["type"],
        "readOnly"
    );
    assert!(compat::policy(&json!({"collaborationMode":{"mode":"default","settings":{"model":"test","reasoning_effort":null,"developer_instructions":null}}})).is_ok());
    assert!(
        compat::policy(&json!({"collaborationMode":{"mode":"plan","settings":{"model":"test"}}}))
            .is_ok()
    );
}

#[test]
fn plan_preserves_requested_policy_and_rejects_goal_activation() {
    let (_, mut thread, _) = fixture();
    configure(&mut thread, &json!({"sandbox":"danger-full-access"})).unwrap();
    let requested = thread.requested_permissions().unwrap().effective();
    thread.tool_grants.insert("old-grant".into());
    configure(
        &mut thread,
        &json!({"collaborationMode":{"mode":"plan","settings":{"model":"test"}}}),
    )
    .unwrap();
    assert!(thread.is_plan());
    assert!(thread.tool_grants.is_empty());
    assert_eq!(
        thread.permissions().unwrap().effective(),
        json!({"type":"readOnly","networkAccess":false})
    );
    assert_eq!(
        thread.requested_permissions().unwrap().effective(),
        requested
    );
    assert!(goal::mutate(&mut thread, &json!({"objective":"change files"}), false).is_err());
    let persisted = serde_json::to_value(&thread).unwrap();
    let mut restored: Thread = serde_json::from_value(persisted.clone()).unwrap();
    assert!(restored.is_plan());
    configure(
        &mut restored,
        &json!({"collaborationMode":{"mode":"default","settings":{"model":"test"}}}),
    )
    .unwrap();
    assert_eq!(restored.permissions().unwrap().effective(), requested);
    let mut legacy = persisted;
    legacy.as_object_mut().unwrap().remove("collaborationMode");
    assert!(!serde_json::from_value::<Thread>(legacy).unwrap().is_plan());
}

#[tokio::test]
async fn viewer_context_is_delivered_once_and_part_of_receipt_identity() {
    let (mut server, thread, _) = fixture();
    let (commands, mut rx) = mpsc::channel(4);
    server.connections.insert(
        thread.id.clone(),
        Connection {
            pid: None,
            generation: "test".into(),
            commands,
        },
    );
    let worker = tokio::spawn(async move {
        let Some(Command::Send(value, reply)) = rx.recv().await else {
            panic!("missing input")
        };
        reply.send(Ok(())).unwrap();
        value
    });
    let p = json!({"threadId":thread.id,"clientUserMessageId":"context-test","input":[{"type":"text","text":"continue"}],"additionalContext":{"viewer:switched-history":{"kind":"application","value":"previous provider result"}},"collaborationMode":{"mode":"default","settings":{"model":"test","reasoning_effort":null,"developer_instructions":null}}});
    let first = server.request("turn/start", p.clone()).await.unwrap();
    let delivered = worker.await.unwrap();
    assert!(
        delivered["userInput"]["parts"]
            .to_string()
            .contains("previous provider result")
    );
    assert_eq!(
        server.request("turn/start", p.clone()).await.unwrap(),
        first
    );
    let mut changed = p;
    changed["additionalContext"]["viewer:switched-history"]["value"] = json!("different result");
    assert!(server.request("turn/start", changed).await.is_err());
}

#[tokio::test]
async fn viewer_modes_names_and_archive_are_supported() {
    let (mut server, thread, _) = fixture();
    assert_eq!(
        server
            .request("collaborationMode/list", json!({}))
            .await
            .unwrap()["data"][0]["mode"],
        "default"
    );
    server
        .request(
            "thread/name/set",
            json!({"threadId":thread.id,"name":"My task"}),
        )
        .await
        .unwrap();
    assert_eq!(
        server
            .request("thread/read", json!({"threadId":thread.id}))
            .await
            .unwrap()["thread"]["name"],
        "My task"
    );
    server
        .request("thread/archive", json!({"threadId":thread.id}))
        .await
        .unwrap();
    assert_eq!(
        server.request("thread/list", json!({})).await.unwrap()["data"],
        json!([])
    );
    assert_eq!(
        server
            .request("thread/list", json!({"archived":true}))
            .await
            .unwrap()["data"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn configuration_rejects_invalid_limits_and_resolves_directories() {
    let (server, mut thread, _) = fixture();
    assert!(configure(&mut thread, &json!({"budget":{"maxModelCalls":0}})).is_err());
    assert!(configure(&mut thread, &json!({"budget":{"unknown":1}})).is_err());
    assert!(configure(&mut thread, &json!({"approvalPolicy":"typo"})).is_err());
    assert!(configure(&mut thread, &json!({"additionalDirectories":["."]})).is_err());
    configure(&mut thread, &json!({"additionalDirectories":[server.store.root,server.store.root],"budget":{"maxModelCalls":1,"scope":"BUDGET_SCOPE_FORWARD_LOOKING"},"unrestricted":true})).unwrap();
    assert_eq!(thread.additional_directories.len(), 1);
    assert!(thread.unrestricted);
    configure(
        &mut thread,
        &json!({"budget":null,"unrestricted":false,"additionalDirectories":[]}),
    )
    .unwrap();
    assert!(thread.budget.is_none());
    assert!(thread.additional_directories.is_empty());
    assert!(!thread.unrestricted);
    let root = server.store.root.clone();
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn native_idle_after_acknowledged_halt_is_interrupted() {
    let (mut server, mut thread, _) = fixture();
    thread.status = "running".into();
    thread.turn = Some(json!({"id":"turn","status":"inProgress"}));
    server.store.save(&thread).unwrap();
    let (commands, mut rx) = mpsc::channel(4);
    server.connections.insert(
        thread.id.clone(),
        Connection {
            pid: None,
            generation: "test".into(),
            commands,
        },
    );
    let worker = tokio::spawn(async move {
        let Some(Command::Send(value, reply)) = rx.recv().await else {
            panic!("missing halt")
        };
        assert_eq!(value, json!({"haltRequest":true}));
        reply.send(Ok(())).unwrap();
    });
    server
        .request(
            "turn/interrupt",
            json!({"threadId":thread.id,"turnId":"turn"}),
        )
        .await
        .unwrap();
    server.event(RuntimeEvent {id:thread.id.clone(),generation:"test".into(),event:Ok(json!({"trajectoryStateUpdate":{"state":"STATE_FULLY_IDLE","trajectoryId":thread.id}}))}).await.unwrap();
    let turn = server.store.load(&thread.id).unwrap().turn.unwrap();
    assert_eq!(turn["status"], "interrupted");
    assert_eq!(turn["nativeStop"]["state"], "STATE_FULLY_IDLE");
    worker.await.unwrap();
    let root = server.store.root.clone();
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

pub(crate) fn fixture() -> (Server, Thread, mpsc::UnboundedReceiver<Value>) {
    let root = std::env::current_dir()
        .unwrap()
        .join(".tmp/rust-tests")
        .join(uuid::Uuid::new_v4().to_string());
    let store = Arc::new(Store::open(&root).unwrap());
    let thread = Thread {
        runtime: Some("app-core-v1".into()),
        subagents: subagents::State::default(),
        options: Default::default(),
        collaboration_mode: "default".into(),
        id: uuid::Uuid::new_v4().to_string(),
        cwd: root,
        model: "test".into(),
        pending_context: Vec::new(),
        name: None,
        archived: false,
        permission_policy: Some(
            permissions::PermissionPolicy::from_rpc(&json!({}), &store.root).unwrap(),
        ),
        tool_receipts: BTreeMap::new(),
        native_command_receipts: BTreeMap::new(),
        tool_grants: Default::default(),
        revision: 0,
        status: "idle".into(),
        turn: None,
        approval_policy: "on-request".into(),
        approvals_reviewer: "user".into(),
        review_config: guardian::ReviewConfig::default(),
        budget: None,
        additional_directories: Vec::new(),
        unrestricted: false,
        usage: Value::Null,
        token_usage: Value::Null,
        receipts: BTreeMap::new(),
        steer_receipts: BTreeMap::new(),
        turns: Vec::new(),
        created_at: now(),
        updated_at: now(),
        goal: None,
        mcp_servers: Value::Null,
    };
    store.save(&thread).unwrap();
    let (output, rx) = mpsc::unbounded_channel();
    let (events, _) = mpsc::unbounded_channel();
    let server = Server {
        store,
        binary: PathBuf::new(),
        endpoint: json!({}),
        model_catalog: Some((
            std::time::Instant::now(),
            models::Catalog::parse(&json!({"models":{"test":{}}})).unwrap(),
        )),
        output,
        events,
        connections: BTreeMap::new(),
        approvals: BTreeMap::new(),
        grants: Default::default(),
        tool_jobs: BTreeMap::new(),
        reviews: BTreeMap::new(),
        mcp: BTreeMap::new(),
        config: json!({}),
        control: mpsc::unbounded_channel().0,
        agent_waits: BTreeMap::new(),
        initialized: true,
    };
    (server, thread, rx)
}

#[tokio::test]
async fn steering_is_durable_idempotent_and_acknowledged_in_fifo_order() {
    let (mut server, mut thread, mut output) = fixture();
    thread.status = "running".into();
    thread.turn = Some(json!({"id":"active","status":"inProgress","items":[],
        "input":[{"type":"text","text":"same"}],"nativeInputText":"same","clientUserMessageId":"initial"}));
    server.store.save(&thread).unwrap();
    let (commands, mut rx) = mpsc::channel(4);
    server.connections.insert(
        thread.id.clone(),
        Connection {
            pid: None,
            generation: "test".into(),
            commands,
        },
    );
    let worker = tokio::spawn(async move {
        let Some(Command::Send(value, reply)) = rx.recv().await else {
            panic!("missing steer");
        };
        reply.send(Ok(())).unwrap();
        assert_eq!(value, json!({"userInput":{"parts":[{"text":"same"}]}}));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
                .await
                .is_err()
        );
    });
    let params = json!({"threadId":thread.id,"expectedTurnId":"active","clientUserMessageId":"queued","input":[{"type":"text","text":"same"}]});
    assert_eq!(
        server.steer(&params).await.unwrap()["deliveryStatus"],
        "queued"
    );
    assert_eq!(
        server.steer(&params).await.unwrap()["deliveryStatus"],
        "queued"
    );
    assert_eq!(
        server.store.load(&thread.id).unwrap().steer_receipts["queued"]["state"],
        "queued"
    );
    let mut changed = params.clone();
    changed["input"][0]["text"] = json!("changed");
    assert!(server.steer(&changed).await.is_err());
    for (trajectory, index) in [
        ("child".to_string(), 0),
        (thread.id.clone(), 0),
        (thread.id.clone(), 1),
        (thread.id.clone(), 1),
    ] {
        server.event(RuntimeEvent{id:thread.id.clone(),generation:"test".into(),event:Ok(json!({"stepUpdate":{"trajectoryId":trajectory,"stepIndex":index,"source":"SOURCE_USER","target":"TARGET_MODEL","state":"STATE_DONE","text":"same"}}))}).await.unwrap();
    }
    let saved = server.store.load(&thread.id).unwrap();
    assert_eq!(saved.steer_receipts["queued"]["state"], "consumed");
    let items = saved.turn.unwrap()["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["clientId"], "initial");
    assert_eq!(items[1]["clientId"], "queued");
    let mut delivered = Vec::new();
    while let Ok(e) = output.try_recv() {
        if e["method"] == "item/completed" {
            delivered.push(e["params"]["item"]["clientId"].clone());
        }
    }
    assert_eq!(delivered, vec![json!("initial"), json!("queued")]);
    assert_eq!(
        server.steer(&params).await.unwrap()["deliveryStatus"],
        "consumed"
    );
    worker.await.unwrap();
}

#[tokio::test]
async fn steering_rejects_stale_turn_and_does_not_replay_unknown_dispatch() {
    let (mut server, mut thread, _) = fixture();
    thread.status = "running".into();
    thread.turn = Some(json!({"id":"active","items":[]}));
    server.store.save(&thread).unwrap();
    let (commands, rx) = mpsc::channel(1);
    drop(rx);
    server.connections.insert(
        thread.id.clone(),
        Connection {
            pid: None,
            generation: "test".into(),
            commands,
        },
    );
    let mut p = json!({"threadId":thread.id,"expectedTurnId":"stale","clientUserMessageId":"queued","input":[{"type":"text","text":"next"}]});
    assert!(server.steer(&p).await.is_err());
    assert!(
        server
            .store
            .load(&thread.id)
            .unwrap()
            .steer_receipts
            .is_empty()
    );
    p["expectedTurnId"] = json!("active");
    assert!(server.steer(&p).await.is_err());
    assert_eq!(server.steer(&p).await.unwrap()["deliveryStatus"], "unknown");
    let mut saved = server.store.load(&thread.id).unwrap();
    saved.turn.as_mut().unwrap()["interruptRequested"] = json!(true);
    server.store.save(&saved).unwrap();
    p["clientUserMessageId"] = json!("new");
    assert!(server.steer(&p).await.is_err());
}

#[tokio::test]
async fn idle_frame_racing_steer_waits_for_consumption_before_completing() {
    let (mut server, mut thread, _) = fixture();
    thread.status = "running".into();
    thread.turn = Some(json!({"id":"active","items":[],"nativeUserItemId":"initial"}));
    thread.steer_receipts.insert("queued".into(),json!({"turnId":"active","state":"queued","nativeText":"next","sequence":1,"input":[{"type":"text","text":"next"}]}));
    server.store.save(&thread).unwrap();
    let (commands, _rx) = mpsc::channel(1);
    server.connections.insert(
        thread.id.clone(),
        Connection {
            pid: None,
            generation: "test".into(),
            commands,
        },
    );
    let idle =
        json!({"trajectoryStateUpdate":{"trajectoryId":thread.id,"state":"STATE_FULLY_IDLE"}});
    server
        .event(RuntimeEvent {
            id: thread.id.clone(),
            generation: "test".into(),
            event: Ok(idle.clone()),
        })
        .await
        .unwrap();
    assert_eq!(server.store.load(&thread.id).unwrap().status, "running");
    server.event(RuntimeEvent{id:thread.id.clone(),generation:"test".into(),event:Ok(json!({"stepUpdate":{"trajectoryId":thread.id,"stepIndex":2,"source":"SOURCE_USER","state":"STATE_DONE","text":"next"}}))}).await.unwrap();
    server
        .event(RuntimeEvent {
            id: thread.id.clone(),
            generation: "test".into(),
            event: Ok(idle),
        })
        .await
        .unwrap();
    assert_eq!(server.store.load(&thread.id).unwrap().status, "idle");
}

#[tokio::test]
async fn terminal_native_states_do_not_wait_forever_for_queued_steering() {
    for state in [
        json!({"state":"STATE_CANCELLED"}),
        json!({"state":"STATE_FULLY_IDLE","stopReason":"STOP_REASON_MAX_MODEL_CALLS_EXCEEDED"}),
        json!({"state":"STATE_FULLY_IDLE","error":{"message":"model failed"}}),
    ] {
        let (mut server, mut thread, _) = fixture();
        thread.status = "running".into();
        thread.turn = Some(json!({"id":"active","items":[]}));
        thread
            .steer_receipts
            .insert("queued".into(), json!({"turnId":"active","state":"queued"}));
        server.store.save(&thread).unwrap();
        let (commands, _rx) = mpsc::channel(1);
        server.connections.insert(
            thread.id.clone(),
            Connection {
                pid: None,
                generation: "test".into(),
                commands,
            },
        );
        let mut state = state;
        state["trajectoryId"] = json!(thread.id);
        server
            .event(RuntimeEvent {
                id: thread.id.clone(),
                generation: "test".into(),
                event: Ok(json!({"trajectoryStateUpdate":state})),
            })
            .await
            .unwrap();
        let saved = server.store.load(&thread.id).unwrap();
        assert_eq!(saved.status, "idle");
        assert_eq!(saved.steer_receipts["queued"]["state"], "unknown");
        if !state["error"].is_null() {
            assert_eq!(saved.turn.as_ref().unwrap()["status"], "failed");
            assert_eq!(
                saved.turn.as_ref().unwrap()["error"]["message"],
                "model failed"
            );
        }
    }
}

#[test]
fn store_is_single_writer_and_rejects_path_traversal() {
    let (server, thread, _) = fixture();
    assert!(Store::open(&server.store.root).is_err());
    assert!(server.store.load("../../secret").is_err());
    assert_eq!(server.store.load(&thread.id).unwrap().id, thread.id);
    let root = server.store.root.clone();
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn turn_receipt_prevents_replay_and_rejects_conflicting_input() {
    let (mut server, thread, _) = fixture();
    let (commands, mut rx) = mpsc::channel(4);
    server.connections.insert(
        thread.id.clone(),
        Connection {
            pid: None,
            generation: "test".into(),
            commands,
        },
    );
    let worker = tokio::spawn(async move {
        let Some(Command::Send(value, reply)) = rx.recv().await else {
            panic!("missing native command")
        };
        assert_eq!(value["userInput"]["parts"][0]["text"], "hello");
        reply.send(Ok(())).unwrap();
        assert!(rx.try_recv().is_err());
    });
    let p = json!({"threadId":thread.id,"requestId":"r1","input":[{"type":"text","text":"hello"}]});
    let first = server.request("turn/start", p.clone()).await.unwrap();
    assert_eq!(
        server.request("turn/start", p.clone()).await.unwrap(),
        first
    );
    let mut conflict = p.clone();
    conflict["outputSchema"] = json!({"type":"object"});
    assert!(server.request("turn/start", conflict).await.is_err());
    let mut conflict = p.clone();
    conflict["input"][0]["text"] = json!("different");
    assert!(server.request("turn/start", conflict).await.is_err());
    let mut busy = p;
    busy["requestId"] = json!("r2");
    assert!(server.request("turn/start", busy).await.is_err());
    worker.await.unwrap();
    let root = server.store.root.clone();
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn structured_native_finish_maps_to_message_and_invalid_output_fails_turn() {
    for (text, expected) in [
        (r#"{"title":"正确标题"}"#, "completed"),
        (r#"{"title":42}"#, "failed"),
    ] {
        let (mut server, mut thread, mut output) = fixture();
        thread.status = "running".into();
        thread.turn = Some(json!({"id":"turn","status":"inProgress","items":[]}));
        thread.options.output_schema = Some(
            json!({"type":"object","properties":{"title":{"type":"string"}},"required":["title"]}),
        );
        server.store.save(&thread).unwrap();
        let (commands, _rx) = mpsc::channel(4);
        server.connections.insert(
            thread.id.clone(),
            Connection {
                pid: None,
                generation: "live".into(),
                commands,
            },
        );
        server.event(RuntimeEvent{id:thread.id.clone(),generation:"live".into(),event:Ok(json!({"stepUpdate":{"trajectoryId":thread.id,"stepIndex":1,"source":"SOURCE_MODEL","target":"TARGET_USER","text":"Finished","finish":{"outputString":text},"state":"STATE_DONE"}}))}).await.unwrap();
        server.event(RuntimeEvent{id:thread.id.clone(),generation:"live".into(),event:Ok(json!({"trajectoryStateUpdate":{"trajectoryId":thread.id,"state":"STATE_FULLY_IDLE"}}))}).await.unwrap();
        let saved = server.store.load(&thread.id).unwrap().turn.unwrap();
        assert_eq!(saved["status"], expected);
        assert_eq!(saved["items"][0]["text"], text);
        assert_eq!(saved["items"][0]["type"], "agentMessage");
        if expected == "failed" {
            assert!(
                saved["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("outputSchema")
            );
        }
        let mut completed = None;
        while let Ok(n) = output.try_recv() {
            if n["method"] == "turn/completed" {
                completed = Some(n);
            }
        }
        assert_eq!(completed.unwrap()["params"]["turn"]["status"], expected);
        let root = server.store.root.clone();
        drop(server);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn child_idle_and_stale_connections_cannot_complete_root_turn() {
    let (mut server, mut thread, _) = fixture();
    thread.status = "running".into();
    thread.turn = Some(json!({"id":"turn","status":"inProgress"}));
    server.store.save(&thread).unwrap();
    let (commands, _rx) = mpsc::channel(4);
    server.connections.insert(
        thread.id.clone(),
        Connection {
            pid: None,
            generation: "live".into(),
            commands,
        },
    );
    server
        .event(RuntimeEvent {
            id: thread.id.clone(),
            generation: "old".into(),
            event: Err(anyhow::anyhow!("old disconnect")),
        })
        .await
        .unwrap();
    assert!(server.connections.contains_key(&thread.id));
    server.event(RuntimeEvent {id:thread.id.clone(),generation:"live".into(),event:Ok(json!({"trajectoryStateUpdate":{"state":"STATE_FULLY_IDLE","parentTrajectoryId":thread.id,"depth":1}}))}).await.unwrap();
    server.event(RuntimeEvent {id:thread.id.clone(),generation:"live".into(),event:Ok(json!({"trajectoryStateUpdate":{"state":"STATE_FULLY_IDLE","trajectoryId":"child-with-no-parent-field"}}))}).await.unwrap();
    assert_eq!(server.store.load(&thread.id).unwrap().status, "running");
    server
        .event(RuntimeEvent {
            id: thread.id.clone(),
            generation: "live".into(),
            event: Err(anyhow::anyhow!("lost connection")),
        })
        .await
        .unwrap();
    let saved = server.store.load(&thread.id).unwrap();
    assert_eq!(saved.status, "unknown");
    assert!(
        server
            .request(
                "thread/reconcile",
                json!({"threadId":thread.id,"expectedRevision":0,"acknowledgeUnknownOutcome":true})
            )
            .await
            .is_err()
    );
    server.request("thread/reconcile",json!({"threadId":thread.id,"expectedRevision":saved.revision,"acknowledgeUnknownOutcome":true})).await.unwrap();
    assert_eq!(
        server.store.load(&thread.id).unwrap().turn.unwrap()["status"],
        "unknown"
    );
    let root = server.store.root.clone();
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn viewer_history_survives_reload_and_repeated_native_snapshots() {
    let (mut server, mut thread, mut output) = fixture();
    thread.status = "running".into();
    thread.turn = Some(
        json!({"id":"turn","status":"inProgress","items":[],"clientUserMessageId":"client-1"}),
    );
    server.store.save(&thread).unwrap();
    let (commands, _rx) = mpsc::channel(4);
    server.connections.insert(
        thread.id.clone(),
        Connection {
            pid: None,
            generation: "live".into(),
            commands,
        },
    );
    for (index, source, text) in [
        (0, "SOURCE_USER", "hello"),
        (1, "SOURCE_MODEL", "answer"),
        (1, "SOURCE_MODEL", "answer"),
    ] {
        server.event(RuntimeEvent{id:thread.id.clone(),generation:"live".into(),event:Ok(json!({"stepUpdate":{"trajectoryId":thread.id,"stepIndex":index,"source":source,"target":"TARGET_USER","text":text,"state":"STATE_DONE"}}))}).await.unwrap();
    }
    server.event(RuntimeEvent{id:thread.id.clone(),generation:"live".into(),event:Ok(json!({"trajectoryStateUpdate":{"trajectoryId":thread.id,"state":"STATE_FULLY_IDLE"}}))}).await.unwrap();
    let r = server
        .request(
            "thread/read",
            json!({"threadId":thread.id,"includeTurns":true}),
        )
        .await
        .unwrap();
    assert_eq!(
        r["thread"]["turns"][0]["items"].as_array().unwrap().len(),
        2
    );
    assert_eq!(
        r["thread"]["turns"][0]["items"][0]["content"][0]["text"],
        "hello"
    );
    assert_eq!(r["thread"]["turns"][0]["items"][0]["clientId"], "client-1");
    assert_eq!(r["thread"]["turns"][0]["status"], "completed");
    let mut completed = 0;
    while let Ok(event) = output.try_recv() {
        if event["method"] == "item/completed" {
            completed += 1;
        }
    }
    assert_eq!(completed, 2);
    let root = server.store.root.clone();
    drop(server);
    let reopened = Store::open(&root).unwrap();
    assert_eq!(
        compat::thread(&reopened.load(&thread.id).unwrap()),
        r["thread"]
    );
    drop(reopened);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn unsupported_worker_policy_is_rejected_before_identity_is_reserved() {
    let (mut server, thread, _) = fixture();
    for extra in [
        json!({"sandbox":"invalid-sandbox"}),
        json!({"ephemeral":"invalid"}),
        json!({"config":{"features.shell_tool":false}}),
        json!({"outputSchema":{"type":"invalid"}}),
    ] {
        let mut p = extra;
        p["cwd"] = json!(thread.cwd);
        assert!(server.request("thread/start", p).await.is_err());
        assert_eq!(server.store.list().unwrap().len(), 1);
    }
    let root = server.store.root.clone();
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn goal_scheduler_starts_once_pause_preserves_turn_and_budget_counts_once() {
    let (mut server, thread, _) = fixture();
    let (commands, mut rx) = mpsc::channel(4);
    server.connections.insert(
        thread.id.clone(),
        Connection {
            pid: None,
            generation: "live".into(),
            commands,
        },
    );
    let worker = tokio::spawn(async move {
        let Some(Command::Send(value, reply)) = rx.recv().await else {
            panic!("missing continuation")
        };
        assert!(
            value["userInput"]["parts"][0]["text"]
                .as_str()
                .unwrap()
                .contains("test goal")
        );
        reply.send(Ok(())).unwrap();
        rx
    });
    server
        .request(
            "thread/goal/set",
            json!({"threadId":thread.id,"objective":"test goal","tokenBudget":10}),
        )
        .await
        .unwrap();
    server.advance_goals().await.unwrap();
    server.advance_goals().await.unwrap();
    let mut rx = worker.await.unwrap();
    assert!(rx.try_recv().is_err());
    let turn = server.store.load(&thread.id).unwrap().turn.unwrap();
    server
        .request(
            "thread/goal/set",
            json!({"threadId":thread.id,"status":"paused"}),
        )
        .await
        .unwrap();
    assert_eq!(server.store.load(&thread.id).unwrap().turn.unwrap(), turn);
    assert!(rx.try_recv().is_err());
    for _ in 0..2 {
        server
            .event(RuntimeEvent {
                id: thread.id.clone(),
                generation: "live".into(),
                event: Ok(json!({"usageUpdate":{"total":{"totalTokenCount":"10"}}})),
            })
            .await
            .unwrap();
    }
    let paused = server
        .request("thread/goal/get", json!({"threadId":thread.id}))
        .await
        .unwrap();
    assert_eq!(paused["goal"]["tokensUsed"], 10);
    assert_eq!(paused["goal"]["status"], "paused");
    let resumed = server
        .request(
            "thread/goal/set",
            json!({"threadId":thread.id,"status":"active"}),
        )
        .await
        .unwrap();
    assert_eq!(resumed["goal"]["status"], "budgetLimited");
    server
        .request("thread/goal/clear", json!({"threadId":thread.id}))
        .await
        .unwrap();
    server
        .event(RuntimeEvent {
            id: thread.id.clone(),
            generation: "live".into(),
            event: Ok(json!({"usageUpdate":{"total":{"totalTokenCount":"20"}}})),
        })
        .await
        .unwrap();
    assert!(server.store.load(&thread.id).unwrap().goal.is_none());
    let root = server.store.root.clone();
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn skill_discovery_is_workspace_scoped_and_disabled_config_survives_reload() {
    let (server, thread, _) = fixture();
    let directory = thread.cwd.join(".agents/skills/contract-skill");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("SKILL.md"),"---\nname: contract-skill\ndescription: >-\n  first line\n  second line\n---\nUse ./resources/note.txt\n").unwrap();
    let group = skills::discover(&server.store, &thread.cwd).unwrap();
    let skill = group["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "contract-skill")
        .unwrap();
    assert_eq!(skill["description"], "first line second line");
    assert_eq!(skill["enabled"], true);
    let path = skill["path"].clone();
    skills::write(
        &server.store,
        &json!({"path":path,"enabled":false}),
        std::slice::from_ref(&thread.cwd),
    )
    .unwrap();
    let native = native_config::build(&server.store, &thread, json!([])).unwrap();
    assert!(
        !native["cascadeConfig"]["plannerConfig"]["customizationDiscoveryConfig"]["skills"]["skillsPaths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["absolute"] == path)
    );
    let sibling = thread.cwd.join("sibling");
    std::fs::create_dir(&sibling).unwrap();
    assert!(
        !skills::discover(&server.store, &sibling).unwrap()["skills"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "contract-skill")
    );
    let root = server.store.root.clone();
    drop(server);
    let store = Store::open(&root).unwrap();
    assert_eq!(
        skills::discover(&store, &thread.cwd).unwrap()["skills"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["path"] == path)
            .unwrap()["enabled"],
        false
    );
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn mcp_proxy_and_direct_calls_share_execution_state_and_override_forged_identity() {
    let cwd = std::env::current_dir().unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let app=axum::Router::new().route("/mcp",axum::routing::post({
        let calls=calls.clone();
        move |axum::Json(message):axum::Json<Value>| {
            let calls=calls.clone();
            async move {
                let params=&message["params"];
                let result=match message["method"].as_str().unwrap_or("") {
                    "initialize"=>json!({"protocolVersion":"2024-11-05","serverInfo":{"name":"rust-fixture","version":"1"},"capabilities":{"tools":{},"resources":{}}}),
                    "tools/list"=>json!({"tools":[{"name":"contract_echo","inputSchema":{"type":"object"}}]}),
                    "tools/call"=>{
                        assert_eq!(params["_meta"]["threadId"],"thread-a");
                        let count=calls.fetch_add(1,std::sync::atomic::Ordering::SeqCst)+1;
                        json!({"content":[{"type":"text","text":json!({"threadId":params["_meta"]["threadId"],"calls":count}).to_string()}]})
                    },
                    "resources/read"=>json!({"contents":[{"uri":"fixture://note","mimeType":"text/plain","text":"fixture"}]}),
                    _=>json!({}),
                };
                axum::Json(json!({"jsonrpc":"2.0","id":message["id"],"result":result}))
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let fixture = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let configs = json!({"fixture":{"url":url}});
    let hub = mcp::Hub::open("thread-a", &configs, &cwd).await.unwrap();
    let catalog = hub.catalog().await.unwrap();
    assert_eq!(catalog["data"][0]["status"], "ready");
    let direct=hub.call("fixture","tools/call",json!({"name":"contract_echo","arguments":{"text":"one"},"_meta":{"threadId":"forged"}})).await.unwrap();
    let result: Value =
        serde_json::from_str(direct["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(result["threadId"], "thread-a");
    assert_eq!(result["calls"], 1);
    let native = hub.native();
    let endpoint = &native[0]["http"];
    let client = reqwest::Client::new();
    let reply:Value=client.post(endpoint["url"].as_str().unwrap()).header("Authorization",endpoint["headers"]["Authorization"].as_str().unwrap())
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"contract_echo","arguments":{"text":"two"},"_meta":{"threadId":"forged"}}})).send().await.unwrap().json().await.unwrap();
    let result: Value =
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(result["calls"], 2);
    assert_eq!(result["threadId"], "thread-a");
    assert_eq!(
        client
            .post(endpoint["url"].as_str().unwrap())
            .json(&json!({"id":2,"method":"tools/list"}))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        hub.call("fixture", "resources/read", json!({"uri":"fixture://note"}))
            .await
            .unwrap()["contents"][0]["mimeType"],
        "text/plain"
    );
    hub.set_interrupted(true);
    assert!(
        hub.call(
            "fixture",
            "tools/call",
            json!({"name":"contract_echo","arguments":{}})
        )
        .await
        .is_err()
    );
    let denied:Value=client.post(endpoint["url"].as_str().unwrap()).header("Authorization",endpoint["headers"]["Authorization"].as_str().unwrap())
        .json(&json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"contract_echo","arguments":{}}})).send().await.unwrap().json().await.unwrap();
    assert!(denied.get("error").is_some());
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    hub.set_interrupted(false);
    hub.call(
        "fixture",
        "tools/call",
        json!({"name":"contract_echo","arguments":{}}),
    )
    .await
    .unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    let mut disabled = configs.clone();
    disabled["fixture"]["disabled_tools"] = json!(["contract_echo"]);
    let blocked = mcp::Hub::open("thread-b", &disabled, &cwd).await.unwrap();
    assert!(
        blocked
            .call(
                "fixture",
                "tools/call",
                json!({"name":"contract_echo","arguments":{"text":"x"}})
            )
            .await
            .is_err()
    );
    assert!(
        blocked.catalog().await.unwrap()["data"][0]["tools"]
            .as_object()
            .unwrap()
            .is_empty()
    );
    fixture.abort();
}

#[test]
fn native_input_and_questions_keep_paths_choice_indices_and_free_text() {
    let (server, thread, _) = fixture();
    assert_eq!(
        input::parts(&server.store, &thread, &json!([])).unwrap(),
        vec![json!({"text":""})]
    );
    assert!(
        input::parts(
            &server.store,
            &thread,
            &json!([{"type":"image","url":"https://example.invalid/image.png"}])
        )
        .is_err()
    );
    assert!(
        input::parts(
            &server.store,
            &thread,
            &json!([{"type":"image","url":"data:image/png;base64,invalid!"}])
        )
        .is_err()
    );
    let image = thread.cwd.join("image.png");
    image::RgbImage::new(2, 2).save(&image).unwrap();
    let parts = input::parts(
        &server.store,
        &thread,
        &json!([{"type":"localImage","path":image}]),
    )
    .unwrap();
    assert_eq!(parts[0]["media"]["mimeType"], "image/png");
    let step = json!({"trajectoryId":thread.id,"stepIndex":5,"questionsRequest":{"questions":[{"multipleChoice":{"question":"Format?","choices":["PDF","Text"]}},{"multipleChoice":{"question":"Comment?","choices":[]}}]}});
    let (key, _, request) = input::question(&step, &thread).unwrap();
    assert_eq!(request["method"], "item/tool/requestUserInput");
    let answer = input::answer(
        &step,
        &json!({"id":key,"result":{"answers":{"q0":{"answers":["Text"]},"q1":{"answers":["Keep formulas"]}}}}),
    );
    assert_eq!(
        answer["questionResponse"]["response"]["answers"][0]["multipleChoiceAnswer"]["selectedChoiceIndices"],
        json!([1])
    );
    assert_eq!(
        answer["questionResponse"]["response"]["answers"][1]["multipleChoiceAnswer"]["freeformResponse"],
        "Keep formulas"
    );
    assert_eq!(
        input::answer(&step, &json!({"error":{"message":"cancel"}}))["questionResponse"]["response"]
            ["answers"][0]["unanswered"],
        true
    );
    let root = server.store.root.clone();
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn goal_completion_requires_current_active_execution_and_clear_cannot_be_undone() {
    let (mut server, mut thread, _) = fixture();
    server
        .request(
            "thread/goal/set",
            json!({"threadId":thread.id,"objective":"finish","tokenBudget":100}),
        )
        .await
        .unwrap();
    let params = json!({"status":"complete","reason":"Verified requested output"});
    assert!(server.goal_action(&thread.id, &params).is_err());
    thread = server.store.load(&thread.id).unwrap();
    thread.status = "running".into();
    thread.turn = Some(
        json!({"id":"turn","status":"inProgress","goalId":thread.goal.as_ref().unwrap()["id"]}),
    );
    server.store.save(&thread).unwrap();
    server.goal_action(&thread.id, &params).unwrap();
    assert_eq!(
        server.store.load(&thread.id).unwrap().goal.unwrap()["status"],
        "complete"
    );
    server
        .request("thread/goal/clear", json!({"threadId":thread.id}))
        .await
        .unwrap();
    assert!(server.goal_action(&thread.id, &params).is_err());
    assert!(server.store.load(&thread.id).unwrap().goal.is_none());
    let root = server.store.root.clone();
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn native_session_approval_grants_only_identical_arguments_and_expires_on_disconnect() {
    let (mut server, mut thread, mut output) = fixture();
    thread.status = "running".into();
    thread.turn = Some(json!({"id":"turn","status":"inProgress"}));
    server.store.save(&thread).unwrap();
    let (commands, mut rx) = mpsc::channel(4);
    server.connections.insert(
        thread.id.clone(),
        Connection {
            pid: None,
            generation: "live".into(),
            commands,
        },
    );
    let worker = tokio::spawn(async move {
        let mut count = 0;
        while let Some(command) = rx.recv().await {
            match command {
                Command::Send(v, reply) => {
                    assert_eq!(
                        v["policyDecisionResponse"]["outcome"],
                        "POLICY_EVALUATION_OUTCOME_ALLOW"
                    );
                    assert!(v.get("grantKey").is_none());
                    count += 1;
                    reply.send(Ok(())).unwrap();
                }
                Command::Close(reply) => {
                    reply.send(()).unwrap();
                    break;
                }
            }
        }
        count
    });
    let native = json!({"policyDecisionRequest":{"requestId":"one","toolArgs":{"toolName":"mcp_write","trajectoryId":thread.id,"stepIndex":1,"argumentsJson":"{\"TargetFile\":\"proof.txt\"}"}}});
    server
        .event(RuntimeEvent {
            id: thread.id.clone(),
            generation: "live".into(),
            event: Ok(native.clone()),
        })
        .await
        .unwrap();
    let request = loop {
        let event = output.recv().await.unwrap();
        if event.get("id").is_some() {
            break event;
        }
    };
    assert_eq!(request["method"], "item/commandExecution/requestApproval");
    server
        .approval(json!({"id":request["id"],"result":{"decision":"acceptForSession"}}))
        .await
        .unwrap();
    let mut second = native.clone();
    second["policyDecisionRequest"]["requestId"] = json!("two");
    server
        .event(RuntimeEvent {
            id: thread.id.clone(),
            generation: "live".into(),
            event: Ok(second),
        })
        .await
        .unwrap();
    assert!(server.approvals.is_empty());
    let mut different = native;
    different["policyDecisionRequest"]["toolArgs"]["argumentsJson"] =
        json!("{\"TargetFile\":\"other.txt\"}");
    server
        .event(RuntimeEvent {
            id: thread.id.clone(),
            generation: "live".into(),
            event: Ok(different),
        })
        .await
        .unwrap();
    assert_eq!(server.approvals.len(), 1);
    server.disconnect(&thread.id).await;
    assert!(server.grants.is_empty());
    assert_eq!(worker.await.unwrap(), 2);
    let root = server.store.root.clone();
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn approval_errors_deny_and_duplicate_responses_cannot_reauthorize() {
    let (mut server, thread, _) = fixture();
    let (commands, mut rx) = mpsc::channel(4);
    server.connections.insert(
        thread.id.clone(),
        Connection {
            pid: None,
            generation: "test".into(),
            commands,
        },
    );
    server.approvals.insert(
        "a1".into(),
        (
            thread.id,
            json!({"policyDecisionResponse":{"requestId":"native-1"}}),
        ),
    );
    let worker = tokio::spawn(async move {
        let Some(Command::Send(value, reply)) = rx.recv().await else {
            panic!("missing native command")
        };
        assert_eq!(
            value["policyDecisionResponse"]["outcome"],
            "POLICY_EVALUATION_OUTCOME_DENY"
        );
        reply.send(Ok(())).unwrap();
    });
    server
        .approval(json!({"id":"a1","error":{"message":"cancelled"}}))
        .await
        .unwrap();
    assert!(
        server
            .approval(json!({"id":"a1","result":{"decision":"accept"}}))
            .await
            .is_err()
    );
    worker.await.unwrap();
    let root = server.store.root.clone();
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn sdk_threads_remain_readable_but_never_execute_after_migration() {
    let (mut server, mut thread, _) = fixture();
    thread.runtime = None;
    thread.turns = vec![
        json!({"id":"historic","status":"completed","items":[{"type":"agentMessage","text":"preserved history"}]}),
    ];
    server.store.save(&thread).unwrap();
    let result = server
        .request("thread/resume", json!({"threadId":thread.id}))
        .await
        .unwrap();
    assert_eq!(result["thread"]["readOnly"], true);
    assert_eq!(
        result["thread"]["turns"][0]["items"][0]["text"],
        "preserved history"
    );
    assert!(server.connections.is_empty());
    for method in [
        "turn/start",
        "turn/steer",
        "thread/settings/update",
        "thread/inject_items",
        "thread/goal/set",
    ] {
        let error = server
            .request(method, json!({"threadId":thread.id}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("read-only"), "{method}: {error}");
    }
    server
        .request(
            "thread/name/set",
            json!({"threadId":thread.id,"name":"Archived work"}),
        )
        .await
        .unwrap();
    server
        .request("thread/archive", json!({"threadId":thread.id}))
        .await
        .unwrap();
    let saved = server.store.load(&thread.id).unwrap();
    assert_eq!(saved.turns, thread.turns);
    assert!(saved.archived);
    let mut old_json = serde_json::to_value(&thread).unwrap();
    old_json.as_object_mut().unwrap().remove("runtime");
    assert!(
        serde_json::from_value::<Thread>(old_json)
            .unwrap()
            .executable()
            .is_err()
    );
}

#[tokio::test]
async fn native_thinking_streams_as_reasoning_without_empty_messages() {
    let (mut server, mut thread, mut output) = fixture();
    thread.turn = Some(json!({"id":"turn","status":"inProgress","items":[]}));
    thread.native_command_receipts.insert(
        "command".into(),
        json!({
            "trajectoryId":thread.id,"turnId":"turn","state":"running",
            "original":{"CommandLine":"pwd","Cwd":thread.cwd},
            "modified":{"CommandLine":"pwd"}
        }),
    );
    server.store.save(&thread).unwrap();
    let (commands, _rx) = mpsc::channel(4);
    server.connections.insert(
        thread.id.clone(),
        Connection {
            pid: None,
            generation: "test".into(),
            commands,
        },
    );
    let mut state = native_state::NativeState::new(&thread.id);
    let mut updates = vec![
        (0, json!({"status":2,"plannerResponse":{"toolCalls":[{}]}})),
        (
            0,
            json!({"status":2,"plannerResponse":{"thinking":"Inspect"}}),
        ),
        (
            0,
            json!({"status":2,"plannerResponse":{"thinking":"Inspect files"}}),
        ),
        // Reconnect replay must not append the same summary again.
        (
            0,
            json!({"status":2,"plannerResponse":{"thinking":"Inspect files"}}),
        ),
        // A non-prefix replacement must replace the summary, not concatenate it.
        (
            0,
            json!({"status":2,"plannerResponse":{"thinking":"Check files"}}),
        ),
        (
            0,
            json!({"status":3,"plannerResponse":{"thinking":"Check files","toolCalls":[{}]}}),
        ),
        (
            1,
            json!({"status":3,"runCommand":{"commandLine":"pwd","combinedOutput":"/workspace","exitCode":0}}),
        ),
        (2, json!({"status":3,"plannerResponse":{"toolCalls":[{}]}})),
        (
            3,
            json!({"status":3,"viewFile":{"filePath":"/workspace/file"}}),
        ),
        (
            4,
            json!({"status":2,"plannerResponse":{"thinking":"Summarize","response":"Found"}}),
        ),
        (
            4,
            json!({"status":3,"plannerResponse":{"thinking":"Summarize","response":"Found files"}}),
        ),
    ];
    for (index, step) in updates.drain(..) {
        let update = json!({"trajectoryId":"native","mainTrajectoryUpdate":{"stepsUpdate":{
            "totalLength":index+1,"indices":[index],"steps":[step]
        }}});
        for event in state
            .apply(&update)
            .into_iter()
            .filter(|e| e.get("stepUpdate").is_some())
        {
            server
                .event(RuntimeEvent {
                    id: thread.id.clone(),
                    generation: "test".into(),
                    event: Ok(event),
                })
                .await
                .unwrap();
        }
    }
    let saved = server.store.load(&thread.id).unwrap();
    let read = server
        .request("thread/read", json!({"threadId":thread.id}))
        .await
        .unwrap();
    let items = &read["thread"]["turns"][0]["items"];
    assert_eq!(
        items
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "reasoning",
            "commandExecution",
            "dynamicToolCall",
            "reasoning",
            "agentMessage"
        ]
    );
    assert_eq!(items[0]["summary"], json!(["Check files"]));
    assert_eq!(items[0]["content"], json!([]));
    assert_eq!(items[3]["summary"], json!(["Summarize"]));
    assert_eq!(items[4]["text"], "Found files");
    assert_eq!(items[0]["id"], format!("{}:0:reasoning", thread.id));
    assert_eq!(items[4]["id"], format!("{}:4", thread.id));
    assert_eq!(*items, saved.turn.as_ref().unwrap()["items"]);

    let mut events = Vec::new();
    while let Ok(event) = output.try_recv() {
        events.push(event);
    }
    let deltas: Vec<_> = events
        .iter()
        .filter(|e| e["method"] == "item/reasoning/summaryTextDelta")
        .collect();
    assert_eq!(deltas.len(), 3);
    assert_eq!(deltas[0]["params"]["delta"], "Inspect");
    assert_eq!(deltas[1]["params"]["delta"], " files");
    assert_eq!(deltas[2]["params"]["delta"], "Summarize");
    assert_eq!(deltas[0]["params"]["summaryIndex"], 0);
    assert_eq!(deltas[0]["params"]["itemId"], items[0]["id"]);
    assert!(!events.iter().any(
        |e| e["params"]["item"]["type"] == "agentMessage" && e["params"]["item"]["text"] == ""
    ));
    assert!(events.iter().any(|e|e["method"]=="item/completed" && e["params"]["item"]["id"]==items[0]["id"]));
    // Reconstruct exactly as a client would, including replacement snapshots.
    let mut reconstructed: Vec<Value> = Vec::new();
    for event in &events {
        let p = &event["params"];
        if event["method"] == "item/started" || event["method"] == "item/completed" {
            if let Some(previous) = reconstructed
                .iter_mut()
                .find(|i| i["id"] == p["item"]["id"])
            {
                *previous = p["item"].clone();
            } else {
                reconstructed.push(p["item"].clone());
            }
        } else if event["method"] == "item/reasoning/summaryTextDelta" {
            let item = reconstructed
                .iter_mut()
                .find(|i| i["id"] == p["itemId"])
                .unwrap();
            item["summary"] = json!([format!(
                "{}{}",
                item["summary"][0].as_str().unwrap_or(""),
                p["delta"].as_str().unwrap()
            )]);
        }
    }
    assert_eq!(json!(reconstructed), *items);
}

#[test]
fn native_message_history_projection_is_idempotent_and_preserves_real_answers() {
    let model = |id: &str, text: &str, thinking: &str| {
        json!({"id":id,"type":"agentMessage","text":text,"state":"STATE_DONE",
        "native":{"source":"SOURCE_MODEL","target":"TARGET_USER","thinking":thinking}})
    };
    let old = json!({"id":"turn","status":"completed","items":[
        model("a","","Inspect files"),
        {"id":"tool1","type":"commandExecution"},
        model("b","   ",""),
        {"id":"tool2","type":"dynamicToolCall"},
        model("c","Answer","Summary"),
        {"id":"question","type":"agentMessage","text":"","questions":[{"title":"Choose"}]},
        {"id":"ordinary","type":"agentMessage","text":"Original client message"}
    ]});
    let projected = compat::turn(&old);
    assert_eq!(
        projected["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "reasoning",
            "commandExecution",
            "dynamicToolCall",
            "reasoning",
            "agentMessage",
            "agentMessage",
            "agentMessage"
        ]
    );
    assert_eq!(compat::turn(&projected), projected);
    assert_eq!(projected["items"][4]["id"], "c");
    assert_eq!(projected["items"][4]["text"], "Answer");
    assert_eq!(projected["items"][5]["questions"][0]["title"], "Choose");
    assert_eq!(old["items"][0]["type"], "agentMessage");
}
