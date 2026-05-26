use super::*;

#[test]
fn glob_schema_uses_claude_style_name_and_optional_environment() {
    let tool = create_glob_tool(SimpleSearchToolOptions {
        include_environment_id: true,
    });

    let ToolSpec::Function(function) = tool else {
        panic!("expected function tool");
    };
    let properties = function
        .parameters
        .properties
        .expect("object schema should have properties");

    assert_eq!(function.name, "Glob");
    assert!(function.strict);
    assert!(
        function
            .description
            .contains("Fast file pattern matching tool")
    );
    assert_eq!(
        function.parameters.required,
        Some(vec!["pattern".to_string()])
    );
    assert!(properties.contains_key("pattern"));
    assert!(properties.contains_key("path"));
    assert!(properties.contains_key("environment_id"));
    let output_schema = function.output_schema.expect("glob output schema");
    assert_eq!(output_schema["properties"]["durationMs"]["type"], "integer");
    assert_eq!(output_schema["properties"]["numFiles"]["type"], "integer");
    assert_eq!(
        output_schema["properties"]["filenames"]["items"]["type"],
        "string"
    );
    assert_eq!(output_schema["properties"]["truncated"]["type"], "boolean");
}

#[test]
fn grep_schema_includes_expected_filters() {
    let tool = create_grep_tool(SimpleSearchToolOptions {
        include_environment_id: false,
    });

    let ToolSpec::Function(function) = tool else {
        panic!("expected function tool");
    };
    let properties = function
        .parameters
        .properties
        .expect("object schema should have properties");

    assert_eq!(function.name, "Grep");
    assert!(function.strict);
    assert!(
        function
            .description
            .contains("ALWAYS use Grep for search tasks")
    );
    assert_eq!(
        function.parameters.required,
        Some(vec!["pattern".to_string()])
    );
    assert!(properties.contains_key("glob"));
    assert!(properties.contains_key("type"));
    assert!(properties.contains_key("output_mode"));
    assert!(properties.contains_key("multiline"));
    assert!(properties.contains_key("-B"));
    assert!(properties.contains_key("-A"));
    assert!(properties.contains_key("-C"));
    assert!(properties.contains_key("context"));
    assert!(properties.contains_key("-n"));
    assert!(properties.contains_key("-i"));
    assert!(properties.contains_key("head_limit"));
    assert!(properties.contains_key("offset"));
    assert!(!properties.contains_key("case_insensitive"));
    assert!(!properties.contains_key("environment_id"));
    let output_schema = function.output_schema.expect("grep output schema");
    assert_eq!(
        output_schema["properties"]["mode"]["enum"],
        serde_json::json!(["content", "files_with_matches", "count"])
    );
    assert_eq!(output_schema["properties"]["numFiles"]["type"], "integer");
    assert_eq!(output_schema["properties"]["content"]["type"], "string");
    assert_eq!(
        output_schema["properties"]["appliedLimit"]["type"],
        "integer"
    );
}
