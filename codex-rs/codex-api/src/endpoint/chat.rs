use crate::auth::SharedAuthProvider;
use crate::common::ResponseStream;
use crate::common::ResponsesApiRequest;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::requests::Compression;
use crate::requests::headers::build_session_headers;
use crate::requests::headers::insert_header;
use crate::requests::headers::subagent_header;
use crate::sse::spawn_chat_response_stream;
use crate::telemetry::SseTelemetry;
use codex_client::HttpTransport;
use codex_client::RequestCompression;
use codex_client::RequestTelemetry;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::OnceLock;
use tracing::instrument;

pub struct ChatClient<T: HttpTransport> {
    session: EndpointSession<T>,
    sse_telemetry: Option<Arc<dyn SseTelemetry>>,
}

#[derive(Default)]
pub struct ChatOptions {
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub session_source: Option<SessionSource>,
    pub extra_headers: HeaderMap,
    pub compression: Compression,
    pub turn_state: Option<Arc<OnceLock<String>>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChatToolName {
    pub(crate) namespace: Option<String>,
    pub(crate) name: String,
}

pub(crate) type ChatToolNameMap = HashMap<String, ChatToolName>;

#[derive(Debug)]
struct ChatRequestBody {
    body: Value,
    tool_names: ChatToolNameMap,
}

#[derive(Debug, Serialize)]
struct ChatCompletionsRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
    stream_options: ChatStreamOptions,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ChatTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ChatStreamOptions {
    include_usage: bool,
}

#[derive(Debug, Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
enum ChatMessage {
    System {
        content: String,
    },
    User {
        content: ChatContent,
    },
    Assistant {
        content: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ChatToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ChatContent {
    Text(String),
    Parts(Vec<ChatContentPart>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ChatContentPart {
    Text { text: String },
    ImageUrl { image_url: ChatImageUrl },
}

#[derive(Debug, Serialize)]
struct ChatImageUrl {
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<ImageDetail>,
}

#[derive(Debug, Serialize)]
struct ChatToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: ChatToolCallKind,
    function: ChatToolCallFunction,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ChatToolCallKind {
    Function,
}

#[derive(Debug, Serialize)]
struct ChatToolCallFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct ChatTool {
    #[serde(rename = "type")]
    kind: ChatToolKind,
    function: ChatFunctionTool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ChatToolKind {
    Function,
}

#[derive(Debug, Serialize)]
struct ChatFunctionTool {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    strict: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsesTool {
    Function(ResponsesFunctionTool),
    Namespace(ResponsesNamespaceTool),
    Custom,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct ResponsesFunctionTool {
    name: String,
    description: Option<String>,
    parameters: Option<Value>,
    strict: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ResponsesNamespaceTool {
    name: String,
    tools: Vec<ResponsesFunctionTool>,
}

impl<T: HttpTransport> ChatClient<T> {
    pub fn new(transport: T, provider: Provider, auth: SharedAuthProvider) -> Self {
        Self {
            session: EndpointSession::new(transport, provider, auth),
            sse_telemetry: None,
        }
    }

    pub fn with_telemetry(
        self,
        request: Option<Arc<dyn RequestTelemetry>>,
        sse: Option<Arc<dyn SseTelemetry>>,
    ) -> Self {
        Self {
            session: self.session.with_request_telemetry(request),
            sse_telemetry: sse,
        }
    }

    #[instrument(
        name = "chat.stream_request",
        level = "info",
        skip_all,
        fields(
            transport = "chat_http",
            http.method = "POST",
            api.path = "chat/completions"
        )
    )]
    pub async fn stream_request(
        &self,
        request: ResponsesApiRequest,
        options: ChatOptions,
    ) -> Result<ResponseStream, ApiError> {
        let ChatOptions {
            session_id,
            thread_id,
            session_source,
            extra_headers,
            compression,
            turn_state,
        } = options;

        let mut request_body = chat_body_from_responses_request(request)?;
        merge_extra_body(&mut request_body.body, &self.session.provider().extra_body)?;

        let mut headers = extra_headers;
        if let Some(ref thread_id) = thread_id {
            insert_header(&mut headers, "x-client-request-id", thread_id);
        }
        headers.extend(build_session_headers(session_id, thread_id));
        if let Some(subagent) = subagent_header(&session_source) {
            insert_header(&mut headers, "x-openai-subagent", &subagent);
        }

        self.stream(
            request_body.body,
            headers,
            compression,
            turn_state,
            request_body.tool_names,
        )
        .await
    }

    fn path() -> &'static str {
        "chat/completions"
    }

    #[instrument(
        name = "chat.stream",
        level = "info",
        skip_all,
        fields(
            transport = "chat_http",
            http.method = "POST",
            api.path = "chat/completions",
            turn.has_state = turn_state.is_some()
        )
    )]
    async fn stream(
        &self,
        body: Value,
        extra_headers: HeaderMap,
        compression: Compression,
        turn_state: Option<Arc<OnceLock<String>>>,
        tool_names: ChatToolNameMap,
    ) -> Result<ResponseStream, ApiError> {
        let request_compression = match compression {
            Compression::None => RequestCompression::None,
            Compression::Zstd => RequestCompression::Zstd,
        };

        let stream_response = self
            .session
            .stream_with(
                Method::POST,
                Self::path(),
                extra_headers,
                Some(body),
                |req| {
                    req.headers.insert(
                        http::header::ACCEPT,
                        HeaderValue::from_static("text/event-stream"),
                    );
                    req.compression = request_compression;
                },
            )
            .await?;

        Ok(spawn_chat_response_stream(
            stream_response,
            self.session.provider().stream_idle_timeout,
            self.sse_telemetry.clone(),
            turn_state,
            tool_names,
        ))
    }
}

fn merge_extra_body(
    body: &mut Value,
    extra_body: &std::collections::HashMap<String, Value>,
) -> Result<(), ApiError> {
    if extra_body.is_empty() {
        return Ok(());
    }
    let Some(body) = body.as_object_mut() else {
        return Ok(());
    };
    for (key, value) in extra_body {
        if key == "stream_options" {
            let Some(base_stream_options) = body
                .get_mut("stream_options")
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            let Some(extra_stream_options) = value.as_object() else {
                return Err(ApiError::Stream(
                    "extra_body.stream_options must be an object".to_string(),
                ));
            };
            for (option_key, option_value) in extra_stream_options {
                base_stream_options.insert(option_key.clone(), option_value.clone());
            }
            base_stream_options.insert("include_usage".to_string(), Value::Bool(true));
        } else {
            body.insert(key.clone(), value.clone());
        }
    }
    Ok(())
}

fn chat_body_from_responses_request(
    request: ResponsesApiRequest,
) -> Result<ChatRequestBody, ApiError> {
    let mut tool_names = ChatToolNameMap::new();
    let tools = chat_tools_from_responses_tools(request.tools, &mut tool_names)?;
    let has_tools = !tools.is_empty();
    let request = ChatCompletionsRequest {
        model: request.model,
        messages: chat_messages_from_items(&request.instructions, request.input)?,
        stream: true,
        stream_options: ChatStreamOptions {
            include_usage: true,
        },
        tools,
        tool_choice: has_tools.then_some(request.tool_choice),
        parallel_tool_calls: has_tools.then_some(request.parallel_tool_calls),
    };
    let body = serde_json::to_value(request)
        .map_err(|err| ApiError::Stream(format!("failed to encode chat request: {err}")))?;
    Ok(ChatRequestBody { body, tool_names })
}

fn chat_messages_from_items(
    instructions: &str,
    items: Vec<ResponseItem>,
) -> Result<Vec<ChatMessage>, ApiError> {
    let mut messages = Vec::new();
    if !instructions.is_empty() {
        messages.push(ChatMessage::System {
            content: instructions.to_string(),
        });
    }

    let mut pending_tool_calls = Vec::new();
    for item in items {
        match item {
            ResponseItem::Message { role, content, .. } => {
                flush_pending_tool_calls(&mut messages, &mut pending_tool_calls);
                let role = if role == "developer" {
                    "system".to_string()
                } else {
                    role
                };
                match role.as_str() {
                    "system" => {
                        messages.push(ChatMessage::System {
                            content: chat_content_from_items(content).into_text(),
                        });
                    }
                    "assistant" => {
                        messages.push(ChatMessage::Assistant {
                            content: Some(chat_content_from_items(content).into_text()),
                            tool_calls: Vec::new(),
                        });
                    }
                    _ => {
                        messages.push(ChatMessage::User {
                            content: chat_content_from_items(content),
                        });
                    }
                }
            }
            ResponseItem::FunctionCallOutput { call_id, output }
            | ResponseItem::CustomToolCallOutput {
                call_id, output, ..
            } => {
                flush_pending_tool_calls(&mut messages, &mut pending_tool_calls);
                messages.push(ChatMessage::Tool {
                    tool_call_id: call_id,
                    content: function_output_to_chat_content(output.body),
                });
            }
            ResponseItem::FunctionCall {
                name,
                namespace,
                arguments,
                call_id,
                ..
            } => {
                let name = chat_function_name(namespace.as_deref(), &name);
                pending_tool_calls.push(ChatToolCall {
                    id: call_id,
                    kind: ChatToolCallKind::Function,
                    function: ChatToolCallFunction { name, arguments },
                });
            }
            ResponseItem::CustomToolCall {
                call_id,
                name,
                input,
                ..
            } => {
                pending_tool_calls.push(ChatToolCall {
                    id: call_id,
                    kind: ChatToolCallKind::Function,
                    function: ChatToolCallFunction {
                        name,
                        arguments: input,
                    },
                });
            }
            ResponseItem::Compaction { encrypted_content } => {
                flush_pending_tool_calls(&mut messages, &mut pending_tool_calls);
                messages.push(ChatMessage::System {
                    content: encrypted_content,
                });
            }
            ResponseItem::ContextCompaction {
                encrypted_content: Some(encrypted_content),
            } => {
                flush_pending_tool_calls(&mut messages, &mut pending_tool_calls);
                messages.push(ChatMessage::System {
                    content: encrypted_content,
                });
            }
            _ => {
                flush_pending_tool_calls(&mut messages, &mut pending_tool_calls);
            }
        }
    }
    flush_pending_tool_calls(&mut messages, &mut pending_tool_calls);
    validate_tool_call_history(&messages)?;
    Ok(messages)
}

fn validate_tool_call_history(messages: &[ChatMessage]) -> Result<(), ApiError> {
    let mut seen_tool_calls = HashSet::new();
    let mut pending_tool_calls: Vec<String> = Vec::new();
    let mut pending_assistant_index: Option<usize> = None;

    for (message_index, message) in messages.iter().enumerate() {
        match message {
            ChatMessage::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                if !pending_tool_calls.is_empty() {
                    return Err(invalid_chat_tool_history(format!(
                        "assistant message at index {assistant_index} has tool calls without matching tool outputs before message index {message_index}",
                        assistant_index = pending_assistant_index.unwrap_or(message_index)
                    )));
                }

                for tool_call in tool_calls {
                    if tool_call.id.is_empty() {
                        return Err(invalid_chat_tool_history(format!(
                            "assistant tool call at message index {message_index} has an empty id"
                        )));
                    }
                    if !seen_tool_calls.insert(tool_call.id.clone()) {
                        return Err(invalid_chat_tool_history(format!(
                            "duplicate assistant tool call id '{}' at message index {message_index}",
                            tool_call.id
                        )));
                    }
                    pending_tool_calls.push(tool_call.id.clone());
                }
                pending_assistant_index = Some(message_index);
            }
            ChatMessage::Tool { tool_call_id, .. } => {
                if tool_call_id.is_empty() {
                    return Err(invalid_chat_tool_history(format!(
                        "tool message at index {message_index} has an empty tool_call_id"
                    )));
                }

                if pending_tool_calls.is_empty() {
                    return Err(invalid_chat_tool_history(format!(
                        "tool message at index {message_index} references '{tool_call_id}' without a preceding assistant tool call"
                    )));
                }

                let Some(position) = pending_tool_calls
                    .iter()
                    .position(|pending_id| pending_id == tool_call_id)
                else {
                    return Err(invalid_chat_tool_history(format!(
                        "tool message at index {message_index} references '{tool_call_id}' but expected one of [{}]",
                        pending_tool_calls.join(", ")
                    )));
                };

                pending_tool_calls.remove(position);
                if pending_tool_calls.is_empty() {
                    pending_assistant_index = None;
                }
            }
            _ => {
                if !pending_tool_calls.is_empty() {
                    return Err(invalid_chat_tool_history(format!(
                        "assistant message at index {assistant_index} has tool calls without matching tool outputs before message index {message_index}",
                        assistant_index = pending_assistant_index.unwrap_or(message_index)
                    )));
                }
            }
        }
    }

    if !pending_tool_calls.is_empty() {
        return Err(invalid_chat_tool_history(format!(
            "assistant message at index {assistant_index} has tool calls without matching tool outputs",
            assistant_index = pending_assistant_index.unwrap_or(messages.len())
        )));
    }

    Ok(())
}

fn invalid_chat_tool_history(message: String) -> ApiError {
    ApiError::InvalidRequest {
        message: format!("invalid chat tool call history: {message}"),
    }
}

impl ChatContent {
    fn into_text(self) -> String {
        match self {
            Self::Text(text) => text,
            Self::Parts(parts) => parts
                .into_iter()
                .filter_map(|part| match part {
                    ChatContentPart::Text { text } => Some(text),
                    ChatContentPart::ImageUrl { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

fn flush_pending_tool_calls(
    messages: &mut Vec<ChatMessage>,
    pending_tool_calls: &mut Vec<ChatToolCall>,
) {
    if pending_tool_calls.is_empty() {
        return;
    }
    messages.push(ChatMessage::Assistant {
        content: None,
        tool_calls: std::mem::take(pending_tool_calls),
    });
}

fn chat_content_from_items(content: Vec<ContentItem>) -> ChatContent {
    let mut parts = Vec::new();
    for item in content {
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                parts.push(ChatContentPart::Text { text });
            }
            ContentItem::InputImage { image_url, detail } => {
                parts.push(ChatContentPart::ImageUrl {
                    image_url: ChatImageUrl {
                        url: image_url,
                        detail,
                    },
                });
            }
        }
    }

    if parts.len() == 1
        && let ChatContentPart::Text { text } = &parts[0]
    {
        return ChatContent::Text(text.clone());
    }
    ChatContent::Parts(parts)
}

fn function_output_to_chat_content(output: FunctionCallOutputBody) -> String {
    output.to_text().unwrap_or_default()
}

fn chat_tools_from_responses_tools(
    tools: Vec<Value>,
    tool_names: &mut ChatToolNameMap,
) -> Result<Vec<ChatTool>, ApiError> {
    let mut converted = Vec::new();
    for tool in tools {
        match serde_json::from_value::<ResponsesTool>(tool) {
            Ok(ResponsesTool::Function(tool)) => {
                converted.push(chat_function_tool(tool, None, tool_names)?);
            }
            Ok(ResponsesTool::Namespace(namespace)) => {
                for tool in namespace.tools {
                    converted.push(chat_function_tool(tool, Some(&namespace.name), tool_names)?);
                }
            }
            Ok(ResponsesTool::Custom | ResponsesTool::Unknown) => {}
            Err(err) => {
                return Err(ApiError::Stream(format!("invalid chat tool object: {err}")));
            }
        }
    }
    Ok(converted)
}

fn chat_function_tool(
    tool: ResponsesFunctionTool,
    namespace: Option<&str>,
    tool_names: &mut ChatToolNameMap,
) -> Result<ChatTool, ApiError> {
    let chat_name = chat_function_name(namespace, &tool.name);
    insert_tool_name_mapping(
        tool_names,
        chat_name.clone(),
        ChatToolName {
            namespace: namespace.map(str::to_string),
            name: tool.name,
        },
    )?;
    Ok(ChatTool {
        kind: ChatToolKind::Function,
        function: ChatFunctionTool {
            name: chat_name,
            description: tool.description,
            parameters: tool.parameters,
            strict: tool.strict,
        },
    })
}

fn chat_function_name(namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(namespace) if namespace.ends_with('_') || name.starts_with('_') => {
            format!("{namespace}{name}")
        }
        Some(namespace) => format!("{namespace}_{name}"),
        None => name.to_string(),
    }
}

fn insert_tool_name_mapping(
    tool_names: &mut ChatToolNameMap,
    chat_name: String,
    tool_name: ChatToolName,
) -> Result<(), ApiError> {
    if let Some(existing) = tool_names.get(&chat_name)
        && existing != &tool_name
    {
        return Err(ApiError::Stream(format!(
            "duplicate chat tool name after namespace encoding: {chat_name}"
        )));
    }
    tool_names.insert(chat_name, tool_name);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::ResponsesApiRequest;
    use codex_protocol::models::FunctionCallOutputPayload;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn request(input: Vec<ResponseItem>, tools: Vec<Value>) -> ResponsesApiRequest {
        ResponsesApiRequest {
            model: "qwen".to_string(),
            instructions: "system prompt".to_string(),
            input,
            tools,
            tool_choice: "auto".to_string(),
            parallel_tool_calls: false,
            reasoning: None,
            store: false,
            stream: true,
            include: Vec::new(),
            service_tier: None,
            prompt_cache_key: None,
            text: None,
            client_metadata: None,
        }
    }

    fn body_from(request: ResponsesApiRequest) -> Value {
        chat_body_from_responses_request(request)
            .expect("chat body should convert")
            .body
    }

    fn chat_body_error(input: Vec<ResponseItem>) -> String {
        match chat_body_from_responses_request(request(input, Vec::new())) {
            Ok(_) => panic!("chat body should fail"),
            Err(ApiError::InvalidRequest { message }) => message,
            Err(err) => panic!("unexpected error: {err}"),
        }
    }

    #[test]
    fn converts_text_messages_and_tool_outputs() {
        let body = body_from(request(
            vec![
                ResponseItem::Message {
                    id: None,
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "hello".to_string(),
                    }],
                    phase: None,
                },
                ResponseItem::FunctionCall {
                    id: None,
                    name: "shell".to_string(),
                    namespace: None,
                    arguments: "{\"cmd\":\"printf ok\"}".to_string(),
                    call_id: "call-1".to_string(),
                },
                ResponseItem::FunctionCallOutput {
                    call_id: "call-1".to_string(),
                    output: FunctionCallOutputPayload::from_text("ok".to_string()),
                },
            ],
            Vec::new(),
        ));

        assert_eq!(
            body["messages"],
            json!([
                {"role": "system", "content": "system prompt"},
                {"role": "user", "content": "hello"},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": {
                            "name": "shell",
                            "arguments": "{\"cmd\":\"printf ok\"}"
                        }
                    }]
                },
                {"role": "tool", "tool_call_id": "call-1", "content": "ok"},
            ])
        );
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"], json!({"include_usage": true}));
    }

    #[test]
    fn converts_function_tools() {
        let request_body = chat_body_from_responses_request(request(
            Vec::new(),
            vec![json!({
                "type": "function",
                "name": "shell",
                "description": "run",
                "strict": false,
                "parameters": {"type": "object"}
            })],
        ))
        .expect("chat body should convert");
        let body = request_body.body;

        assert_eq!(
            body["tools"],
            json!([{
                "type": "function",
                "function": {
                    "name": "shell",
                    "description": "run",
                    "strict": false,
                    "parameters": {"type": "object"}
                }
            }])
        );
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(
            request_body.tool_names.get("shell"),
            Some(&ChatToolName {
                namespace: None,
                name: "shell".to_string(),
            })
        );
    }

    #[test]
    fn converts_namespace_tools_with_reversible_chat_names() {
        let request_body = chat_body_from_responses_request(request(
            Vec::new(),
            vec![json!({
                "type": "namespace",
                "name": "mcp__calendar__",
                "description": "calendar tools",
                "tools": [{
                    "type": "function",
                    "name": "lookup_order",
                    "description": "lookup",
                    "strict": false,
                    "parameters": {"type": "object"}
                }]
            })],
        ))
        .expect("chat body should convert");
        let body = request_body.body;

        assert_eq!(
            body["tools"],
            json!([{
                "type": "function",
                "function": {
                    "name": "mcp__calendar__lookup_order",
                    "description": "lookup",
                    "strict": false,
                    "parameters": {"type": "object"}
                }
            }])
        );
        assert_eq!(
            request_body.tool_names.get("mcp__calendar__lookup_order"),
            Some(&ChatToolName {
                namespace: Some("mcp__calendar__".to_string()),
                name: "lookup_order".to_string(),
            })
        );
    }

    #[test]
    fn converts_parallel_tool_calls_flag() {
        let mut request = request(
            Vec::new(),
            vec![json!({
                "type": "function",
                "name": "shell",
                "parameters": {"type": "object"}
            })],
        );
        request.parallel_tool_calls = true;

        let body = body_from(request);

        assert_eq!(body["parallel_tool_calls"], true);
    }

    #[test]
    fn coalesces_consecutive_function_calls_into_one_assistant_message() {
        let body = body_from(request(
            vec![
                ResponseItem::FunctionCall {
                    id: None,
                    name: "shell".to_string(),
                    namespace: None,
                    arguments: "{\"cmd\":\"printf alpha\"}".to_string(),
                    call_id: "call-alpha".to_string(),
                },
                ResponseItem::FunctionCall {
                    id: None,
                    name: "shell".to_string(),
                    namespace: None,
                    arguments: "{\"cmd\":\"printf beta\"}".to_string(),
                    call_id: "call-beta".to_string(),
                },
                ResponseItem::FunctionCallOutput {
                    call_id: "call-alpha".to_string(),
                    output: FunctionCallOutputPayload::from_text("alpha".to_string()),
                },
                ResponseItem::FunctionCallOutput {
                    call_id: "call-beta".to_string(),
                    output: FunctionCallOutputPayload::from_text("beta".to_string()),
                },
            ],
            Vec::new(),
        ));

        assert_eq!(
            body["messages"],
            json!([
                {"role": "system", "content": "system prompt"},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {
                            "id": "call-alpha",
                            "type": "function",
                            "function": {
                                "name": "shell",
                                "arguments": "{\"cmd\":\"printf alpha\"}"
                            }
                        },
                        {
                            "id": "call-beta",
                            "type": "function",
                            "function": {
                                "name": "shell",
                                "arguments": "{\"cmd\":\"printf beta\"}"
                            }
                        }
                    ]
                },
                {"role": "tool", "tool_call_id": "call-alpha", "content": "alpha"},
                {"role": "tool", "tool_call_id": "call-beta", "content": "beta"},
            ])
        );
    }

    #[test]
    fn encodes_namespaced_function_call_history() {
        let body = body_from(request(
            vec![
                ResponseItem::FunctionCall {
                    id: None,
                    name: "lookup_order".to_string(),
                    namespace: Some("mcp__calendar__".to_string()),
                    arguments: "{\"id\":\"ord_123\"}".to_string(),
                    call_id: "call-namespace".to_string(),
                },
                ResponseItem::FunctionCallOutput {
                    call_id: "call-namespace".to_string(),
                    output: FunctionCallOutputPayload::from_text("ok".to_string()),
                },
            ],
            Vec::new(),
        ));

        assert_eq!(
            body["messages"],
            json!([
                {"role": "system", "content": "system prompt"},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call-namespace",
                        "type": "function",
                        "function": {
                            "name": "mcp__calendar__lookup_order",
                            "arguments": "{\"id\":\"ord_123\"}"
                        }
                    }]
                },
                {"role": "tool", "tool_call_id": "call-namespace", "content": "ok"},
            ])
        );
    }

    #[test]
    fn rejects_empty_tool_call_ids() {
        let message = chat_body_error(vec![
            ResponseItem::FunctionCall {
                id: None,
                name: "shell".to_string(),
                namespace: None,
                arguments: "{\"cmd\":\"ls\"}".to_string(),
                call_id: String::new(),
            },
            ResponseItem::FunctionCallOutput {
                call_id: String::new(),
                output: FunctionCallOutputPayload::from_text("ok".to_string()),
            },
        ]);

        assert_eq!(
            message,
            "invalid chat tool call history: assistant tool call at message index 1 has an empty id"
        );
    }

    #[test]
    fn rejects_tool_outputs_without_preceding_tool_calls() {
        let message = chat_body_error(vec![ResponseItem::FunctionCallOutput {
            call_id: "call-orphan".to_string(),
            output: FunctionCallOutputPayload::from_text("ok".to_string()),
        }]);

        assert_eq!(
            message,
            "invalid chat tool call history: tool message at index 1 references 'call-orphan' without a preceding assistant tool call"
        );
    }

    #[test]
    fn rejects_mismatched_tool_outputs() {
        let message = chat_body_error(vec![
            ResponseItem::FunctionCall {
                id: None,
                name: "shell".to_string(),
                namespace: None,
                arguments: "{\"cmd\":\"ls\"}".to_string(),
                call_id: "call-a".to_string(),
            },
            ResponseItem::FunctionCallOutput {
                call_id: "call-b".to_string(),
                output: FunctionCallOutputPayload::from_text("ok".to_string()),
            },
        ]);

        assert_eq!(
            message,
            "invalid chat tool call history: tool message at index 2 references 'call-b' but expected one of [call-a]"
        );
    }

    #[test]
    fn rejects_duplicate_tool_call_ids() {
        let message = chat_body_error(vec![
            ResponseItem::FunctionCall {
                id: None,
                name: "shell".to_string(),
                namespace: None,
                arguments: "{\"cmd\":\"printf one\"}".to_string(),
                call_id: "call-dup".to_string(),
            },
            ResponseItem::FunctionCallOutput {
                call_id: "call-dup".to_string(),
                output: FunctionCallOutputPayload::from_text("one".to_string()),
            },
            ResponseItem::FunctionCall {
                id: None,
                name: "shell".to_string(),
                namespace: None,
                arguments: "{\"cmd\":\"printf two\"}".to_string(),
                call_id: "call-dup".to_string(),
            },
            ResponseItem::FunctionCallOutput {
                call_id: "call-dup".to_string(),
                output: FunctionCallOutputPayload::from_text("two".to_string()),
            },
        ]);

        assert_eq!(
            message,
            "invalid chat tool call history: duplicate assistant tool call id 'call-dup' at message index 3"
        );
    }

    #[test]
    fn rejects_tool_calls_without_outputs() {
        let message = chat_body_error(vec![ResponseItem::FunctionCall {
            id: None,
            name: "shell".to_string(),
            namespace: None,
            arguments: "{\"cmd\":\"ls\"}".to_string(),
            call_id: "call-pending".to_string(),
        }]);

        assert_eq!(
            message,
            "invalid chat tool call history: assistant message at index 1 has tool calls without matching tool outputs"
        );
    }

    #[test]
    fn preserves_custom_tool_call_history() {
        let body = body_from(request(
            vec![
                ResponseItem::CustomToolCall {
                    id: None,
                    status: None,
                    call_id: "call-custom".to_string(),
                    name: "apply_patch".to_string(),
                    input: "*** Begin Patch\n*** End Patch".to_string(),
                },
                ResponseItem::CustomToolCallOutput {
                    call_id: "call-custom".to_string(),
                    name: Some("apply_patch".to_string()),
                    output: FunctionCallOutputPayload::from_text("ok".to_string()),
                },
            ],
            Vec::new(),
        ));

        assert_eq!(
            body["messages"],
            json!([
                {"role": "system", "content": "system prompt"},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call-custom",
                        "type": "function",
                        "function": {
                            "name": "apply_patch",
                            "arguments": "*** Begin Patch\n*** End Patch"
                        }
                    }]
                },
                {"role": "tool", "tool_call_id": "call-custom", "content": "ok"},
            ])
        );
    }

    #[test]
    fn skips_custom_tools() {
        let body = body_from(request(
            Vec::new(),
            vec![json!({
                "type": "custom",
                "name": "apply_patch",
                "description": "patch",
                "format": {"type": "grammar", "syntax": "lark", "definition": "start: /x/"}
            })],
        ));

        assert!(body.get("tools").is_none());
    }

    #[test]
    fn converts_compaction_items_to_system_messages() {
        let body = body_from(request(
            vec![
                ResponseItem::Compaction {
                    encrypted_content: "compact summary".to_string(),
                },
                ResponseItem::ContextCompaction {
                    encrypted_content: Some("context compact summary".to_string()),
                },
                ResponseItem::ContextCompaction {
                    encrypted_content: None,
                },
            ],
            Vec::new(),
        ));

        assert_eq!(
            body["messages"],
            json!([
                {"role": "system", "content": "system prompt"},
                {"role": "system", "content": "compact summary"},
                {"role": "system", "content": "context compact summary"},
            ])
        );
    }

    #[test]
    fn merges_extra_body_fields() {
        let mut body = json!({"model": "qwen"});
        merge_extra_body(
            &mut body,
            &std::collections::HashMap::from([
                ("enable_thinking".to_string(), json!(true)),
                ("thinking_budget".to_string(), json!(1024)),
            ]),
        )
        .expect("extra body should merge");

        assert_eq!(body["enable_thinking"], true);
        assert_eq!(body["thinking_budget"], 1024);
    }

    #[test]
    fn merges_stream_options_without_disabling_usage() {
        let mut body = json!({
            "model": "qwen",
            "stream_options": {"include_usage": true}
        });
        merge_extra_body(
            &mut body,
            &std::collections::HashMap::from([(
                "stream_options".to_string(),
                json!({
                    "include_usage": false,
                    "provider_option": true
                }),
            )]),
        )
        .expect("extra body should merge");

        assert_eq!(
            body["stream_options"],
            json!({
                "include_usage": true,
                "provider_option": true
            })
        );
    }
}
