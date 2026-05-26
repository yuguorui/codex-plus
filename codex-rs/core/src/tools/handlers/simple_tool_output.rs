use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_tools::ToolOutput;
use codex_tools::ToolPayload;
use serde::Serialize;
use serde_json::Value as JsonValue;
use serde_json::json;

use crate::tools::TELEMETRY_PREVIEW_MAX_BYTES;
use crate::tools::TELEMETRY_PREVIEW_MAX_LINES;
use crate::tools::TELEMETRY_PREVIEW_TRUNCATION_NOTICE;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GlobOutput {
    /// Time taken to execute the search in milliseconds.
    pub(crate) duration_ms: u64,
    /// Total number of files found.
    pub(crate) num_files: usize,
    /// Array of file paths that match the pattern.
    pub(crate) filenames: Vec<String>,
    /// Whether results were truncated (limited to 100 files).
    pub(crate) truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GrepOutputModeSchema {
    Content,
    FilesWithMatches,
    Count,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GrepOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) mode: Option<GrepOutputModeSchema>,
    pub(crate) num_files: usize,
    pub(crate) filenames: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) num_lines: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) num_matches: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) applied_limit: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) applied_offset: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StructuredPatchHunk {
    pub(crate) old_start: usize,
    pub(crate) old_lines: usize,
    pub(crate) new_start: usize,
    pub(crate) new_lines: usize,
    pub(crate) lines: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)]
pub(crate) enum GitDiffStatus {
    Modified,
    Added,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GitDiff {
    pub(crate) filename: String,
    pub(crate) status: GitDiffStatus,
    pub(crate) additions: usize,
    pub(crate) deletions: usize,
    pub(crate) changes: usize,
    pub(crate) patch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) repository: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditOutput {
    /// The file path that was edited.
    pub(crate) file_path: String,
    /// The original string that was replaced.
    pub(crate) old_string: String,
    /// The new string that replaced it.
    pub(crate) new_string: String,
    /// The original file contents before editing.
    pub(crate) original_file: String,
    /// Diff patch showing the changes.
    pub(crate) structured_patch: Vec<StructuredPatchHunk>,
    /// Whether the user modified the proposed changes.
    pub(crate) user_modified: bool,
    /// Whether all occurrences were replaced.
    pub(crate) replace_all: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) git_diff: Option<GitDiff>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum WriteOutputType {
    Create,
    Update,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WriteOutput {
    /// Whether a new file was created or an existing file was updated.
    pub(crate) r#type: WriteOutputType,
    /// The path to the file that was written.
    pub(crate) file_path: String,
    /// The content that was written to the file.
    pub(crate) content: String,
    /// Diff patch showing the changes.
    pub(crate) structured_patch: Vec<StructuredPatchHunk>,
    /// The original file content before the write (null for new files).
    pub(crate) original_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) git_diff: Option<GitDiff>,
}

pub(crate) trait GeneratedOutputSchema {
    fn output_schema() -> JsonValue;
}

pub(crate) fn generated_output_schema<T>() -> JsonValue
where
    T: GeneratedOutputSchema,
{
    T::output_schema()
}

impl GeneratedOutputSchema for GlobOutput {
    fn output_schema() -> JsonValue {
        object_schema(
            [
                (
                    "durationMs",
                    json!({
                        "type": "integer",
                        "description": "Time taken to execute the search in milliseconds",
                    }),
                ),
                (
                    "numFiles",
                    json!({
                        "type": "integer",
                        "description": "Total number of files found",
                    }),
                ),
                (
                    "filenames",
                    json!({
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Array of file paths that match the pattern",
                    }),
                ),
                (
                    "truncated",
                    json!({
                        "type": "boolean",
                        "description": "Whether results were truncated (limited to 100 files)",
                    }),
                ),
            ],
            ["durationMs", "numFiles", "filenames", "truncated"],
        )
    }
}

impl GeneratedOutputSchema for GrepOutput {
    fn output_schema() -> JsonValue {
        object_schema(
            [
                (
                    "mode",
                    json!({
                        "type": "string",
                        "enum": ["content", "files_with_matches", "count"],
                    }),
                ),
                ("numFiles", json!({ "type": "integer" })),
                (
                    "filenames",
                    json!({
                        "type": "array",
                        "items": { "type": "string" },
                    }),
                ),
                ("content", json!({ "type": "string" })),
                ("numLines", json!({ "type": "integer" })),
                ("numMatches", json!({ "type": "integer" })),
                ("appliedLimit", json!({ "type": "integer" })),
                ("appliedOffset", json!({ "type": "integer" })),
            ],
            ["numFiles", "filenames"],
        )
    }
}

impl GeneratedOutputSchema for EditOutput {
    fn output_schema() -> JsonValue {
        object_schema(
            [
                (
                    "filePath",
                    json!({
                        "type": "string",
                        "description": "The file path that was edited",
                    }),
                ),
                (
                    "oldString",
                    json!({
                        "type": "string",
                        "description": "The original string that was replaced",
                    }),
                ),
                (
                    "newString",
                    json!({
                        "type": "string",
                        "description": "The new string that replaced it",
                    }),
                ),
                (
                    "originalFile",
                    json!({
                        "type": "string",
                        "description": "The original file contents before editing",
                    }),
                ),
                (
                    "structuredPatch",
                    json!({
                        "type": "array",
                        "items": structured_patch_hunk_schema(),
                        "description": "Diff patch showing the changes",
                    }),
                ),
                (
                    "userModified",
                    json!({
                        "type": "boolean",
                        "description": "Whether the user modified the proposed changes",
                    }),
                ),
                (
                    "replaceAll",
                    json!({
                        "type": "boolean",
                        "description": "Whether all occurrences were replaced",
                    }),
                ),
                ("gitDiff", git_diff_schema()),
            ],
            [
                "filePath",
                "oldString",
                "newString",
                "originalFile",
                "structuredPatch",
                "userModified",
                "replaceAll",
            ],
        )
    }
}

impl GeneratedOutputSchema for WriteOutput {
    fn output_schema() -> JsonValue {
        object_schema(
            [
                (
                    "type",
                    json!({
                        "type": "string",
                        "enum": ["create", "update"],
                        "description": "Whether a new file was created or an existing file was updated",
                    }),
                ),
                (
                    "filePath",
                    json!({
                        "type": "string",
                        "description": "The path to the file that was written",
                    }),
                ),
                (
                    "content",
                    json!({
                        "type": "string",
                        "description": "The content that was written to the file",
                    }),
                ),
                (
                    "structuredPatch",
                    json!({
                        "type": "array",
                        "items": structured_patch_hunk_schema(),
                        "description": "Diff patch showing the changes",
                    }),
                ),
                (
                    "originalFile",
                    json!({
                        "type": ["string", "null"],
                        "description": "The original file content before the write (null for new files)",
                    }),
                ),
                ("gitDiff", git_diff_schema()),
            ],
            [
                "type",
                "filePath",
                "content",
                "structuredPatch",
                "originalFile",
            ],
        )
    }
}

fn object_schema(
    properties: impl IntoIterator<Item = (&'static str, JsonValue)>,
    required: impl IntoIterator<Item = &'static str>,
) -> JsonValue {
    json!({
        "type": "object",
        "properties": properties
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect::<serde_json::Map<_, _>>(),
        "required": required
            .into_iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        "additionalProperties": false,
    })
}

fn structured_patch_hunk_schema() -> JsonValue {
    object_schema(
        [
            ("oldStart", json!({ "type": "integer" })),
            ("oldLines", json!({ "type": "integer" })),
            ("newStart", json!({ "type": "integer" })),
            ("newLines", json!({ "type": "integer" })),
            (
                "lines",
                json!({
                    "type": "array",
                    "items": { "type": "string" },
                }),
            ),
        ],
        ["oldStart", "oldLines", "newStart", "newLines", "lines"],
    )
}

fn git_diff_schema() -> JsonValue {
    object_schema(
        [
            ("filename", json!({ "type": "string" })),
            (
                "status",
                json!({
                    "type": "string",
                    "enum": ["modified", "added"],
                }),
            ),
            ("additions", json!({ "type": "integer" })),
            ("deletions", json!({ "type": "integer" })),
            ("changes", json!({ "type": "integer" })),
            ("patch", json!({ "type": "string" })),
            (
                "repository",
                json!({
                    "type": ["string", "null"],
                    "description": "GitHub owner/repo when available",
                }),
            ),
        ],
        [
            "filename",
            "status",
            "additions",
            "deletions",
            "changes",
            "patch",
        ],
    )
}

pub(crate) fn structured_patch_from_unified_diff(diff: &str) -> Vec<StructuredPatchHunk> {
    let mut hunks = Vec::new();
    let mut current: Option<StructuredPatchHunk> = None;

    for line in diff.lines() {
        if let Some((old_start, old_lines, new_start, new_lines)) = parse_hunk_header(line) {
            if let Some(hunk) = current.take() {
                hunks.push(hunk);
            }
            current = Some(StructuredPatchHunk {
                old_start,
                old_lines,
                new_start,
                new_lines,
                lines: Vec::new(),
            });
        } else if let Some(hunk) = current.as_mut() {
            hunk.lines.push(line.to_string());
        }
    }

    if let Some(hunk) = current {
        hunks.push(hunk);
    }
    hunks
}

fn parse_hunk_header(line: &str) -> Option<(usize, usize, usize, usize)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old_range, rest) = rest.split_once(" +")?;
    let (new_range, _) = rest.split_once(" @@")?;
    let (old_start, old_lines) = parse_diff_range(old_range)?;
    let (new_start, new_lines) = parse_diff_range(new_range)?;
    Some((old_start, old_lines, new_start, new_lines))
}

fn parse_diff_range(range: &str) -> Option<(usize, usize)> {
    match range.split_once(',') {
        Some((start, lines)) => Some((start.parse().ok()?, lines.parse().ok()?)),
        None => Some((range.parse().ok()?, 1)),
    }
}

pub(crate) struct TextStructuredOutput {
    text: String,
    structured: JsonValue,
    success: Option<bool>,
}

impl TextStructuredOutput {
    pub(crate) fn new(text: String, structured: impl Serialize) -> Self {
        let structured = serde_json::to_value(structured).unwrap_or_else(|err| {
            JsonValue::String(format!("failed to serialize structured tool output: {err}"))
        });
        Self {
            text,
            structured,
            success: Some(true),
        }
    }
}

impl ToolOutput for TextStructuredOutput {
    fn log_preview(&self) -> String {
        telemetry_preview(&self.text)
    }

    fn success_for_logging(&self) -> bool {
        self.success.unwrap_or(true)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        let output = FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(self.text.clone()),
            success: self.success,
        };

        if matches!(payload, ToolPayload::Custom { .. }) {
            return ResponseInputItem::CustomToolCallOutput {
                call_id: call_id.to_string(),
                name: None,
                output,
            };
        }

        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output,
        }
    }

    fn post_tool_use_response(&self, _call_id: &str, _payload: &ToolPayload) -> Option<JsonValue> {
        Some(self.structured.clone())
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        self.structured.clone()
    }
}

fn telemetry_preview(content: &str) -> String {
    let truncated_slice =
        codex_utils_string::take_bytes_at_char_boundary(content, TELEMETRY_PREVIEW_MAX_BYTES);
    let truncated_by_bytes = truncated_slice.len() < content.len();

    let mut preview = String::new();
    let mut lines_iter = truncated_slice.lines();
    for idx in 0..TELEMETRY_PREVIEW_MAX_LINES {
        match lines_iter.next() {
            Some(line) => {
                if idx > 0 {
                    preview.push('\n');
                }
                preview.push_str(line);
            }
            None => break,
        }
    }

    if truncated_by_bytes || lines_iter.next().is_some() {
        if !preview.is_empty() {
            preview.push('\n');
        }
        preview.push_str(TELEMETRY_PREVIEW_TRUNCATION_NOTICE);
    }

    preview
}
