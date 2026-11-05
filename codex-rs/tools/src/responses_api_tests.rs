use super::FreeformTool;
use super::LoadableToolSpec;
use super::ResponsesApiNamespace;
use super::ResponsesApiNamespaceTool;
use super::ResponsesApiTool;
use super::agent_plugin_mcp_tool_to_responses_api_tool;
use super::dynamic_tool_to_responses_api_tool;
use super::mcp_tool_to_deferred_responses_api_tool;
use super::tool_definition_to_responses_api_tool;
use crate::JsonSchema;
use crate::ToolDefinition;
use crate::ToolName;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeMap;

#[test]
fn freeform_tool_deferral_matches_function_tool_wire_shape() {
    let mut expected_wire_shape = json!({
        "name": "apply_patch",
        "description": "Apply a patch",
        "format": {
            "type": "grammar",
            "syntax": "lark",
            "definition": "start: \"patch\"",
        },
    });

    let mut tool: FreeformTool = serde_json::from_value(expected_wire_shape.clone())
        .expect("deserialize legacy freeform tool");

    assert_eq!(tool.defer_loading, None);
    assert_eq!(
        serde_json::to_value(&tool).expect("serialize eager freeform tool"),
        expected_wire_shape
    );

    tool.defer_loading = Some(true);
    expected_wire_shape["defer_loading"] = json!(true);
    assert_eq!(
        serde_json::to_value(tool).expect("serialize deferred freeform tool"),
        expected_wire_shape
    );
}

#[test]
fn tool_definition_to_responses_api_tool_omits_false_defer_loading() {
    assert_eq!(
        tool_definition_to_responses_api_tool(ToolDefinition {
            name: "lookup_order".to_string(),
            description: "Look up an order".to_string(),
            input_schema: JsonSchema::object(
                BTreeMap::from([(
                    "order_id".to_string(),
                    JsonSchema::string(/*description*/ None),
                )]),
                Some(vec!["order_id".to_string()]),
                Some(false.into())
            ),
            output_schema: Some(json!({"type": "object"}).into()),
            defer_loading: false,
        }),
        ResponsesApiTool {
            name: "lookup_order".to_string(),
            description: "Look up an order".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([(
                    "order_id".to_string(),
                    JsonSchema::string(/*description*/ None),
                )]),
                Some(vec!["order_id".to_string()]),
                Some(false.into())
            ),
            output_schema: Some(json!({"type": "object"}).into()),
        }
    );
}

#[test]
fn dynamic_tool_to_responses_api_tool_preserves_defer_loading() {
    let tool = DynamicToolFunctionSpec {
        name: "lookup_order".to_string(),
        description: "Look up an order".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "order_id": {"type": "string"}
            },
            "required": ["order_id"],
            "additionalProperties": false,
        }),
        defer_loading: true,
    };

    assert_eq!(
        dynamic_tool_to_responses_api_tool(&tool).expect("convert dynamic tool"),
        ResponsesApiTool {
            name: "lookup_order".to_string(),
            description: "Look up an order".to_string(),
            strict: false,
            defer_loading: Some(true),
            parameters: JsonSchema::object(
                BTreeMap::from([(
                    "order_id".to_string(),
                    JsonSchema::string(/*description*/ None),
                )]),
                Some(vec!["order_id".to_string()]),
                Some(false.into())
            ),
            output_schema: None,
        }
    );
}

#[test]
fn mcp_tool_to_deferred_responses_api_tool_sets_defer_loading() {
    let tool = rmcp::model::Tool::new(
        "lookup_order",
        "Look up an order",
        std::sync::Arc::new(rmcp::model::object(json!({
            "type": "object",
            "properties": {
                "order_id": {"type": "string"}
            },
            "required": ["order_id"],
            "additionalProperties": false,
        }))),
    );

    assert_eq!(
        mcp_tool_to_deferred_responses_api_tool(
            &ToolName::namespaced("mcp__codex_apps__", "lookup_order"),
            &tool,
        )
        .expect("convert deferred tool"),
        ResponsesApiTool {
            name: "lookup_order".to_string(),
            description: "Look up an order".to_string(),
            strict: false,
            defer_loading: Some(true),
            parameters: JsonSchema::object(
                BTreeMap::from([(
                    "order_id".to_string(),
                    JsonSchema::string(/*description*/ None),
                )]),
                Some(vec!["order_id".to_string()]),
                Some(false.into())
            ),
            output_schema: None,
        }
    );
}

#[test]
fn agent_plugin_mcp_tool_uses_fallback_for_oversized_schema() {
    let properties: serde_json::Map<String, serde_json::Value> = (0..1_024)
        .map(|index| (format!("property_{index}"), json!({"type": "string"})))
        .collect();
    let tool = rmcp::model::Tool::new(
        "oversized",
        "Large schema",
        std::sync::Arc::new(rmcp::model::object(json!({
            "type": "object",
            "properties": properties,
        }))),
    );

    assert_eq!(
        agent_plugin_mcp_tool_to_responses_api_tool(&ToolName::from("oversized"), &tool)
            .expect("Agent Plugin MCP tool should use a fallback schema")
            .parameters,
        JsonSchema::object(BTreeMap::new(), /*required*/ None, Some(true.into()))
    );
}

#[test]
fn legacy_mcp_tool_accepts_oversized_schema() {
    let properties: serde_json::Map<String, serde_json::Value> = (0..1_024)
        .map(|index| (format!("property_{index}"), json!({"type": "string"})))
        .collect();
    let tool = rmcp::model::Tool::new(
        "oversized",
        "Large legacy schema",
        std::sync::Arc::new(rmcp::model::object(json!({
            "type": "object",
            "properties": properties,
        }))),
    );

    let schema_bytes = serde_json::to_vec(tool.input_schema.as_ref())
        .expect("MCP input schema JSON")
        .len();
    assert!(schema_bytes > codex_code_mode::DEFAULT_INPUT_SCHEMA_MAX_BYTES);
    for (server_limit, expected_limit, expected_type) in [
        (
            None,
            codex_code_mode::DEFAULT_INPUT_SCHEMA_MAX_BYTES,
            "args: unknown",
        ),
        (Some(schema_bytes), schema_bytes, "property_1023?: string;"),
    ] {
        let tool = super::mcp_tool_to_responses_api_tool(
            &ToolName::from("oversized"),
            &tool,
            server_limit,
        )
        .expect("legacy MCP conversion must preserve existing acceptance");
        let definition =
            crate::tool_spec_to_code_mode_tool_definition(&crate::ToolSpec::Function(tool))
                .expect("Code Mode tool");
        assert_eq!(definition.input_schema_max_bytes, Some(expected_limit));
        assert!(definition.description.contains(expected_type));
    }
}

#[test]
fn loadable_tool_spec_namespace_serializes_with_deferred_child_tools() {
    let namespace = LoadableToolSpec::Namespace(ResponsesApiNamespace {
        name: "mcp__codex_apps__calendar".to_string(),
        description: "Plan events".to_string(),
        tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
            name: "create_event".to_string(),
            description: "Create a calendar event.".to_string(),
            strict: false,
            defer_loading: Some(true),
            parameters: JsonSchema::object(
                Default::default(),
                /*required*/ None,
                /*additional_properties*/ None,
            ),
            output_schema: None,
        })],
    });

    let value = serde_json::to_value(namespace).expect("serialize namespace");

    assert_eq!(
        value,
        json!({
            "type": "namespace",
            "name": "mcp__codex_apps__calendar",
            "description": "Plan events",
            "tools": [
                {
                    "type": "function",
                    "name": "create_event",
                    "description": "Create a calendar event.",
                    "strict": false,
                    "defer_loading": true,
                    "parameters": {
                        "type": "object",
                        "properties": {}
                    }
                }
            ]
        })
    );
}

fn tool_search_spec() -> crate::ToolSpec {
    crate::ToolSpec::ToolSearch {
        execution: "client".to_string(),
        description: "# Tool discovery\n\nSearches over deferred tool metadata.".to_string(),
        parameters: JsonSchema::object(
            BTreeMap::from([
                (
                    "query".to_string(),
                    JsonSchema::string(Some("Search query for deferred tools.".to_string())),
                ),
                (
                    "limit".to_string(),
                    JsonSchema::number(Some(
                        "Maximum number of tools to return. Defaults to 8.".to_string(),
                    )),
                ),
            ]),
            Some(vec!["query".to_string()]),
            Some(false.into()),
        ),
    }
}

#[test]
fn function_tool_search_transport_rewrites_wire_shape() {
    let tools = vec![tool_search_spec()];

    let raw = crate::create_tools_raw_json_for_responses_api_for_capabilities(
        &tools, /*supports_custom_exec_tool*/ true, /*supports_native_tool_search*/ false,
    )
    .expect("serialize tools for function-backed tool_search transport");

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(raw.get()).expect("parse raw tools"),
        json!([
            {
                "type": "function",
                "name": "tool_search",
                "description": "# Tool discovery\n\nSearches over deferred tool metadata.\n\nThe matcher is keyword-based (BM25) over English tool metadata: write `query` in English as tool-related keywords separated by spaces, for example `github repo file reader`. Avoid full sentences and non-English text.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search query for deferred tools."
                        },
                        "limit": {
                            "type": "number",
                            "description": "Maximum number of tools to return. Defaults to 8."
                        }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }
            }
        ])
    );
}

#[test]
fn capabilities_serializer_rewrites_both_transports_independently() {
    let tools = vec![
        crate::ToolSpec::Freeform(FreeformTool {
            name: "exec".to_string(),
            description: "Run JavaScript".to_string(),
            format: serde_json::from_value(json!({
                "type": "grammar",
                "syntax": "lark",
                "definition": "start: \"js\"",
            }))
            .expect("grammar format"),
            defer_loading: None,
        }),
        tool_search_spec(),
    ];

    let native = crate::create_tools_raw_json_for_responses_api_for_capabilities(
        &tools, /*supports_custom_exec_tool*/ true, /*supports_native_tool_search*/ true,
    )
    .expect("serialize tools for a native provider");
    let value: serde_json::Value = serde_json::from_str(native.get()).expect("parse raw tools");
    assert_eq!(value[0]["type"], json!("custom"));
    assert_eq!(value[1]["type"], json!("tool_search"));
    let native_description = value[1]["description"].as_str().unwrap_or_default();
    assert_eq!(
        native_description, "# Tool discovery\n\nSearches over deferred tool metadata.",
        "native transport keeps the original description without query guidance"
    );

    let shimmed = crate::create_tools_raw_json_for_responses_api_for_capabilities(
        &tools, /*supports_custom_exec_tool*/ false,
        /*supports_native_tool_search*/ false,
    )
    .expect("serialize tools for a function-only provider");
    let value: serde_json::Value = serde_json::from_str(shimmed.get()).expect("parse raw tools");
    assert_eq!(value[0]["type"], json!("function"));
    assert_eq!(value[0]["name"], json!("exec"));
    assert_eq!(value[0]["strict"], json!(true));
    assert_eq!(value[1]["type"], json!("function"));
    assert_eq!(value[1]["name"], json!("tool_search"));
    assert!(
        value[1].get("strict").is_none(),
        "tool_search keeps strict unset because limit is optional"
    );
    let shimmed_description = value[1]["description"].as_str().unwrap_or_default();
    assert!(shimmed_description.starts_with(native_description));
    assert!(
        shimmed_description.contains("BM25")
            && shimmed_description.contains("github repo file reader"),
        "function transport appends English-keyword query guidance: {shimmed_description}"
    );

    // Gateways such as BigModel accept custom tools but never expose a native
    // tool_search to the model: only the discovery tool is rewritten.
    let tool_search_shimmed = crate::create_tools_raw_json_for_responses_api_for_capabilities(
        &tools, /*supports_custom_exec_tool*/ true, /*supports_native_tool_search*/ false,
    )
    .expect("serialize tools for a custom-capable gateway without native tool_search");
    let value: serde_json::Value =
        serde_json::from_str(tool_search_shimmed.get()).expect("parse raw tools");
    assert_eq!(value[0]["type"], json!("custom"));
    assert_eq!(value[0]["name"], json!("exec"));
    assert_eq!(value[1]["type"], json!("function"));
    assert_eq!(value[1]["name"], json!("tool_search"));
}
