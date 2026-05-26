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
        .as_ref()
        .expect("object schema should have properties");

    assert_eq!(function.name, "Bash");
    assert!(function.strict);
    assert!(
        function
            .description
            .contains("Executes a given bash command and returns its output.")
    );
    assert!(
        function
            .description
            .contains("File search: Use Glob (NOT find or ls)")
    );
    assert!(
        function
            .description
            .contains("Content search: Use Grep (NOT grep or rg)")
    );
    assert!(
        function
            .description
            .contains("When issuing multiple commands:")
    );
    assert!(
        function
            .description
            .contains("You can use the `run_in_background` parameter")
    );
    assert_eq!(
        function.parameters.required,
        Some(vec!["command".to_string()])
    );
    assert_eq!(
        properties
            .get("command")
            .and_then(|schema| schema.description.as_deref()),
        Some("The command to execute")
    );
    assert_eq!(
        properties
            .get("timeout")
            .and_then(|schema| schema.description.as_deref()),
        Some("Optional timeout in milliseconds (max 600000)")
    );
    assert!(
        properties
            .get("description")
            .and_then(|schema| schema.description.as_deref())
            .expect("description schema should have description")
            .contains("Clear, concise description of what this command does in active voice")
    );
    assert!(properties.contains_key("run_in_background"));
    assert!(properties.contains_key("dangerouslyDisableSandbox"));
    assert!(properties.contains_key("environment_id"));
}

#[test]
fn bash_schema_matches_claude_style_without_environment() {
    let tool = create_bash_tool(SimpleFileToolOptions {
        include_environment_id: false,
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
    assert!(!properties.contains_key("environment_id"));
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
    assert!(read.strict);
    assert!(
        read.description
            .contains("Reads a file from the local filesystem")
    );
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
    let output_schema = read
        .output_schema
        .as_ref()
        .expect("read should expose output schema");
    assert_eq!(
        output_schema["oneOf"][0]["properties"]["type"]["const"],
        "text"
    );
    assert_eq!(
        output_schema["oneOf"][1]["properties"]["type"]["const"],
        "image"
    );
    assert_eq!(
        output_schema["oneOf"][1]["properties"]["file"]["properties"]["type"]["enum"],
        serde_json::json!(["image/jpeg", "image/png", "image/gif", "image/webp"])
    );
    assert_eq!(
        output_schema["oneOf"][2]["properties"]["type"]["const"],
        "pdf"
    );
    assert_eq!(
        output_schema["oneOf"][3]["properties"]["type"]["const"],
        "file_unchanged"
    );
    assert_eq!(edit.name, "Edit");
    assert!(edit.strict);
    assert!(
        edit.description
            .contains("Performs exact string replacements in files")
    );
    assert_eq!(
        edit.parameters.required,
        Some(vec![
            "file_path".to_string(),
            "old_string".to_string(),
            "new_string".to_string(),
        ])
    );
    let edit_output_schema = edit.output_schema.expect("edit output schema");
    assert_eq!(
        edit_output_schema["properties"]["filePath"]["type"],
        "string"
    );
    assert_eq!(
        edit_output_schema["properties"]["structuredPatch"]["items"]["properties"]["oldStart"]["type"],
        "integer"
    );
    assert_eq!(
        edit_output_schema["properties"]["gitDiff"]["properties"]["status"]["enum"],
        serde_json::json!(["modified", "added"])
    );
    assert_eq!(write.name, "Write");
    assert!(write.strict);
    assert!(
        write
            .description
            .contains("Writes a file to the local filesystem")
    );
    assert_eq!(
        write.parameters.required,
        Some(vec!["file_path".to_string(), "content".to_string()])
    );
    let write_output_schema = write.output_schema.expect("write output schema");
    assert_eq!(
        write_output_schema["properties"]["type"]["enum"],
        serde_json::json!(["create", "update"])
    );
    assert_eq!(
        write_output_schema["properties"]["originalFile"]["type"],
        serde_json::json!(["string", "null"])
    );
    assert_eq!(
        write_output_schema["properties"]["structuredPatch"]["items"]["properties"]["lines"]["items"]
            ["type"],
        "string"
    );
}
