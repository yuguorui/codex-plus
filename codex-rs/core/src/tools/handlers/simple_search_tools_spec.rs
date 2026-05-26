use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

use crate::tools::handlers::simple_tool_output::GlobOutput;
use crate::tools::handlers::simple_tool_output::GrepOutput;
use crate::tools::handlers::simple_tool_output::generated_output_schema;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SimpleSearchToolOptions {
    pub include_environment_id: bool,
}

pub(crate) const GLOB_TOOL_NAME: &str = "Glob";
pub(crate) const GREP_TOOL_NAME: &str = "Grep";

pub(crate) fn create_glob_tool(options: SimpleSearchToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::from([
        (
            "pattern".to_string(),
            JsonSchema::string(Some(
                "The glob pattern to match files against".to_string(),
            )),
        ),
        (
            "path".to_string(),
            JsonSchema::string(Some(
                "The directory to search in. If not specified, the current working directory will be used. IMPORTANT: Omit this field to use the default directory. DO NOT enter \"undefined\" or \"null\" - simply omit it for the default behavior. Must be a valid directory path if provided.".to_string(),
            )),
        ),
    ]);
    maybe_add_environment_id(&mut properties, options);

    ToolSpec::Function(ResponsesApiTool {
        name: GLOB_TOOL_NAME.to_string(),
        description: glob_tool_description(),
        strict: true,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["pattern".to_string()]),
            Some(false.into()),
        ),
        output_schema: Some(glob_output_schema()),
    })
}

pub(crate) fn create_grep_tool(options: SimpleSearchToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::from([
        (
            "pattern".to_string(),
            JsonSchema::string(Some(
                "The regular expression pattern to search for in file contents".to_string(),
            )),
        ),
        (
            "path".to_string(),
            JsonSchema::string(Some(
                "File or directory to search in (rg PATH). Defaults to current working directory."
                    .to_string(),
            )),
        ),
        (
            "glob".to_string(),
            JsonSchema::string(Some(
                "Glob pattern to filter files (e.g. \"*.js\", \"*.{ts,tsx}\") - maps to rg --glob"
                    .to_string(),
            )),
        ),
        (
            "type".to_string(),
            JsonSchema::string(Some(
                "File type to search (rg --type). Common types: js, py, rust, go, java, etc. More efficient than include for standard file types."
                    .to_string(),
            )),
        ),
        (
            "output_mode".to_string(),
            JsonSchema::string_enum(
                vec![json!("files_with_matches"), json!("content"), json!("count")],
                Some(
                    "Output mode: \"content\" shows matching lines (supports -A/-B/-C context, -n line numbers, head_limit), \"files_with_matches\" shows file paths (supports head_limit), \"count\" shows match counts (supports head_limit). Defaults to \"files_with_matches\"."
                        .to_string(),
                ),
            ),
        ),
        (
            "multiline".to_string(),
            JsonSchema::boolean(Some(
                "Enable multiline mode where . matches newlines and patterns can span lines (rg -U --multiline-dotall). Default: false.".to_string(),
            )),
        ),
        (
            "-B".to_string(),
            JsonSchema::number(Some(
                "Number of lines to show before each match (rg -B). Requires output_mode: \"content\", ignored otherwise."
                    .to_string(),
            )),
        ),
        (
            "-A".to_string(),
            JsonSchema::number(Some(
                "Number of lines to show after each match (rg -A). Requires output_mode: \"content\", ignored otherwise."
                    .to_string(),
            )),
        ),
        (
            "-C".to_string(),
            JsonSchema::number(Some("Alias for context.".to_string())),
        ),
        (
            "context".to_string(),
            JsonSchema::number(Some(
                "Number of lines to show before and after each match (rg -C). Requires output_mode: \"content\", ignored otherwise."
                    .to_string(),
            )),
        ),
        (
            "-n".to_string(),
            JsonSchema::boolean(Some(
                "Show line numbers in output (rg -n). Requires output_mode: \"content\", ignored otherwise. Defaults to true.".to_string(),
            )),
        ),
        (
            "-i".to_string(),
            JsonSchema::boolean(Some("Case insensitive search (rg -i)".to_string())),
        ),
        (
            "head_limit".to_string(),
            JsonSchema::number(Some(
                "Limit output to first N lines/entries, equivalent to \"| head -N\". Works across all output modes: content (limits output lines), files_with_matches (limits file paths), count (limits count entries). Defaults to 250 when unspecified. Pass 0 for unlimited (use sparingly — large result sets waste context)."
                    .to_string(),
            )),
        ),
        (
            "offset".to_string(),
            JsonSchema::number(Some(
                "Skip first N lines/entries before applying head_limit, equivalent to \"| tail -n +N | head -N\". Works across all output modes. Defaults to 0."
                    .to_string(),
            )),
        ),
    ]);
    maybe_add_environment_id(&mut properties, options);

    ToolSpec::Function(ResponsesApiTool {
        name: GREP_TOOL_NAME.to_string(),
        description: grep_tool_description(),
        strict: true,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["pattern".to_string()]),
            Some(false.into()),
        ),
        output_schema: Some(grep_output_schema()),
    })
}

pub(crate) fn glob_output_schema() -> Value {
    generated_output_schema::<GlobOutput>()
}

pub(crate) fn grep_output_schema() -> Value {
    generated_output_schema::<GrepOutput>()
}

fn maybe_add_environment_id(
    properties: &mut BTreeMap<String, JsonSchema>,
    options: SimpleSearchToolOptions,
) {
    if options.include_environment_id {
        properties.insert(
            "environment_id".to_string(),
            JsonSchema::string(Some(
                "Optional selected environment id to target. Omit this to use the primary environment."
                    .to_string(),
            )),
        );
    }
}

fn glob_tool_description() -> String {
    r#"- Fast file pattern matching tool that works with any codebase size
- Supports glob patterns like "**/*.js" or "src/**/*.ts"
- Returns matching file paths sorted by modification time
- Use this tool when you need to find files by name patterns
- When you are doing an open ended search that may require multiple rounds of globbing and grepping, use the Agent tool instead"#
        .to_string()
}

fn grep_tool_description() -> String {
    r#"A powerful search tool built on ripgrep

  Usage:
  - ALWAYS use Grep for search tasks. NEVER invoke `grep` or `rg` as a Bash command. The Grep tool has been optimized for correct permissions and access.
  - Supports full regex syntax (e.g., "log.*Error", "function\s+\w+")
  - Filter files with glob parameter (e.g., "*.js", "**/*.tsx") or type parameter (e.g., "js", "py", "rust")
  - Output modes: "content" shows matching lines, "files_with_matches" shows only file paths (default), "count" shows match counts
  - Use Agent tool for open-ended searches requiring multiple rounds
  - Pattern syntax: Uses ripgrep (not grep) - literal braces need escaping (use `interface\{\}` to find `interface{}` in Go code)
  - Multiline matching: By default patterns match within single lines only. For cross-line patterns like `struct \{[\s\S]*?field`, use `multiline: true`
"#
    .to_string()
}

#[cfg(test)]
#[path = "simple_search_tools_spec_tests.rs"]
mod tests;
