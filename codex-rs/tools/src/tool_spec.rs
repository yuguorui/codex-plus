use crate::FreeformTool;
use crate::JsonSchema;
use crate::LoadableToolSpec;
use crate::ResponsesApiNamespace;
use crate::ResponsesApiNamespaceTool;
use crate::ResponsesApiTool;
use crate::default_namespace_description;
use codex_protocol::DEFAULT_FUNCTION_NAMESPACE;
use codex_protocol::config_types::WebSearchContextSize;
use codex_protocol::config_types::WebSearchFilters as ConfigWebSearchFilters;
use codex_protocol::config_types::WebSearchUserLocation as ConfigWebSearchUserLocation;
use codex_protocol::config_types::WebSearchUserLocationType;
use serde::Serialize;
use serde_json::Value;
use serde_json::value::RawValue;
use std::sync::Arc;

/// When serialized as JSON, this produces a valid "Tool" in the OpenAI
/// Responses API.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type")]
pub enum ToolSpec {
    #[serde(rename = "function")]
    Function(ResponsesApiTool),
    #[serde(rename = "namespace")]
    Namespace(ResponsesApiNamespace),
    #[serde(rename = "tool_search")]
    ToolSearch {
        execution: String,
        description: String,
        parameters: JsonSchema,
    },
    // TODO: Understand why we get an error on web_search although the API docs
    // say it's supported.
    // https://platform.openai.com/docs/guides/tools-web-search?api-mode=responses#:~:text=%7B%20type%3A%20%22web_search%22%20%7D%2C
    // `external_web_access` distinguishes cached from live-capable search, while
    // `indexed_web_access` restricts live fetches to indexed URLs.
    // https://platform.openai.com/docs/guides/tools-web-search#live-internet-access
    #[serde(rename = "web_search")]
    WebSearch {
        #[serde(skip_serializing_if = "Option::is_none")]
        external_web_access: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        indexed_web_access: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        filters: Option<ResponsesApiWebSearchFilters>,
        #[serde(skip_serializing_if = "Option::is_none")]
        user_location: Option<ResponsesApiWebSearchUserLocation>,
        #[serde(skip_serializing_if = "Option::is_none")]
        search_context_size: Option<WebSearchContextSize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        search_content_types: Option<Vec<String>>,
    },
    #[serde(rename = "custom")]
    Freeform(FreeformTool),
}

impl ToolSpec {
    pub fn name(&self) -> &str {
        match self {
            ToolSpec::Function(tool) => tool.name.as_str(),
            ToolSpec::Namespace(namespace) => namespace.name.as_str(),
            ToolSpec::ToolSearch { .. } => "tool_search",
            ToolSpec::WebSearch { .. } => "web_search",
            ToolSpec::Freeform(tool) => tool.name.as_str(),
        }
    }
}

impl From<LoadableToolSpec> for ToolSpec {
    fn from(value: LoadableToolSpec) -> Self {
        match value {
            LoadableToolSpec::Function(tool) => ToolSpec::Function(tool),
            LoadableToolSpec::Namespace(namespace) => ToolSpec::Namespace(namespace),
        }
    }
}

/// Returns JSON values that are compatible with Function Calling in the
/// Responses API:
/// https://platform.openai.com/docs/guides/function-calling?api-mode=responses
pub fn create_tools_json_for_responses_api(
    tools: &[ToolSpec],
) -> Result<Vec<Value>, serde_json::Error> {
    let mut tools_json = Vec::new();

    for tool in tools {
        let json = serde_json::to_value(tool)?;
        tools_json.push(json);
    }

    Ok(tools_json)
}

pub fn create_tools_json_for_responses_lite(
    tools: &[ToolSpec],
) -> Result<Vec<Value>, serde_json::Error> {
    let mut functions = ResponsesApiNamespace {
        name: DEFAULT_FUNCTION_NAMESPACE.to_string(),
        description: default_namespace_description(DEFAULT_FUNCTION_NAMESPACE),
        tools: Vec::new(),
    };
    let mut functions_index = None;
    let mut tools_json = Vec::new();

    for tool in tools {
        match tool {
            ToolSpec::Function(tool) => {
                functions
                    .tools
                    .push(ResponsesApiNamespaceTool::Function(tool.clone()));
            }
            ToolSpec::Freeform(tool) => {
                functions
                    .tools
                    .push(ResponsesApiNamespaceTool::Custom(tool.clone()));
            }
            ToolSpec::Namespace(namespace) if namespace.name == DEFAULT_FUNCTION_NAMESPACE => {
                if !namespace.description.trim().is_empty() {
                    functions.description = namespace.description.clone();
                }
                functions.tools.extend(namespace.tools.clone());
            }
            tool => {
                tools_json.push(serde_json::to_value(tool)?);
                continue;
            }
        }
        functions_index.get_or_insert(tools_json.len());
    }

    if let Some(functions_index) = functions_index
        && !functions.tools.is_empty()
    {
        tools_json.insert(
            functions_index,
            serde_json::to_value(ToolSpec::Namespace(functions))?,
        );
    }

    Ok(tools_json)
}

/// Returns raw JSON that can be embedded directly in a Responses API request.
pub fn create_tools_raw_json_for_responses_api(
    tools: &[ToolSpec],
) -> Result<Arc<RawValue>, serde_json::Error> {
    serde_json::value::to_raw_value(tools).map(Arc::from)
}

/// Returns raw Responses tools with Code Mode's `custom exec` rewritten as a
/// `function` tool.
///
/// Providers such as DeepSeek reject custom tools other than `apply_patch`.
/// The function schema carries the same raw JavaScript in `input`, and the
/// Code Mode handler accepts that payload. `apply_patch` stays custom because
/// DeepSeek explicitly supports that one custom tool.
pub fn create_tools_raw_json_for_responses_api_with_function_exec(
    tools: &[ToolSpec],
) -> Result<Arc<RawValue>, serde_json::Error> {
    let mut tools = serde_json::to_value(tools)?;
    if let Some(tools) = tools.as_array_mut() {
        for tool in tools {
            if tool.get("type").and_then(Value::as_str) != Some("custom") {
                continue;
            }
            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                continue;
            };
            let name = name.to_string();
            if name == "apply_patch" {
                continue;
            }
            let description = tool
                .get("description")
                .cloned()
                .unwrap_or_else(|| Value::String(String::new()));
            let input_description = if name == "exec" {
                "Raw JavaScript source for the exec tool. Put the source in this `input` string; do not send a {\"cmd\": ...} object."
            } else {
                "Raw input for the custom tool."
            };
            let mut function = serde_json::json!({
                "type": "function",
                "name": name,
                "description": description,
                "parameters": {
                    "type": "object",
                    "properties": {
                        "input": {
                            "type": "string",
                            "description": input_description
                        }
                    },
                    "required": ["input"],
                    "additionalProperties": false
                },
                "strict": true
            });
            if let Some(defer_loading) = tool.get("defer_loading") {
                function["defer_loading"] = defer_loading.clone();
            }
            *tool = function;
        }
    }
    serde_json::value::to_raw_value(&tools).map(Arc::from)
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ResponsesApiWebSearchFilters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
}

impl From<ConfigWebSearchFilters> for ResponsesApiWebSearchFilters {
    fn from(filters: ConfigWebSearchFilters) -> Self {
        Self {
            allowed_domains: filters.allowed_domains,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ResponsesApiWebSearchUserLocation {
    #[serde(rename = "type")]
    pub r#type: WebSearchUserLocationType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

impl From<ConfigWebSearchUserLocation> for ResponsesApiWebSearchUserLocation {
    fn from(user_location: ConfigWebSearchUserLocation) -> Self {
        Self {
            r#type: user_location.r#type,
            country: user_location.country,
            region: user_location.region,
            city: user_location.city,
            timezone: user_location.timezone,
        }
    }
}

#[cfg(test)]
#[path = "tool_spec_tests.rs"]
mod tests;
