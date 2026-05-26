use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

use crate::tools::handlers::simple_tool_output::EditOutput;
use crate::tools::handlers::simple_tool_output::WriteOutput;
use crate::tools::handlers::simple_tool_output::generated_output_schema;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SimpleFileToolOptions {
    pub include_environment_id: bool,
}

const BASH_DEFAULT_TIMEOUT_MS: u64 = 120_000;
const BASH_MAX_TIMEOUT_MS: u64 = 600_000;

pub(crate) fn create_bash_tool(options: SimpleFileToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::from([
        (
            "command".to_string(),
            JsonSchema::string(Some("The command to execute".to_string())),
        ),
        (
            "timeout".to_string(),
            JsonSchema::number(Some(
                format!("Optional timeout in milliseconds (max {BASH_MAX_TIMEOUT_MS})"),
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
            JsonSchema::string(Some(bash_description_parameter_description())),
        ),
    ]);
    add_environment_id(&mut properties, options);

    ToolSpec::Function(ResponsesApiTool {
        name: "Bash".to_string(),
        description: bash_tool_description(),
        strict: true,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["command".to_string()]),
            Some(false.into()),
        ),
        output_schema: Some(edit_output_schema()),
    })
}

fn bash_description_parameter_description() -> String {
    r#"Clear, concise description of what this command does in active voice. Never use words like "complex" or "risk" in the description - just describe what it does.

For simple commands (git, npm, standard CLI tools), keep it brief (5-10 words):
- ls → "List files in current directory"
- git status → "Show working tree status"
- npm install → "Install package dependencies"

For commands that are harder to parse at a glance (piped commands, obscure flags, etc.), add enough context to clarify what it does:
- find . -name "*.tmp" -exec rm {} \; → "Find and delete all .tmp files recursively"
- git reset --hard origin/main → "Discard all local changes and match remote main"
- curl -s url | jq '.data[]' → "Fetch JSON from URL and extract data array elements""#
        .to_string()
}

fn bash_tool_description() -> String {
    format!(
        r#"Executes a given bash command and returns its output.

The working directory persists between commands, but shell state does not. The shell environment is initialized from the user's profile (bash or zsh).

IMPORTANT: Avoid using this tool to run `find`, `grep`, `cat`, `head`, `tail`, `sed`, `awk`, or `echo` commands, unless explicitly instructed or after you have verified that a dedicated tool cannot accomplish your task. Instead, use the appropriate dedicated tool as this will provide a much better experience for the user:

 - File search: Use Glob (NOT find or ls)
 - Content search: Use Grep (NOT grep or rg)
 - Read files: Use Read (NOT cat/head/tail)
 - Edit files: Use Edit (NOT sed/awk)
 - Write files: Use Write (NOT echo >/cat <<EOF)
 - Communication: Output text directly (NOT echo/printf)
While the Bash tool can do similar things, it’s better to use the built-in tools as they provide a better user experience and make it easier to review tool calls and give permission.

# Instructions
 - If your command will create new directories or files, first use this tool to run `ls` to verify the parent directory exists and is the correct location.
 - Always quote file paths that contain spaces with double quotes in your command (e.g., cd "path with spaces/file.txt")
 - Try to maintain your current working directory throughout the session by using absolute paths and avoiding usage of `cd`. You may use `cd` if the User explicitly requests it.
 - You may specify an optional timeout in milliseconds (up to {BASH_MAX_TIMEOUT_MS}ms / {max_timeout_minutes} minutes). By default, your command will timeout after {BASH_DEFAULT_TIMEOUT_MS}ms ({default_timeout_minutes} minutes).
 - You can use the `run_in_background` parameter to run the command in the background. Only use this if you don't need the result immediately and are OK being notified when the command completes later. You do not need to check the output right away - you'll be notified when it finishes. You do not need to use '&' at the end of the command when using this parameter.
 - When issuing multiple commands:
  - If the commands are independent and can run in parallel, make multiple Bash tool calls in a single message. Example: if you need to run "git status" and "git diff", send a single message with two Bash tool calls in parallel.
  - If the commands depend on each other and must run sequentially, use a single Bash call with '&&' to chain them together.
  - Use ';' only when you need to run commands sequentially but don't care if earlier commands fail.
  - DO NOT use newlines to separate commands (newlines are ok in quoted strings).
 - For git commands:
  - Prefer to create a new commit rather than amending an existing commit.
  - Before running destructive operations (e.g., git reset --hard, git push --force, git checkout --), consider whether there is a safer alternative that achieves the same goal. Only use destructive operations when they are truly the best approach.
  - Never skip hooks (--no-verify) or bypass signing (--no-gpg-sign, -c commit.gpgsign=false) unless the user has explicitly asked for it. If a hook fails, investigate and fix the underlying issue.
 - Avoid unnecessary `sleep` commands:
  - Do not sleep between commands that can run immediately — just run them.
  - If your command is long running and you would like to be notified when it finishes — use `run_in_background`. No sleep needed.
  - Do not retry failing commands in a sleep loop — diagnose the root cause.
  - If waiting for a background task you started with `run_in_background`, you will be notified when it completes — do not poll.
  - If you must poll an external process, use a check command (e.g. `gh run view`) rather than sleeping first.
  - If you must sleep, keep the duration short (1-5 seconds) to avoid blocking the user."#,
        max_timeout_minutes = BASH_MAX_TIMEOUT_MS / 60_000,
        default_timeout_minutes = BASH_DEFAULT_TIMEOUT_MS / 60_000,
    )
}

pub(crate) fn create_read_tool(options: SimpleFileToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::from([
        (
            "file_path".to_string(),
            JsonSchema::string(Some(
                "The absolute path to the file to read".to_string(),
            )),
        ),
        (
            "offset".to_string(),
            JsonSchema::integer(Some(
                "The line number to start reading from. Only provide if the file is too large to read at once"
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
                "Page range for PDF files (e.g., \"1-5\", \"3\", \"10-20\"). Only applicable to PDF files.".to_string(),
            )),
        ),
    ]);
    add_environment_id(&mut properties, options);

    ToolSpec::Function(ResponsesApiTool {
        name: "Read".to_string(),
        description: read_tool_description(),
        strict: true,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["file_path".to_string()]),
            Some(false.into()),
        ),
        output_schema: Some(read_tool_output_schema()),
    })
}

pub(crate) fn create_edit_tool(options: SimpleFileToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::from([
        (
            "file_path".to_string(),
            JsonSchema::string(Some("The absolute path to the file to modify".to_string())),
        ),
        (
            "old_string".to_string(),
            JsonSchema::string(Some("The text to replace".to_string())),
        ),
        (
            "new_string".to_string(),
            JsonSchema::string(Some(
                "The text to replace it with (must be different from old_string)".to_string(),
            )),
        ),
        (
            "replace_all".to_string(),
            JsonSchema::boolean(Some(
                "Replace all occurrences of old_string (default false)".to_string(),
            )),
        ),
    ]);
    add_environment_id(&mut properties, options);

    ToolSpec::Function(ResponsesApiTool {
        name: "Edit".to_string(),
        description: edit_tool_description(),
        strict: true,
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
        output_schema: Some(edit_output_schema()),
    })
}

pub(crate) fn create_write_tool(options: SimpleFileToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::from([
        (
            "file_path".to_string(),
            JsonSchema::string(Some(
                "The absolute path to the file to write (must be absolute, not relative)"
                    .to_string(),
            )),
        ),
        (
            "content".to_string(),
            JsonSchema::string(Some("The content to write to the file".to_string())),
        ),
    ]);
    add_environment_id(&mut properties, options);

    ToolSpec::Function(ResponsesApiTool {
        name: "Write".to_string(),
        description: write_tool_description(),
        strict: true,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["file_path".to_string(), "content".to_string()]),
            Some(false.into()),
        ),
        output_schema: Some(write_output_schema()),
    })
}

pub(crate) fn edit_output_schema() -> Value {
    generated_output_schema::<EditOutput>()
}

pub(crate) fn write_output_schema() -> Value {
    generated_output_schema::<WriteOutput>()
}

fn read_tool_output_schema() -> Value {
    json!({
        "oneOf": [
            {
                "type": "object",
                "properties": {
                    "type": { "const": "text" },
                    "file": {
                        "type": "object",
                        "properties": {
                            "filePath": {
                                "type": "string",
                                "description": "The path to the file that was read"
                            },
                            "content": {
                                "type": "string",
                                "description": "The content of the file"
                            },
                            "numLines": {
                                "type": "number",
                                "description": "Number of lines in the returned content"
                            },
                            "startLine": {
                                "type": "number",
                                "description": "The starting line number"
                            },
                            "totalLines": {
                                "type": "number",
                                "description": "Total number of lines in the file"
                            }
                        },
                        "required": ["filePath", "content", "numLines", "startLine", "totalLines"],
                        "additionalProperties": false
                    }
                },
                "required": ["type", "file"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "type": { "const": "image" },
                    "file": {
                        "type": "object",
                        "properties": {
                            "base64": {
                                "type": "string",
                                "description": "Base64-encoded image data"
                            },
                            "type": {
                                "type": "string",
                                "enum": ["image/jpeg", "image/png", "image/gif", "image/webp"],
                                "description": "The MIME type of the image"
                            },
                            "originalSize": {
                                "type": "number",
                                "description": "Original file size in bytes"
                            },
                            "dimensions": {
                                "type": "object",
                                "properties": {
                                    "originalWidth": { "type": "number" },
                                    "originalHeight": { "type": "number" },
                                    "displayWidth": { "type": "number" },
                                    "displayHeight": { "type": "number" }
                                },
                                "additionalProperties": false,
                                "description": "Image dimension info for coordinate mapping"
                            }
                        },
                        "required": ["base64", "type", "originalSize"],
                        "additionalProperties": false
                    }
                },
                "required": ["type", "file"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "type": { "const": "pdf" },
                    "file": {
                        "type": "object",
                        "properties": {
                            "filePath": {
                                "type": "string",
                                "description": "The path to the PDF file"
                            },
                            "content": {
                                "type": "string",
                                "description": "Extracted PDF text content"
                            },
                            "numLines": {
                                "type": "number",
                                "description": "Number of lines in the returned content"
                            },
                            "startLine": {
                                "type": "number",
                                "description": "The starting line number"
                            },
                            "totalLines": {
                                "type": "number",
                                "description": "Total number of lines in the extracted PDF text"
                            }
                        },
                        "required": ["filePath", "content", "numLines", "startLine", "totalLines"],
                        "additionalProperties": false
                    }
                },
                "required": ["type", "file"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "type": { "const": "file_unchanged" },
                    "file": {
                        "type": "object",
                        "properties": {
                            "filePath": {
                                "type": "string",
                                "description": "The path to the file"
                            }
                        },
                        "required": ["filePath"],
                        "additionalProperties": false
                    }
                },
                "required": ["type", "file"],
                "additionalProperties": false
            }
        ]
    })
}

fn read_tool_description() -> String {
    r#"Reads a file from the local filesystem. You can access any file directly by using this tool.
Assume this tool is able to read all files on the machine. If the User provides a path to a file assume that path is valid. It is okay to read a file that does not exist; an error will be returned.

Usage:
- The file_path parameter must be an absolute path, not a relative path
- You can optionally specify a line offset and limit (especially handy for long files), but it's recommended to read the whole file by not providing these parameters
- Results are returned using cat -n format, with line numbers starting at 1
- This tool can read images (.png, .jpg, .jpeg, .gif, .webp). When you need to inspect a screenshot path, ALWAYS use this tool.
- This tool can read PDF files (.pdf). For large PDFs, use the pages parameter to read specific page ranges (e.g., pages: "1-5").
- This tool can only read files, not directories. To read a directory, use an ls command via the Bash tool.
- If you read a file that exists but has empty contents you will receive a system reminder warning in place of file contents."#
        .to_string()
}

fn edit_tool_description() -> String {
    format!(
        r#"Performs exact string replacements in files.

Usage:
- You must use your `Read` tool at least once in the conversation before editing. This tool will error if you attempt an edit without reading the file.{pre_read_trailing_space}
- When editing text from Read tool output, ensure you preserve the exact indentation (tabs/spaces) as it appears AFTER the line number prefix. The line number prefix format is: spaces + line number + arrow. Everything after that is the actual file content to match. Never include any part of the line number prefix in the old_string or new_string.
- ALWAYS prefer editing existing files in the codebase. NEVER write new files unless explicitly required.
- Only use emojis if the user explicitly requests it. Avoid adding emojis to files unless asked.
- The edit will FAIL if `old_string` is not unique in the file. Either provide a larger string with more surrounding context to make it unique or use `replace_all` to change every instance of `old_string`.
- Use `replace_all` for replacing and renaming strings across the file. This parameter is useful if you want to rename a variable for instance."#,
        pre_read_trailing_space = " ",
    )
}

fn write_tool_description() -> String {
    r#"Writes a file to the local filesystem.

Usage:
- This tool will overwrite the existing file if there is one at the provided path.
- If this is an existing file, you MUST use the Read tool first to read the file's contents. This tool will fail if you did not read the file first.
- Prefer the Edit tool for modifying existing files — it only sends the diff. Only use this tool to create new files or for complete rewrites.
- NEVER create documentation files (*.md) or README files unless explicitly requested by the User.
- Only use emojis if the user explicitly requests it. Avoid writing emojis to files unless asked."#
        .to_string()
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
