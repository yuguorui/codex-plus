use super::*;

#[test]
fn bash_schema_matches_claude_style_name_and_environment() {
    let tool = create_bash_tool(SimpleFileToolOptions {
        include_environment_id: true,
    });

    let ToolSpec::Function(function) = tool else {
        panic!("expected function tool");
    };
    let properties = function
        .parameters
        .properties
        .expect("object schema should have properties");

    assert_eq!(function.name, "Bash");
    assert_eq!(
        function.parameters.required,
        Some(vec!["command".to_string()])
    );
    assert!(properties.contains_key("command"));
    assert!(properties.contains_key("timeout"));
    assert!(properties.contains_key("run_in_background"));
    assert!(properties.contains_key("description"));
    assert!(properties.contains_key("dangerouslyDisableSandbox"));
    assert!(properties.contains_key("environment_id"));
}

#[test]
fn file_tool_schemas_use_expected_required_fields() {
    let options = SimpleFileToolOptions {
        include_environment_id: false,
    };

    let ToolSpec::Function(read) = create_read_tool(options) else {
        panic!("expected function tool");
    };
    let ToolSpec::Function(edit) = create_edit_tool(options) else {
        panic!("expected function tool");
    };
    let ToolSpec::Function(write) = create_write_tool(options) else {
        panic!("expected function tool");
    };

    assert_eq!(read.name, "Read");
    assert_eq!(
        read.parameters.required,
        Some(vec!["file_path".to_string()])
    );
    assert!(
        read.parameters
            .properties
            .expect("read should have properties")
            .contains_key("pages")
    );
    assert_eq!(edit.name, "Edit");
    assert_eq!(
        edit.parameters.required,
        Some(vec![
            "file_path".to_string(),
            "old_string".to_string(),
            "new_string".to_string(),
        ])
    );
    assert_eq!(write.name, "Write");
    assert_eq!(
        write.parameters.required,
        Some(vec!["file_path".to_string(), "content".to_string()])
    );
}
