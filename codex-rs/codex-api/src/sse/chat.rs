use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::endpoint::chat::ChatToolNameMap;
use crate::error::ApiError;
use crate::rate_limits::parse_all_rate_limits;
use crate::telemetry::SseTelemetry;
use codex_client::ByteStream;
use codex_client::StreamResponse;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::timeout;
use tracing::debug;
use tracing::trace;

const OPENAI_MODEL_HEADER: &str = "openai-model";
const REQUEST_ID_HEADER: &str = "x-request-id";

pub fn spawn_chat_response_stream(
    stream_response: StreamResponse,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
    turn_state: Option<Arc<OnceLock<String>>>,
    tool_names: ChatToolNameMap,
) -> ResponseStream {
    let rate_limit_snapshots = parse_all_rate_limits(&stream_response.headers);
    let server_model = stream_response
        .headers
        .get(OPENAI_MODEL_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);
    let upstream_request_id = stream_response
        .headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    if let Some(turn_state) = turn_state.as_ref()
        && let Some(header_value) = stream_response
            .headers
            .get("x-codex-turn-state")
            .and_then(|v| v.to_str().ok())
    {
        let _ = turn_state.set(header_value.to_string());
    }
    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent, ApiError>>(1600);
    tokio::spawn(async move {
        if let Some(model) = server_model {
            let _ = tx_event.send(Ok(ResponseEvent::ServerModel(model))).await;
        }
        for snapshot in rate_limit_snapshots {
            let _ = tx_event.send(Ok(ResponseEvent::RateLimits(snapshot))).await;
        }
        process_chat_sse(
            stream_response.bytes,
            tx_event,
            idle_timeout,
            telemetry,
            tool_names,
        )
        .await;
    });

    ResponseStream {
        rx_event,
        upstream_request_id,
    }
}

#[derive(Debug, Deserialize)]
struct ChatChunk {
    id: Option<String>,
    choices: Vec<ChatChoice>,
    usage: Option<ChatUsage>,
}

#[derive(Debug, Deserialize)]
struct ChatErrorEvent {
    error: ChatError,
}

#[derive(Debug, Deserialize)]
struct ChatError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    delta: ChatDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ChatDelta {
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<ChatToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct ChatToolCallDelta {
    index: usize,
    id: Option<String>,
    function: Option<ChatFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct ChatFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatUsage {
    prompt_tokens: i64,
    completion_tokens: i64,
    total_tokens: i64,
    prompt_tokens_details: Option<PromptTokensDetails>,
    completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Debug, Deserialize)]
struct PromptTokensDetails {
    cached_tokens: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct CompletionTokensDetails {
    reasoning_tokens: Option<i64>,
}

impl From<ChatUsage> for TokenUsage {
    fn from(value: ChatUsage) -> Self {
        Self {
            input_tokens: value.prompt_tokens,
            cached_input_tokens: value
                .prompt_tokens_details
                .and_then(|details| details.cached_tokens)
                .unwrap_or_default(),
            output_tokens: value.completion_tokens,
            reasoning_output_tokens: value
                .completion_tokens_details
                .and_then(|details| details.reasoning_tokens)
                .unwrap_or_default(),
            total_tokens: value.total_tokens,
        }
    }
}

#[derive(Default)]
struct ChatStreamState {
    response_id: Option<String>,
    tool_names: ChatToolNameMap,
    content: String,
    content_started: bool,
    reasoning: String,
    reasoning_started: bool,
    tool_calls: BTreeMap<usize, ToolCallState>,
    finished: bool,
    token_usage: Option<TokenUsage>,
}

#[derive(Default)]
struct ToolCallState {
    id: Option<String>,
    namespace: Option<String>,
    name: Option<String>,
    arguments: String,
    emitted_argument_bytes: usize,
    started: bool,
}

impl ChatStreamState {
    async fn apply_chunk(
        &mut self,
        chunk: ChatChunk,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    ) -> Result<bool, ApiError> {
        if let Some(id) = chunk.id {
            self.response_id = Some(id);
        }
        if let Some(usage) = chunk.usage {
            self.token_usage = Some(usage.into());
        }

        let mut completed = false;
        for choice in chunk.choices {
            self.apply_delta(choice.delta, tx_event).await;
            if let Some(finish_reason) = choice.finish_reason {
                match finish_reason.as_str() {
                    "stop" | "tool_calls" => {}
                    "length" | "content_filter" => {
                        return Err(ApiError::Stream(format!(
                            "chat completion finished with {finish_reason}"
                        )));
                    }
                    _ => {
                        return Err(ApiError::Stream(format!(
                            "chat completion finished with unknown finish reason {finish_reason}"
                        )));
                    }
                }
                self.finished = true;
                self.finish_items(&finish_reason, tx_event).await?;
                completed = finish_reason != "tool_calls";
            }
        }
        Ok(completed && self.token_usage.is_some())
    }

    async fn apply_delta(
        &mut self,
        delta: ChatDelta,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    ) {
        if let Some(content) = delta.content
            && !content.is_empty()
        {
            if !self.content_started {
                self.content_started = true;
                let _ = tx_event
                    .send(Ok(ResponseEvent::OutputItemAdded(ResponseItem::Message {
                        id: None,
                        role: "assistant".to_string(),
                        content: Vec::new(),
                        phase: None,
                    })))
                    .await;
            }
            self.content.push_str(&content);
            let _ = tx_event
                .send(Ok(ResponseEvent::OutputTextDelta(content)))
                .await;
        }

        if let Some(reasoning_content) = delta.reasoning_content
            && !reasoning_content.is_empty()
        {
            if !self.reasoning_started {
                self.reasoning_started = true;
                let _ = tx_event
                    .send(Ok(ResponseEvent::OutputItemAdded(
                        ResponseItem::Reasoning {
                            id: reasoning_id(),
                            summary: Vec::new(),
                            content: Some(Vec::new()),
                            encrypted_content: None,
                        },
                    )))
                    .await;
            }
            self.reasoning.push_str(&reasoning_content);
            let _ = tx_event
                .send(Ok(ResponseEvent::ReasoningContentDelta {
                    delta: reasoning_content,
                    content_index: 0,
                }))
                .await;
        }

        if let Some(tool_calls) = delta.tool_calls {
            for tool_call in tool_calls {
                let state = self.tool_calls.entry(tool_call.index).or_default();
                if let Some(id) = tool_call.id {
                    state.id = Some(id);
                }
                if let Some(function) = tool_call.function {
                    if let Some(name) = function.name
                        && !name.is_empty()
                    {
                        if let Some(tool_name) = self.tool_names.get(&name) {
                            state.namespace = tool_name.namespace.clone();
                            state.name = Some(tool_name.name.clone());
                        } else {
                            state.namespace = None;
                            state.name = Some(name);
                        }
                    }
                    if let Some(arguments) = function.arguments
                        && !arguments.is_empty()
                    {
                        state.arguments.push_str(&arguments);
                    }
                    if !state.started
                        && let Some(name) = state.name.clone()
                    {
                        state.started = true;
                        let call_id = state.id.clone().unwrap_or_default();
                        let _ = tx_event
                            .send(Ok(ResponseEvent::OutputItemAdded(
                                ResponseItem::FunctionCall {
                                    id: None,
                                    name,
                                    namespace: state.namespace.clone(),
                                    arguments: String::new(),
                                    call_id,
                                },
                            )))
                            .await;
                    }
                    if state.started && state.emitted_argument_bytes < state.arguments.len() {
                        let call_id = state.id.clone();
                        let item_id = call_id
                            .clone()
                            .unwrap_or_else(|| format!("chat_tool_{}", tool_call.index));
                        let delta = state.arguments[state.emitted_argument_bytes..].to_string();
                        state.emitted_argument_bytes = state.arguments.len();
                        let _ = tx_event
                            .send(Ok(ResponseEvent::ToolCallInputDelta {
                                item_id,
                                call_id,
                                delta,
                            }))
                            .await;
                    }
                }
            }
        }
    }

    async fn finish_items(
        &mut self,
        finish_reason: &str,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    ) -> Result<(), ApiError> {
        if self.reasoning_started {
            let _ = tx_event
                .send(Ok(ResponseEvent::OutputItemDone(ResponseItem::Reasoning {
                    id: reasoning_id(),
                    summary: Vec::new(),
                    content: Some(vec![ReasoningItemContent::ReasoningText {
                        text: std::mem::take(&mut self.reasoning),
                    }]),
                    encrypted_content: None,
                })))
                .await;
            self.reasoning_started = false;
        }

        if !self.content.is_empty() {
            let _ = tx_event
                .send(Ok(ResponseEvent::OutputItemDone(ResponseItem::Message {
                    id: None,
                    role: "assistant".to_string(),
                    content: vec![ContentItem::OutputText {
                        text: std::mem::take(&mut self.content),
                    }],
                    phase: None,
                })))
                .await;
            self.content_started = false;
        }

        if finish_reason == "tool_calls" {
            for (_, tool_call) in std::mem::take(&mut self.tool_calls) {
                let Some(name) = tool_call.name else {
                    return Err(ApiError::Stream(
                        "chat tool call missing function name".to_string(),
                    ));
                };
                let call_id = tool_call.id.unwrap_or_default();
                let _ = tx_event
                    .send(Ok(ResponseEvent::OutputItemDone(
                        ResponseItem::FunctionCall {
                            id: None,
                            name,
                            namespace: tool_call.namespace,
                            arguments: tool_call.arguments,
                            call_id,
                        },
                    )))
                    .await;
            }
        }
        Ok(())
    }

    async fn complete(&mut self, tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>) {
        let _ = tx_event
            .send(Ok(ResponseEvent::Completed {
                response_id: self
                    .response_id
                    .clone()
                    .unwrap_or_else(|| "chatcmpl".to_string()),
                token_usage: self.token_usage.take(),
                end_turn: None,
            }))
            .await;
    }

    async fn finish_from_done(
        &mut self,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    ) -> Result<(), ApiError> {
        if self.finished {
            return Ok(());
        }
        let finish_reason = if self.tool_calls.is_empty() {
            "stop"
        } else {
            "tool_calls"
        };
        self.finished = true;
        self.finish_items(finish_reason, tx_event).await
    }
}

pub async fn process_chat_sse(
    stream: ByteStream,
    tx_event: mpsc::Sender<Result<ResponseEvent, ApiError>>,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
    tool_names: ChatToolNameMap,
) {
    let mut stream = stream.eventsource();
    let mut state = ChatStreamState {
        tool_names,
        ..Default::default()
    };
    let _ = tx_event.send(Ok(ResponseEvent::Created {})).await;

    loop {
        let start = Instant::now();
        let response = timeout(idle_timeout, stream.next()).await;
        if let Some(t) = telemetry.as_ref() {
            t.on_sse_poll(&response, start.elapsed());
        }
        let sse = match response {
            Ok(Some(Ok(sse))) => sse,
            Ok(Some(Err(e))) => {
                debug!("SSE Error: {e:#}");
                let _ = tx_event.send(Err(ApiError::Stream(e.to_string()))).await;
                return;
            }
            Ok(None) => {
                if state.finished {
                    state.complete(&tx_event).await;
                } else {
                    let _ = tx_event
                        .send(Err(ApiError::Stream(
                            "stream closed before chat completion finished".into(),
                        )))
                        .await;
                }
                return;
            }
            Err(_) => {
                let _ = tx_event
                    .send(Err(ApiError::Stream("idle timeout waiting for SSE".into())))
                    .await;
                return;
            }
        };

        trace!("SSE event: {}", &sse.data);
        if sse.data.trim() == "[DONE]" {
            match state.finish_from_done(&tx_event).await {
                Ok(()) => state.complete(&tx_event).await,
                Err(err) => {
                    let _ = tx_event.send(Err(err)).await;
                }
            }
            return;
        }

        let chunk = match serde_json::from_str::<ChatChunk>(&sse.data) {
            Ok(chunk) => chunk,
            Err(e) => {
                if let Ok(error_event) = serde_json::from_str::<ChatErrorEvent>(&sse.data) {
                    let _ = tx_event
                        .send(Err(ApiError::Stream(error_event.error.message)))
                        .await;
                    return;
                }
                debug!("Failed to parse chat SSE event: {e}, data: {}", &sse.data);
                continue;
            }
        };

        match state.apply_chunk(chunk, &tx_event).await {
            Ok(true) => {
                state.complete(&tx_event).await;
                return;
            }
            Ok(false) => {}
            Err(err) => {
                let _ = tx_event.send(Err(err)).await;
                return;
            }
        }
    }
}

fn reasoning_id() -> String {
    "chat_reasoning_0".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::chat::ChatToolName;
    use assert_matches::assert_matches;
    use bytes::Bytes;
    use codex_client::TransportError;
    use futures::stream;
    use serde_json::Value;
    use serde_json::json;

    async fn run_chat_sse_results(events: Vec<Value>) -> Vec<Result<ResponseEvent, ApiError>> {
        run_chat_sse_results_with_tool_names(events, ChatToolNameMap::new()).await
    }

    async fn run_chat_sse_results_with_tool_names(
        events: Vec<Value>,
        tool_names: ChatToolNameMap,
    ) -> Vec<Result<ResponseEvent, ApiError>> {
        let mut body = String::new();
        for event in events {
            body.push_str(&format!("data: {event}\n\n"));
        }
        body.push_str("data: [DONE]\n\n");

        let stream = stream::iter(vec![Ok::<Bytes, TransportError>(Bytes::from(body))]);
        let (tx, mut rx) = mpsc::channel::<Result<ResponseEvent, ApiError>>(16);
        tokio::spawn(process_chat_sse(
            Box::pin(stream),
            tx,
            Duration::from_millis(1000),
            /*telemetry*/ None,
            tool_names,
        ));

        let mut out = Vec::new();
        while let Some(event) = rx.recv().await {
            out.push(event);
        }
        out
    }

    async fn run_chat_sse(events: Vec<Value>) -> Vec<ResponseEvent> {
        run_chat_sse_results(events)
            .await
            .into_iter()
            .map(|event| event.expect("event should parse"))
            .collect()
    }

    #[tokio::test]
    async fn parses_text_delta_and_usage() {
        let events = run_chat_sse(vec![
            json!({
                "id": "chatcmpl-1",
                "choices": [{"delta": {"content": "hel"}, "finish_reason": null}],
            }),
            json!({
                "id": "chatcmpl-1",
                "choices": [{"delta": {"content": "lo"}, "finish_reason": "stop"}],
                "usage": {
                    "prompt_tokens": 3,
                    "completion_tokens": 2,
                    "total_tokens": 5,
                    "prompt_tokens_details": {"cached_tokens": 1},
                    "completion_tokens_details": {"reasoning_tokens": 0}
                }
            }),
        ])
        .await;

        assert_matches!(events[0], ResponseEvent::Created);
        assert_matches!(
            events[1],
            ResponseEvent::OutputItemAdded(ResponseItem::Message { .. })
        );
        assert_matches!(&events[2], ResponseEvent::OutputTextDelta(delta) if delta == "hel");
        assert_matches!(&events[3], ResponseEvent::OutputTextDelta(delta) if delta == "lo");
        assert_matches!(
            &events[4],
            ResponseEvent::OutputItemDone(ResponseItem::Message { role, content, .. })
                if role == "assistant"
                    && content == &vec![ContentItem::OutputText { text: "hello".to_string() }]
        );
        assert_matches!(
            &events[5],
            ResponseEvent::Completed {
                response_id,
                token_usage: Some(TokenUsage {
                    input_tokens: 3,
                    cached_input_tokens: 1,
                    output_tokens: 2,
                    reasoning_output_tokens: 0,
                    total_tokens: 5,
                }),
                ..
            } if response_id == "chatcmpl-1"
        );
    }

    #[tokio::test]
    async fn parses_reasoning_delta() {
        let events = run_chat_sse(vec![json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": {"reasoning_content": "think"},
                "finish_reason": "stop"
            }],
        })])
        .await;

        assert_matches!(
            events[1],
            ResponseEvent::OutputItemAdded(ResponseItem::Reasoning { .. })
        );
        assert_matches!(
            &events[2],
            ResponseEvent::ReasoningContentDelta { delta, content_index: 0 }
                if delta == "think"
        );
        assert_matches!(
            &events[3],
            ResponseEvent::OutputItemDone(ResponseItem::Reasoning { content: Some(content), .. })
                if content == &vec![ReasoningItemContent::ReasoningText {
                    text: "think".to_string()
                }]
        );
    }

    #[tokio::test]
    async fn parses_streaming_tool_call() {
        let events = run_chat_sse(vec![
            json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call-1",
                            "function": {"name": "shell", "arguments": "{\"cmd\":\""}
                        }]
                    },
                    "finish_reason": null
                }]
            }),
            json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": {"arguments": "ls\"}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }),
        ])
        .await;

        assert_matches!(
            events[1],
            ResponseEvent::OutputItemAdded(ResponseItem::FunctionCall { .. })
        );
        assert_matches!(
            &events[2],
            ResponseEvent::ToolCallInputDelta { item_id, call_id: Some(call_id), delta }
                if item_id == "call-1" && call_id == "call-1" && delta == "{\"cmd\":\""
        );
        assert_matches!(
            &events[3],
            ResponseEvent::ToolCallInputDelta { item_id, call_id: Some(call_id), delta }
                if item_id == "call-1" && call_id == "call-1" && delta == "ls\"}"
        );
        assert_matches!(
            &events[4],
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            }) if name == "shell" && arguments == "{\"cmd\":\"ls\"}" && call_id == "call-1"
        );
    }

    #[tokio::test]
    async fn buffers_tool_call_arguments_until_name_arrives() {
        let events = run_chat_sse(vec![
            json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call-1",
                            "function": {"arguments": "{\"cmd\":\""}
                        }]
                    },
                    "finish_reason": null
                }]
            }),
            json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": {"name": "shell", "arguments": "ls\"}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }),
        ])
        .await;

        assert_matches!(
            events[1],
            ResponseEvent::OutputItemAdded(ResponseItem::FunctionCall { .. })
        );
        assert_matches!(
            &events[2],
            ResponseEvent::ToolCallInputDelta { item_id, call_id: Some(call_id), delta }
                if item_id == "call-1" && call_id == "call-1" && delta == "{\"cmd\":\"ls\"}"
        );
        assert_matches!(
            &events[3],
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            }) if name == "shell" && arguments == "{\"cmd\":\"ls\"}" && call_id == "call-1"
        );
    }

    #[tokio::test]
    async fn missing_tool_call_name_errors() {
        let events = run_chat_sse_results(vec![json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call-1",
                        "function": {"arguments": "{\"cmd\":\"ls\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })])
        .await;

        assert_matches!(
            events.last(),
            Some(Err(ApiError::Stream(message)))
                if message == "chat tool call missing function name"
        );
    }

    #[tokio::test]
    async fn parses_namespaced_streaming_tool_call() {
        let events = run_chat_sse_results_with_tool_names(
            vec![json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call-1",
                            "function": {
                                "name": "mcp__calendar__lookup_order",
                                "arguments": "{\"id\":\"ord_123\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            })],
            ChatToolNameMap::from([(
                "mcp__calendar__lookup_order".to_string(),
                ChatToolName {
                    namespace: Some("mcp__calendar__".to_string()),
                    name: "lookup_order".to_string(),
                },
            )]),
        )
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect::<Vec<_>>();

        assert_matches!(
            &events[3],
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                namespace,
                name,
                arguments,
                call_id,
                ..
            }) if namespace.as_deref() == Some("mcp__calendar__")
                && name == "lookup_order"
                && arguments == "{\"id\":\"ord_123\"}"
                && call_id == "call-1"
        );
    }

    #[tokio::test]
    async fn parses_parallel_streaming_tool_calls() {
        let events = run_chat_sse(vec![json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": {
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call-alpha",
                            "function": {"name": "shell", "arguments": "{\"cmd\":\"printf alpha\"}"}
                        },
                        {
                            "index": 1,
                            "id": "call-beta",
                            "function": {"name": "shell", "arguments": "{\"cmd\":\"printf beta\"}"}
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }]
        })])
        .await;

        assert_matches!(
            &events[2],
            ResponseEvent::ToolCallInputDelta { item_id, call_id: Some(call_id), delta }
                if item_id == "call-alpha" && call_id == "call-alpha" && delta == "{\"cmd\":\"printf alpha\"}"
        );
        assert_matches!(
            &events[4],
            ResponseEvent::ToolCallInputDelta { item_id, call_id: Some(call_id), delta }
                if item_id == "call-beta" && call_id == "call-beta" && delta == "{\"cmd\":\"printf beta\"}"
        );
        assert_matches!(
            &events[5],
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            }) if name == "shell"
                && arguments == "{\"cmd\":\"printf alpha\"}"
                && call_id == "call-alpha"
        );
        assert_matches!(
            &events[6],
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            }) if name == "shell"
                && arguments == "{\"cmd\":\"printf beta\"}"
                && call_id == "call-beta"
        );
    }

    #[tokio::test]
    async fn parses_usage_only_final_chunk_details() {
        let events = run_chat_sse(vec![
            json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "delta": {"content": "done"},
                    "finish_reason": "stop"
                }]
            }),
            json!({
                "id": "chatcmpl-1",
                "choices": [],
                "usage": {
                    "prompt_tokens": 11,
                    "completion_tokens": 7,
                    "total_tokens": 18,
                    "prompt_tokens_details": {"cached_tokens": 4},
                    "completion_tokens_details": {"reasoning_tokens": 3}
                }
            }),
        ])
        .await;

        assert_matches!(
            events.last(),
            Some(ResponseEvent::Completed {
                token_usage: Some(TokenUsage {
                    input_tokens: 11,
                    cached_input_tokens: 4,
                    output_tokens: 7,
                    reasoning_output_tokens: 3,
                    total_tokens: 18,
                }),
                ..
            })
        );
    }

    #[tokio::test]
    async fn done_without_finish_reason_flushes_text_item() {
        let events = run_chat_sse(vec![json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": {"content": "done"},
                "finish_reason": null
            }]
        })])
        .await;

        assert_matches!(
            &events[3],
            ResponseEvent::OutputItemDone(ResponseItem::Message { content, .. })
                if content == &vec![ContentItem::OutputText {
                    text: "done".to_string()
                }]
        );
        assert_matches!(&events[4], ResponseEvent::Completed { .. });
    }

    #[tokio::test]
    async fn done_without_finish_reason_flushes_tool_call() {
        let events = run_chat_sse(vec![json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call-1",
                        "function": {"name": "shell", "arguments": "{\"cmd\":\"ls\"}"}
                    }]
                },
                "finish_reason": null
            }]
        })])
        .await;

        assert_matches!(
            &events[3],
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            }) if name == "shell" && arguments == "{\"cmd\":\"ls\"}" && call_id == "call-1"
        );
        assert_matches!(&events[4], ResponseEvent::Completed { .. });
    }

    #[tokio::test]
    async fn length_finish_reason_errors() {
        let events = run_chat_sse_results(vec![json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": {"content": "partial"},
                "finish_reason": "length"
            }]
        })])
        .await;

        assert_matches!(
            events.last(),
            Some(Err(ApiError::Stream(message)))
                if message == "chat completion finished with length"
        );
    }

    #[tokio::test]
    async fn content_filter_finish_reason_errors() {
        let events = run_chat_sse_results(vec![json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": {"content": "partial"},
                "finish_reason": "content_filter"
            }]
        })])
        .await;

        assert_matches!(
            events.last(),
            Some(Err(ApiError::Stream(message)))
                if message == "chat completion finished with content_filter"
        );
    }

    #[tokio::test]
    async fn unknown_finish_reason_errors() {
        let events = run_chat_sse_results(vec![json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": {},
                "finish_reason": "new_reason"
            }]
        })])
        .await;

        assert_matches!(
            events.last(),
            Some(Err(ApiError::Stream(message)))
                if message == "chat completion finished with unknown finish reason new_reason"
        );
    }
}
