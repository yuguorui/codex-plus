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
    assert_eq!(
        function.parameters.required,
        Some(vec!["pattern".to_string()])
    );
    assert!(properties.contains_key("pattern"));
    assert!(properties.contains_key("path"));
    assert!(properties.contains_key("environment_id"));
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
}
