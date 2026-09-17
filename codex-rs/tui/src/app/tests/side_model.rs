use super::*;
use crate::collaboration_modes;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::ThreadSettingsUpdateParams;
use futures::SinkExt as _;
use futures::StreamExt as _;
use pretty_assertions::assert_eq;
use std::sync::Mutex;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn side_model_selection_updates_side_thread_without_changing_parent_or_defaults() {
    let mut app = make_test_app().await;
    app.config.model = Some("gpt-5.4".to_string());
    app.config.model_reasoning_effort = Some(ReasoningEffortConfig::Low);

    let parent_thread_id = ThreadId::new();
    let side_thread_id = ThreadId::new();
    let mut parent_session = test_thread_session(parent_thread_id, test_path_buf("/tmp/parent"));
    parent_session.model = "gpt-5.4".to_string();
    parent_session.reasoning_effort = Some(ReasoningEffortConfig::Low);
    let mut side_session = test_thread_session(side_thread_id, test_path_buf("/tmp/side"));
    side_session.model = "gpt-5.4".to_string();
    side_session.reasoning_effort = Some(ReasoningEffortConfig::Low);

    app.primary_thread_id = Some(parent_thread_id);
    app.primary_session_configured = Some(parent_session.clone());
    app.active_thread_id = Some(side_thread_id);
    app.chat_widget.handle_thread_session(side_session);
    app.side_threads
        .insert(side_thread_id, SideThreadState::new(parent_thread_id));
    app.sync_side_thread_ui();

    let plan_mask =
        collaboration_modes::mask_for_kind(&app.chat_widget.model_catalog(), ModeKind::Plan)
            .expect("Plan collaboration mask");
    app.chat_widget.set_collaboration_mask(plan_mask);
    app.chat_widget.set_model("gpt-5.5");
    app.set_active_thread_reasoning_without_default(Some(ReasoningEffortConfig::Ultra));

    assert_eq!(
        app.chat_widget.current_model(),
        "gpt-5.5",
        "the visible side thread should use the selected model"
    );
    assert_eq!(
        app.chat_widget.current_reasoning_effort(),
        Some(ReasoningEffortConfig::Ultra),
        "the visible side thread should use the selected reasoning effort"
    );
    assert_eq!(
        app.chat_widget
            .current_collaboration_mode()
            .reasoning_effort(),
        Some(ReasoningEffortConfig::Ultra),
        "the side thread's selected effort should survive toggling back to Default mode"
    );
    assert_eq!(
        app.active_thread_reasoning_setting_update_params(Some(ReasoningEffortConfig::Ultra))
            .expect("side reasoning update params")
            .thread_id,
        side_thread_id.to_string(),
        "side reasoning updates must target the side thread"
    );
    assert_eq!(app.config.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(
        app.config.model_reasoning_effort,
        Some(ReasoningEffortConfig::Low)
    );
    assert_eq!(
        app.primary_session_configured
            .as_ref()
            .map(|session| { (session.model.as_str(), session.reasoning_effort.clone(),) }),
        Some(("gpt-5.4", Some(ReasoningEffortConfig::Low))),
        "the cached parent thread must remain unchanged"
    );

    app.chat_widget.handle_thread_session(parent_session);
    assert_eq!(
        app.chat_widget.current_model(),
        "gpt-5.4",
        "returning to the parent thread must restore its model"
    );
    assert_eq!(
        app.chat_widget.current_reasoning_effort(),
        Some(ReasoningEffortConfig::Low),
        "returning to the parent thread must restore its reasoning effort"
    );
}

type RecordedRequests = Arc<Mutex<Vec<JSONRPCRequest>>>;
type RecordingSettingsServer = (AppServerSession, RecordedRequests, JoinHandle<Result<()>>);

async fn start_recording_settings_server() -> Result<RecordingSettingsServer> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let websocket_url = format!("ws://{}", listener.local_addr()?);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let request_sink = Arc::clone(&requests);
    let proxy = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut websocket = accept_async(stream).await?;
        while let Some(frame) = websocket.next().await {
            let frame = frame?;
            let Message::Text(text) = frame else {
                continue;
            };
            let message = serde_json::from_str::<JSONRPCMessage>(&text)?;
            let JSONRPCMessage::Request(request) = message else {
                continue;
            };
            let response = if request.method == "initialize" {
                JSONRPCMessage::Response(JSONRPCResponse {
                    id: request.id.clone(),
                    result: serde_json::json!({
                        "userAgent": "codex-tui-test",
                        "codexHome": std::env::temp_dir(),
                    }),
                })
            } else {
                request_sink
                    .lock()
                    .expect("request recorder lock")
                    .push(request.clone());
                JSONRPCMessage::Response(JSONRPCResponse {
                    id: request.id,
                    result: serde_json::json!({}),
                })
            };
            websocket
                .send(Message::Text(serde_json::to_string(&response)?.into()))
                .await?;
        }
        Ok(())
    });
    let client = crate::connect_remote_app_server(crate::RemoteAppServerEndpoint::WebSocket {
        websocket_url,
        auth_token: None,
    })
    .await?;
    let app_server =
        AppServerSession::new(client, crate::app_server_session::ThreadParamsMode::Remote);
    Ok((app_server, requests, proxy))
}

#[tokio::test]
async fn side_model_events_update_only_the_active_side_thread() -> Result<()> {
    let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
    app.config.model = Some("gpt-5.4".to_string());
    app.config.model_reasoning_effort = Some(ReasoningEffortConfig::Low);
    app.chat_widget.set_model("gpt-5.4");
    app.chat_widget
        .set_reasoning_effort(Some(ReasoningEffortConfig::Low));

    let (mut app_server, requests, proxy) = start_recording_settings_server().await?;
    let side_thread_id = ThreadId::new();
    let parent_thread_id = ThreadId::new();
    let mut parent_session = test_thread_session(parent_thread_id, test_path_buf("/tmp/parent"));
    parent_session.model = "gpt-5.4".to_string();
    parent_session.reasoning_effort = Some(ReasoningEffortConfig::Low);
    let mut side_session = test_thread_session(side_thread_id, test_path_buf("/tmp/side"));
    side_session.model = "gpt-5.4".to_string();
    side_session.reasoning_effort = Some(ReasoningEffortConfig::Low);

    app.primary_thread_id = Some(parent_thread_id);
    app.primary_session_configured = Some(parent_session.clone());
    app.active_thread_id = Some(side_thread_id);
    app.chat_widget.handle_thread_session(side_session);
    app.side_threads
        .insert(side_thread_id, SideThreadState::new(parent_thread_id));
    app.sync_side_thread_ui();
    let plan_mask =
        collaboration_modes::mask_for_kind(&app.chat_widget.model_catalog(), ModeKind::Plan)
            .expect("Plan collaboration mask");
    app.chat_widget.set_collaboration_mask(plan_mask);
    requests.lock().expect("request recorder lock").clear();

    let mut tui = crate::tui::test_support::make_test_tui()?;
    app.handle_event(
        &mut tui,
        &mut app_server,
        AppEvent::UpdateModel("gpt-5.5".to_string()),
    )
    .await?;
    app.handle_event(
        &mut tui,
        &mut app_server,
        AppEvent::UpdateReasoningEffort(Some(ReasoningEffortConfig::High)),
    )
    .await?;

    let updates = requests
        .lock()
        .expect("request recorder lock")
        .iter()
        .filter(|request| request.method == "thread/settings/update")
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(updates.len(), 2, "recorded requests: {updates:?}");
    let decode = |index: usize| {
        serde_json::from_value::<ThreadSettingsUpdateParams>(
            updates[index]
                .params
                .clone()
                .expect("thread settings update params"),
        )
        .expect("decoded thread settings update params")
    };
    let model_update = decode(0);
    let reasoning_update = decode(1);

    assert_eq!(model_update.thread_id, side_thread_id.to_string());
    assert_eq!(model_update.model, Some("gpt-5.5".to_string()));
    assert_eq!(reasoning_update.thread_id, side_thread_id.to_string());
    assert_eq!(reasoning_update.effort, Some(ReasoningEffortConfig::High));
    assert_eq!(
        reasoning_update
            .collaboration_mode
            .as_ref()
            .expect("side reasoning collaboration mode")
            .mode,
        ModeKind::Plan,
        "side reasoning updates must preserve the active Plan mode"
    );
    for update in [model_update, reasoning_update] {
        assert_eq!(update.permissions, None);
        assert_eq!(update.approval_policy, None);
        assert_eq!(update.approvals_reviewer, None);
    }

    requests.lock().expect("request recorder lock").clear();
    app.handle_event(
        &mut tui,
        &mut app_server,
        AppEvent::ApplyAdvancedReasoning {
            model: "gpt-5.5".to_string(),
            effort: ReasoningEffortConfig::Ultra,
        },
    )
    .await?;
    let advanced_updates = requests
        .lock()
        .expect("request recorder lock")
        .iter()
        .filter(|request| request.method == "thread/settings/update")
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        advanced_updates.len(),
        1,
        "recorded requests: {advanced_updates:?}"
    );
    let advanced_update = serde_json::from_value::<ThreadSettingsUpdateParams>(
        advanced_updates[0]
            .params
            .clone()
            .expect("thread settings update params"),
    )
    .expect("decoded advanced thread settings update params");
    assert_eq!(advanced_update.thread_id, side_thread_id.to_string());
    assert_eq!(advanced_update.model, Some("gpt-5.5".to_string()));
    assert_eq!(advanced_update.effort, Some(ReasoningEffortConfig::Ultra));
    assert_eq!(advanced_update.permissions, None);
    assert_eq!(advanced_update.approval_policy, None);
    assert_eq!(advanced_update.approvals_reviewer, None);

    assert_eq!(app.config.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(
        app.config.model_reasoning_effort,
        Some(ReasoningEffortConfig::Low)
    );
    assert_eq!(
        app.primary_session_configured
            .as_ref()
            .map(|session| { (session.model.as_str(), session.reasoning_effort.clone()) }),
        Some(("gpt-5.4", Some(ReasoningEffortConfig::Low))),
        "the cached parent thread must remain unchanged"
    );
    assert!(
        std::iter::from_fn(|| app_event_rx.try_recv().ok())
            .all(|event| !matches!(event, AppEvent::PersistModelSelection { .. })),
        "side model events must not persist default-model settings"
    );

    drop(app_server);
    proxy.abort();
    Ok(())
}
