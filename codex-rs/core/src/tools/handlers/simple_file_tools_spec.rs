use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SimpleFileToolOptions {
    pub include_environment_id: bool,
}

pub(crate) fn create_bash_tool(options: SimpleFileToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::from([
        (
            "command".to_string(),
            JsonSchema::string(Some("The shell command to execute.".to_string())),
        ),
        (
            "timeout".to_string(),
            JsonSchema::number(Some(
                "Optional time in milliseconds to wait for command output before yielding."
                    .to_string(),
            )),
        ),
        (
            "run_in_background".to_string(),
            JsonSchema::boolean(Some(
                "Set to true to run this command in the background. Use Read to read the output later."
                    .to_string(),
            )),
        ),
        (
            "dangerouslyDisableSandbox".to_string(),
            JsonSchema::boolean(Some(
                "Set this to true to dangerously override sandbox mode and run commands without sandboxing."
                    .to_string(),
            )),
        ),
        (
            "description".to_string(),
            JsonSchema::string(Some(
                "Brief description of what the command does.".to_string(),
            )),
        ),
    ]);
    add_environment_id(&mut properties, options);

    ToolSpec::Function(ResponsesApiTool {
        name: "Bash".to_string(),
        description: "Run shell commands using Codex unified exec.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["command".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub(crate) fn create_read_tool(options: SimpleFileToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::from([
        (
            "file_path".to_string(),
            JsonSchema::string(Some(
                "The absolute path to the file to read.".to_string(),
            )),
        ),
        (
            "offset".to_string(),
            JsonSchema::integer(Some(
                "The line number to start reading from. Only provide if the file is too large to read at once."
                    .to_string(),
            )),
        ),
        (
            "limit".to_string(),
            JsonSchema::integer(Some(
                "The number of lines to read. Only provide if the file is too large to read at once."
                    .to_string(),
            )),
        ),
        (
            "pages".to_string(),
            JsonSchema::string(Some(
                "Page range for PDF files, for example `1-5`, `3`, or `10-20`.".to_string(),
            )),
        ),
    ]);
    add_environment_id(&mut properties, options);

    ToolSpec::Function(ResponsesApiTool {
        name: "Read".to_string(),
        description: "Read a text file and return line-numbered content.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["file_path".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub(crate) fn create_edit_tool(options: SimpleFileToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::from([
        (
            "file_path".to_string(),
            JsonSchema::string(Some("The absolute path to the file to modify.".to_string())),
        ),
        (
            "old_string".to_string(),
            JsonSchema::string(Some("Exact text to replace.".to_string())),
        ),
        (
            "new_string".to_string(),
            JsonSchema::string(Some(
                "The text to replace it with (must be different from old_string).".to_string(),
            )),
        ),
        (
            "replace_all".to_string(),
            JsonSchema::boolean(Some(
                "Replace every match instead of requiring exactly one match.".to_string(),
            )),
        ),
    ]);
    add_environment_id(&mut properties, options);

    ToolSpec::Function(ResponsesApiTool {
        name: "Edit".to_string(),
        description: "Perform an exact string replacement in a text file.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec![
                "file_path".to_string(),
                "old_string".to_string(),
                "new_string".to_string(),
            ]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub(crate) fn create_write_tool(options: SimpleFileToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::from([
        (
            "file_path".to_string(),
            JsonSchema::string(Some("The absolute path to the file to write.".to_string())),
        ),
        (
            "content".to_string(),
            JsonSchema::string(Some("Complete file contents to write.".to_string())),
        ),
    ]);
    add_environment_id(&mut properties, options);

    ToolSpec::Function(ResponsesApiTool {
        name: "Write".to_string(),
        description: "Create or overwrite a text file.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["file_path".to_string(), "content".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

fn add_environment_id(
    properties: &mut BTreeMap<String, JsonSchema>,
    options: SimpleFileToolOptions,
) {
    if options.include_environment_id {
        properties.insert(
            "environment_id".to_string(),
            JsonSchema::string(Some(
                "Optional environment id from the <environment_context> block. If omitted, uses the primary environment."
                    .to_string(),
            )),
        );
    }
}

#[cfg(test)]
#[path = "simple_file_tools_spec_tests.rs"]
mod tests;
