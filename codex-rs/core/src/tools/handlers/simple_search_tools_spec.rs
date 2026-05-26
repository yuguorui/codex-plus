use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

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
                "Glob pattern to match against files. Supports `*`, `?`, and `**`.".to_string(),
            )),
        ),
        (
            "path".to_string(),
            JsonSchema::string(Some(
                "Directory to search in. Defaults to the selected environment cwd.".to_string(),
            )),
        ),
    ]);
    maybe_add_environment_id(&mut properties, options);

    ToolSpec::Function(ResponsesApiTool {
        name: GLOB_TOOL_NAME.to_string(),
        description: "Find files by glob pattern. Results are sorted by most recently modified first and capped at 100 paths.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["pattern".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub(crate) fn create_grep_tool(options: SimpleSearchToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::from([
        (
            "pattern".to_string(),
            JsonSchema::string(Some("Regular expression pattern to search for.".to_string())),
        ),
        (
            "path".to_string(),
            JsonSchema::string(Some(
                "File or directory to search in. Defaults to the selected environment cwd."
                    .to_string(),
            )),
        ),
        (
            "glob".to_string(),
            JsonSchema::string(Some(
                "Optional glob filter for files under the search path, for example `*.rs`."
                    .to_string(),
            )),
        ),
        (
            "type".to_string(),
            JsonSchema::string(Some(
                "Optional file type filter such as `rust`, `python`, or an extension like `rs`."
                    .to_string(),
            )),
        ),
        (
            "output_mode".to_string(),
            JsonSchema::string_enum(
                vec![json!("files_with_matches"), json!("content"), json!("count")],
                Some(
                    "Output mode. Defaults to `files_with_matches`; use `content` for matching lines or `count` for per-file match counts."
                        .to_string(),
                ),
            ),
        ),
        (
            "multiline".to_string(),
            JsonSchema::boolean(Some(
                "Allow the regular expression to match across line boundaries.".to_string(),
            )),
        ),
        (
            "-B".to_string(),
            JsonSchema::number(Some(
                "Number of lines to show before each match. Requires output_mode: `content`."
                    .to_string(),
            )),
        ),
        (
            "-A".to_string(),
            JsonSchema::number(Some(
                "Number of lines to show after each match. Requires output_mode: `content`."
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
                "Number of lines to show before and after each match. Requires output_mode: `content`."
                    .to_string(),
            )),
        ),
        (
            "-n".to_string(),
            JsonSchema::boolean(Some(
                "Show line numbers in content output. Defaults to true.".to_string(),
            )),
        ),
        (
            "-i".to_string(),
            JsonSchema::boolean(Some("Case insensitive search.".to_string())),
        ),
        (
            "head_limit".to_string(),
            JsonSchema::number(Some(
                "Limit output to first N lines or entries. Defaults to 250 when unspecified. Pass 0 for unlimited."
                    .to_string(),
            )),
        ),
        (
            "offset".to_string(),
            JsonSchema::number(Some(
                "Skip first N lines or entries before applying head_limit. Defaults to 0."
                    .to_string(),
            )),
        ),
    ]);
    maybe_add_environment_id(&mut properties, options);

    ToolSpec::Function(ResponsesApiTool {
        name: GREP_TOOL_NAME.to_string(),
        description: "Search file contents with a regular expression. Output is compact and capped at 100 results.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["pattern".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
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

#[cfg(test)]
#[path = "simple_search_tools_spec_tests.rs"]
mod tests;
