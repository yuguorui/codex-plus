use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HashlineToolOptions {
    pub include_environment_id: bool,
}

pub(crate) fn create_hashline_tool(options: HashlineToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::from([
        (
            "action".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("read"),
                    json!("edit"),
                    json!("insert"),
                    json!("delete"),
                ],
                Some("Operation to perform on the file.".to_string()),
            ),
        ),
        (
            "path".to_string(),
            JsonSchema::string(Some(
                "Path to the target text file, relative to the selected environment cwd unless absolute."
                    .to_string(),
            )),
        ),
        (
            "anchor".to_string(),
            JsonSchema::string(Some(
                "Hashline anchor such as `LINE:HASH` (e.g. `12:ab`), bare hash (e.g. `ab`), bare line number for read (e.g. `12`), or inclusive range (`12:ab..15:ef`, `12..15`, `12..15:ef`, `12..`, `..50`). Edit/insert/delete require strict anchors: `LINE:HASH` must still match that exact line, while bare hashes are allowed only when they uniquely identify one line. Bare line numbers are only valid for read."
                    .to_string(),
            )),
        ),
        (
            "content".to_string(),
            JsonSchema::string(Some(
                "Replacement or inserted text. Multi-line content is supported for edit and insert."
                    .to_string(),
            )),
        ),
        (
            "before".to_string(),
            JsonSchema::boolean(Some(
                "For insert only, insert before the anchor instead of after it.".to_string(),
            )),
        ),
        (
            "context".to_string(),
            JsonSchema::integer(Some(
                "Number of context lines to return around an anchor or changed region. Defaults to 2."
                    .to_string(),
            )),
        ),
    ]);
    if options.include_environment_id {
        properties.insert(
            "environment_id".to_string(),
            JsonSchema::string(Some(
                "Optional selected environment id to target. Omit this to use the primary environment."
                    .to_string(),
            )),
        );
    }

    ToolSpec::Function(ResponsesApiTool {
        name: "fuzz_view_edit".to_string(),
        description: "Read or edit text files using line-hash anchors (`LINE:HASH|content`). Hashes are 2-character xxHash anchors over each line with trailing whitespace ignored. Writes use strict stale checks for `LINE:HASH` anchors and reject ambiguous bare-hash targets. Read supports bare line numbers and ranges (e.g. `12`, `12..15`, `12..`, `..50`), clamps out-of-range line ranges where possible, truncates very long displayed lines, and defaults to the first 500 lines when no anchor is given.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["action".to_string(), "path".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

#[cfg(test)]
#[path = "hashline_spec_tests.rs"]
mod tests;
