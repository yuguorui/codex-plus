use crate::session::tests::make_session_and_context_with_rx;
use codex_features::Feature;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_login::CodexAuth;
use codex_model_provider_info::WireApi;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ConfigurationReasoning;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use pretty_assertions::assert_eq;
use std::sync::Arc;

async fn make_chat_session_and_context_with_rx() -> (
    Arc<crate::session::Session>,
    Arc<crate::session::TurnContext>,
    async_channel::Receiver<Event>,
) {
    crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
        CodexAuth::from_api_key("Test API Key"),
        Vec::new(),
        |config| config.model_provider.wire_api = WireApi::Chat,
    )
    .await
}

#[tokio::test]
async fn harness_authored_configuration_updates_preserve_metadata_and_resume() {
    let (session, turn_context, rx_event) = make_session_and_context_with_rx().await;
    assert!(!session.enabled(Feature::RetainClientDeveloperMessages));

    let mut expected = ResponseItemEnvelope {
        item: ResponseItem::ConfigurationUpdate {
            reasoning: ConfigurationReasoning {
                effort: ReasoningEffort::High,
            },
        },
        metadata: Some(CodexHarnessMetadata {
            harness_authored_configuration: true,
            ..Default::default()
        }),
    };
    session
        .record_annotated_conversation_items(
            &turn_context,
            turn_context.model_info(),
            vec![expected.clone()],
        )
        .await;

    expected.metadata.as_mut().unwrap().mcp_attribution = Some(
        session
            .services
            .executed_tool_calls
            .mcp_attribution_snapshot(),
    );

    let recorded = session.clone_history().await.into_annotated_items();
    assert_eq!(recorded, vec![expected.clone()]);
    let mut raw_items = Vec::new();
    while let Ok(event) = rx_event.try_recv() {
        if let EventMsg::RawResponseItem(event) = event.msg {
            raw_items.push(event.item);
        }
    }
    assert_eq!(raw_items, vec![expected.item]);

    let rollout_items = recorded
        .iter()
        .cloned()
        .map(RolloutItem::ResponseItem)
        .collect::<Vec<_>>();
    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;
    assert_eq!(reconstructed.history, recorded);
}

#[tokio::test]
async fn out_of_turn_user_context_closes_an_abandoned_call_first() {
    let (session, turn_context, _rx_event) = make_chat_session_and_context_with_rx().await;
    let call = ResponseItem::FunctionCall {
        encrypted_content: None,
        status: None,
        id: Some(codex_protocol::ResponseItemId::new("fc")),
        name: "shell".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: "abandoned-call".to_string(),
        encrypted_function_args: None,
        internal_chat_message_metadata_passthrough: None,
    };
    session
        .record_conversation_items(
            &turn_context,
            turn_context.model_info(),
            std::slice::from_ref(&call),
        )
        .await;
    assert!(
        session.clone_history().await.has_open_tool_calls(),
        "an out-of-turn call stays open until its output or a context write arrives"
    );

    let user_message = ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: Some(codex_protocol::ResponseItemId::new("msg")),
        role: "user".to_string(),
        content: vec![codex_protocol::models::ContentItem::InputText {
            text: "injected later".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    session
        .inject_no_new_turn(vec![user_message.clone()], Some(&turn_context))
        .await;

    let history = session.clone_history().await;
    assert!(!history.has_open_tool_calls());
    let items = history.raw_items().collect::<Vec<_>>();
    assert_eq!(items.len(), 3);
    assert!(matches!(
        items[0],
        ResponseItem::FunctionCall { call_id, .. } if call_id == "abandoned-call"
    ));
    assert!(
        matches!(
            items[1],
            ResponseItem::FunctionCallOutput {
                call_id: Some(call_id),
                output,
                ..
            } if call_id == "abandoned-call"
                && output.body.to_text().as_deref() == Some("aborted")
        ),
        "the abandoned call is closed with the canonical abort result"
    );
    assert!(matches!(
        items[2],
        ResponseItem::Message { role, content, .. }
            if role == "user"
                && matches!(
                    content.as_slice(),
                    [codex_protocol::models::ContentItem::InputText { text }]
                        if text == "injected later"
                )
    ));
}

#[tokio::test]
async fn user_prompt_closes_an_abandoned_call_before_emitting_it() {
    let (session, turn_context, rx_event) = make_chat_session_and_context_with_rx().await;
    let call = ResponseItem::FunctionCall {
        encrypted_content: None,
        status: None,
        id: Some(codex_protocol::ResponseItemId::new("fc")),
        name: "shell".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: "abandoned-before-prompt".to_string(),
        encrypted_function_args: None,
        internal_chat_message_metadata_passthrough: None,
    };
    session
        .record_conversation_items(
            &turn_context,
            turn_context.model_info(),
            std::slice::from_ref(&call),
        )
        .await;

    session
        .record_user_prompt_and_emit_turn_item(
            &turn_context,
            turn_context.model_info(),
            &[UserInput::Text {
                text: "new prompt".to_string(),
                text_elements: Vec::new(),
            }],
            /*client_id*/ None,
            crate::session::UserInputMetadata::default(),
            codex_thread_store::PersistContext::Standard,
        )
        .await;

    let mut observed = Vec::new();
    while let Ok(event) = rx_event.try_recv() {
        match event.msg {
            EventMsg::RawResponseItem(raw) => {
                if matches!(
                    raw.item,
                    ResponseItem::FunctionCallOutput {
                        call_id: Some(call_id),
                        ..
                    } if call_id == "abandoned-before-prompt"
                ) {
                    observed.push("aborted_output");
                }
            }
            EventMsg::ItemCompleted(item) if matches!(item.item, TurnItem::UserMessage(_)) => {
                observed.push("prompt");
            }
            _ => {}
        }
    }
    assert_eq!(
        observed,
        vec!["aborted_output", "prompt"],
        "the UI must see the closing output before the prompt cell it precedes"
    );

    let history = session.clone_history().await;
    assert!(!history.has_open_tool_calls());
    let items = history.raw_items().collect::<Vec<_>>();
    assert_eq!(items.len(), 3);
    assert!(matches!(
        items[0],
        ResponseItem::FunctionCall { call_id, .. } if call_id == "abandoned-before-prompt"
    ));
    assert!(matches!(
        items[1],
        ResponseItem::FunctionCallOutput {
            call_id: Some(call_id),
            ..
        } if call_id == "abandoned-before-prompt"
    ));
    assert!(matches!(
        items[2],
        ResponseItem::Message { role, .. } if role == "user"
    ));
}
