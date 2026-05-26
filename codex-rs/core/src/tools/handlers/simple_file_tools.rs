use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_exec_server::FileSystemSandboxContext;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::items::McpToolCallError;
use codex_protocol::items::McpToolCallItem;
use codex_protocol::items::McpToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::function_call_output_content_items_to_text;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::PatchApplyStatus;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de;
use serde_json::Value as JsonValue;
use serde_json::json;
use similar::TextDiff;
use std::collections::HashMap;
use std::io;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

use crate::function_tool::FunctionCallError;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::handlers::ExecCommandHandler;
use crate::tools::handlers::ExecCommandHandlerOptions;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::handlers::simple_file_tools_spec::SimpleFileToolOptions;
use crate::tools::handlers::simple_file_tools_spec::create_bash_tool;
use crate::tools::handlers::simple_file_tools_spec::create_edit_tool;
use crate::tools::handlers::simple_file_tools_spec::create_read_tool;
use crate::tools::handlers::simple_file_tools_spec::create_write_tool;
use crate::tools::handlers::simple_tool_output::EditOutput;
use crate::tools::handlers::simple_tool_output::TextStructuredOutput;
use crate::tools::handlers::simple_tool_output::WriteOutput;
use crate::tools::handlers::simple_tool_output::WriteOutputType;
use crate::tools::handlers::simple_tool_output::structured_patch_from_unified_diff;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PreToolUsePayload;
use codex_tools::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolOutput;
use codex_tools::ToolSpec;

const DEFAULT_BASH_YIELD_TIME_MS: u64 = 10_000;
const BACKGROUND_BASH_YIELD_TIME_MS: u64 = 1_000;
const DEFAULT_READ_LINE_LIMIT: usize = 2_000;
const PDFINFO_TIMEOUT: Duration = Duration::from_secs(10);
const PDFTOTEXT_TIMEOUT: Duration = Duration::from_secs(120);
const FILE_UNCHANGED_STUB: &str = "File unchanged since last read. The content from the earlier Read tool_result in this conversation is still current — refer to that instead of re-reading.";

pub(crate) struct BashHandler {
    options: SimpleFileToolOptions,
    exec: ExecCommandHandler,
}

impl BashHandler {
    pub(crate) fn new(
        options: SimpleFileToolOptions,
        exec_options: ExecCommandHandlerOptions,
    ) -> Self {
        Self {
            options,
            exec: ExecCommandHandler::new(exec_options),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct BashArgs {
    command: String,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    timeout: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_bool")]
    run_in_background: bool,
    #[serde(
        default,
        rename = "dangerouslyDisableSandbox",
        deserialize_with = "deserialize_bool"
    )]
    dangerously_disable_sandbox: bool,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    environment_id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ReadArgs {
    file_path: String,
    #[serde(default, deserialize_with = "deserialize_optional_usize")]
    offset: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_optional_usize")]
    limit: Option<usize>,
    #[serde(default)]
    pages: Option<String>,
    #[serde(default)]
    environment_id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct EditArgs {
    file_path: String,
    old_string: String,
    new_string: String,
    #[serde(default, deserialize_with = "deserialize_bool")]
    replace_all: bool,
    #[serde(default)]
    environment_id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct WriteArgs {
    file_path: String,
    content: String,
    #[serde(default)]
    environment_id: Option<String>,
}

pub(crate) struct ReadHandler {
    options: SimpleFileToolOptions,
}

impl ReadHandler {
    pub(crate) fn new(options: SimpleFileToolOptions) -> Self {
        Self { options }
    }
}

pub(crate) struct EditHandler {
    options: SimpleFileToolOptions,
}

impl EditHandler {
    pub(crate) fn new(options: SimpleFileToolOptions) -> Self {
        Self { options }
    }
}

pub(crate) struct WriteHandler {
    options: SimpleFileToolOptions,
}

impl WriteHandler {
    pub(crate) fn new(options: SimpleFileToolOptions) -> Self {
        Self { options }
    }
}

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for BashHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("Bash")
    }

    fn spec(&self) -> ToolSpec {
        create_bash_tool(self.options)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    async fn handle(
        &self,
        mut invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return Err(FunctionCallError::RespondToModel(
                "Bash handler received unsupported payload".to_string(),
            ));
        };
        let args: BashArgs = parse_arguments(arguments)?;
        let yield_time_ms = if args.run_in_background {
            BACKGROUND_BASH_YIELD_TIME_MS
        } else {
            DEFAULT_BASH_YIELD_TIME_MS
        };
        let mut exec_arguments = serde_json::json!({
            "cmd": args.command,
            "yield_time_ms": yield_time_ms,
            "timeout_ms": args.timeout,
            "environment_id": args.environment_id,
        });
        if args.dangerously_disable_sandbox {
            exec_arguments["sandbox_permissions"] = serde_json::json!("require_escalated");
            exec_arguments["justification"] = serde_json::json!(
                "Do you want to run this Bash command without sandbox restrictions?"
            );
        }
        invocation.tool_name = ToolName::plain("exec_command");
        invocation.payload = ToolPayload::Function {
            arguments: exec_arguments.to_string(),
        };
        self.exec.handle(invocation).await
    }
}

impl CoreToolRuntime for BashHandler {
    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };
        let args: BashArgs = parse_arguments(arguments).ok()?;
        let _ = args.description.as_deref();
        Some(PreToolUsePayload {
            tool_name: HookToolName::bash(),
            tool_input: serde_json::json!({ "command": args.command }),
        })
    }

    fn with_updated_hook_input(
        &self,
        mut invocation: ToolInvocation,
        updated_input: serde_json::Value,
    ) -> Result<ToolInvocation, FunctionCallError> {
        let ToolPayload::Function { arguments } = invocation.payload else {
            return Err(FunctionCallError::RespondToModel(
                "hook input rewrite received unsupported Bash payload".to_string(),
            ));
        };
        let mut value: serde_json::Value = parse_arguments(&arguments)?;
        let serde_json::Value::Object(arguments) = &mut value else {
            return Err(FunctionCallError::RespondToModel(
                "Bash arguments must be an object".to_string(),
            ));
        };
        let command = updated_input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "hook returned updatedInput without string field `command`".to_string(),
                )
            })?;
        arguments.insert(
            "command".to_string(),
            serde_json::Value::String(command.to_string()),
        );
        invocation.payload = ToolPayload::Function {
            arguments: serde_json::to_string(arguments).map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "failed to serialize rewritten Bash arguments: {err}"
                ))
            })?,
        };
        Ok(invocation)
    }
}

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for ReadHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("Read")
    }

    fn spec(&self) -> ToolSpec {
        create_read_tool(self.options)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            tracker,
            call_id,
            payload,
            ..
        } = invocation;
        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(
                "Read handler received unsupported payload".to_string(),
            ));
        };
        let args: ReadArgs = parse_arguments(&arguments)?;
        let (turn_environment, path, sandbox) = resolve_file_target(
            turn.as_ref(),
            args.environment_id.as_deref(),
            &args.file_path,
        )?;
        let is_pdf = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"));
        if args.pages.is_some() && !is_pdf {
            return Err(FunctionCallError::RespondToModel(
                "Read pages is only supported for PDF files.".to_string(),
            ));
        }
        emit_simple_tool_started(session.as_ref(), turn.as_ref(), &call_id, "Read", &args).await;
        if let Some(mime_type) = image_mime_type(&path) {
            let read_result =
                read_image_file(&turn_environment, &path, Some(&sandbox), mime_type).await;
            return match read_result {
                Ok(read_image) => {
                    emit_simple_tool_completed(
                        session.as_ref(),
                        turn.as_ref(),
                        &call_id,
                        "Read",
                        &args,
                    )
                    .await;
                    let output = ReadToolOutput::image(
                        read_image.mime_type,
                        read_image.base64,
                        read_image.original_size,
                    );
                    Ok(boxed_tool_output(output))
                }
                Err(err) => {
                    emit_simple_tool_failed(
                        session.as_ref(),
                        turn.as_ref(),
                        &call_id,
                        "Read",
                        &args,
                        &err.to_string(),
                    )
                    .await;
                    Err(err)
                }
            };
        }
        let read_result = async {
            if is_pdf {
                read_pdf_text_file(
                    &turn_environment,
                    &path,
                    Some(&sandbox),
                    args.pages.as_deref(),
                )
                .await
            } else {
                read_text_file(&turn_environment, &path, Some(&sandbox), "Read").await
            }
        }
        .await;
        let text = match read_result {
            Ok(text) => text,
            Err(err) => {
                emit_simple_tool_failed(
                    session.as_ref(),
                    turn.as_ref(),
                    &call_id,
                    "Read",
                    &args,
                    &err.to_string(),
                )
                .await;
                return Err(err);
            }
        };
        if !is_pdf && is_full_text_read(&text, args.offset, args.limit) {
            let current_snapshot = crate::turn_diff_tracker::FileReadSnapshot::new(&text);
            let read_snapshot = tracker
                .lock()
                .await
                .simple_file_read_tool_snapshot(path.as_path());
            if read_snapshot == Some(current_snapshot) {
                emit_simple_tool_completed(
                    session.as_ref(),
                    turn.as_ref(),
                    &call_id,
                    "Read",
                    &args,
                )
                .await;
                return Ok(boxed_tool_output(ReadToolOutput::file_unchanged(
                    path.display().to_string(),
                )));
            }
            tracker
                .lock()
                .await
                .record_simple_file_read_tool(path.as_path().to_path_buf(), &text);
        }
        emit_simple_tool_completed(session.as_ref(), turn.as_ref(), &call_id, "Read", &args).await;
        let result_type = if is_pdf { "pdf" } else { "text" };
        Ok(boxed_tool_output(ReadToolOutput::text(
            result_type,
            path.display().to_string(),
            &text,
            args.offset,
            args.limit,
        )))
    }
}

impl CoreToolRuntime for ReadHandler {}

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for EditHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("Edit")
    }

    fn spec(&self) -> ToolSpec {
        create_edit_tool(self.options)
    }

    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            tracker,
            call_id,
            payload,
            ..
        } = invocation;
        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(
                "Edit handler received unsupported payload".to_string(),
            ));
        };
        let args: EditArgs = parse_arguments(&arguments)?;
        if args.old_string.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "Edit old_string must not be empty".to_string(),
            ));
        }
        let (turn_environment, path, sandbox) = resolve_file_target(
            turn.as_ref(),
            args.environment_id.as_deref(),
            &args.file_path,
        )?;
        let before = read_text_file(&turn_environment, &path, Some(&sandbox), "Edit").await?;
        ensure_existing_file_was_read(&tracker, &path, &before, "Edit").await?;
        let matches = before.match_indices(&args.old_string).count();
        if matches == 0 {
            return Err(FunctionCallError::RespondToModel(format!(
                "Edit found no match for old_string in `{}`",
                path.display()
            )));
        }
        if matches > 1 && !args.replace_all {
            return Err(FunctionCallError::RespondToModel(format!(
                "Edit found {matches} matches for old_string in `{}`; set replace_all to true to replace all matches",
                path.display()
            )));
        }
        let after = if args.replace_all {
            before.replace(&args.old_string, &args.new_string)
        } else {
            before.replacen(&args.old_string, &args.new_string, 1)
        };
        let unified_diff = unified_diff(&before, &after, &path);
        let file_change = FileChange::Update {
            unified_diff: unified_diff.clone(),
            move_path: None,
        };
        let emitter = emit_file_change_begin(
            session.as_ref(),
            turn.as_ref(),
            &call_id,
            path.as_path().to_path_buf(),
            file_change,
        )
        .await;
        let write_result = write_text_file(
            &turn_environment,
            &path,
            after.clone(),
            Some(&sandbox),
            "Edit",
        )
        .await;
        let file_change_status = if write_result.is_ok() {
            Some(PatchApplyStatus::Completed)
        } else {
            Some(PatchApplyStatus::Failed)
        };
        emit_file_change_finish(
            session.as_ref(),
            turn.as_ref(),
            &tracker,
            &call_id,
            emitter,
            file_change_status,
        )
        .await?;
        write_result?;
        tracker
            .lock()
            .await
            .record_simple_file_read(path.as_path().to_path_buf(), &after);
        let output = EditOutput {
            file_path: path.display().to_string(),
            old_string: args.old_string,
            new_string: args.new_string,
            original_file: before,
            structured_patch: structured_patch_from_unified_diff(&unified_diff),
            user_modified: false,
            replace_all: args.replace_all,
            git_diff: None,
        };
        Ok(boxed_tool_output(TextStructuredOutput::new(
            format!("Edited `{}`.", path.display()),
            output,
        )))
    }
}

impl CoreToolRuntime for EditHandler {}

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for WriteHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("Write")
    }

    fn spec(&self) -> ToolSpec {
        create_write_tool(self.options)
    }

    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            tracker,
            call_id,
            payload,
            ..
        } = invocation;
        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(
                "Write handler received unsupported payload".to_string(),
            ));
        };
        let args: WriteArgs = parse_arguments(&arguments)?;
        let (turn_environment, path, sandbox) = resolve_file_target(
            turn.as_ref(),
            args.environment_id.as_deref(),
            &args.file_path,
        )?;
        let before =
            read_optional_text_file(&turn_environment, &path, Some(&sandbox), "Write").await?;
        if let Some(before) = before.as_deref() {
            ensure_existing_file_was_read(&tracker, &path, before, "Write").await?;
        }
        let unified_diff =
            unified_diff(before.as_deref().unwrap_or_default(), &args.content, &path);
        let file_change = match before.as_ref() {
            Some(_) => FileChange::Update {
                unified_diff: unified_diff.clone(),
                move_path: None,
            },
            None => FileChange::Add {
                content: args.content.clone(),
            },
        };
        let emitter = emit_file_change_begin(
            session.as_ref(),
            turn.as_ref(),
            &call_id,
            path.as_path().to_path_buf(),
            file_change,
        )
        .await;
        let write_result = write_text_file(
            &turn_environment,
            &path,
            args.content.clone(),
            Some(&sandbox),
            "Write",
        )
        .await;
        let file_change_status = if write_result.is_ok() {
            Some(PatchApplyStatus::Completed)
        } else {
            Some(PatchApplyStatus::Failed)
        };
        emit_file_change_finish(
            session.as_ref(),
            turn.as_ref(),
            &tracker,
            &call_id,
            emitter,
            file_change_status,
        )
        .await?;
        write_result?;
        tracker
            .lock()
            .await
            .record_simple_file_read(path.as_path().to_path_buf(), &args.content);
        let output = WriteOutput {
            r#type: if before.is_some() {
                WriteOutputType::Update
            } else {
                WriteOutputType::Create
            },
            file_path: path.display().to_string(),
            content: args.content,
            structured_patch: structured_patch_from_unified_diff(&unified_diff),
            original_file: before,
            git_diff: None,
        };
        Ok(boxed_tool_output(TextStructuredOutput::new(
            format!("Wrote `{}`.", path.display()),
            output,
        )))
    }
}

impl CoreToolRuntime for WriteHandler {}

fn resolve_file_target(
    turn: &crate::session::turn_context::TurnContext,
    environment_id: Option<&str>,
    file_path: &str,
) -> Result<(TurnEnvironment, AbsolutePathBuf, FileSystemSandboxContext), FunctionCallError> {
    let turn_environment = resolve_tool_environment(turn, environment_id)?.ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "simple file tools are unavailable in this session".to_string(),
        )
    })?;
    let cwd = turn_environment.cwd.clone();
    let path = AbsolutePathBuf::resolve_path_against_base(PathBuf::from(file_path), &cwd);
    let sandbox = turn.file_system_sandbox_context(/*additional_permissions*/ None, &cwd);
    Ok((turn_environment.clone(), path, sandbox))
}

struct ReadImage {
    base64: String,
    mime_type: &'static str,
    original_size: usize,
}

struct ReadToolOutput {
    body: Vec<FunctionCallOutputContentItem>,
    code_mode_result: JsonValue,
    success: Option<bool>,
}

impl ReadToolOutput {
    fn text(
        result_type: &str,
        file_path: String,
        text: &str,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Self {
        let read_range = read_range(text, offset, limit);
        let formatted_content = read_range.formatted_content;
        let content = read_range.content;
        Self {
            body: vec![FunctionCallOutputContentItem::InputText {
                text: formatted_content,
            }],
            code_mode_result: json!({
                "type": result_type,
                "file": {
                    "filePath": file_path,
                    "content": content,
                    "numLines": read_range.num_lines,
                    "startLine": read_range.start_line,
                    "totalLines": read_range.total_lines,
                },
            }),
            success: Some(true),
        }
    }

    fn image(mime_type: &'static str, base64: String, original_size: usize) -> Self {
        let image_url = format!("data:{mime_type};base64,{base64}");
        Self {
            body: vec![FunctionCallOutputContentItem::InputImage {
                image_url,
                detail: Some(DEFAULT_IMAGE_DETAIL),
            }],
            code_mode_result: json!({
                "type": "image",
                "file": {
                    "base64": base64,
                    "type": mime_type,
                    "originalSize": original_size,
                },
            }),
            success: Some(true),
        }
    }

    fn file_unchanged(file_path: String) -> Self {
        Self {
            body: vec![FunctionCallOutputContentItem::InputText {
                text: FILE_UNCHANGED_STUB.to_string(),
            }],
            code_mode_result: json!({
                "type": "file_unchanged",
                "file": {
                    "filePath": file_path,
                },
            }),
            success: Some(true),
        }
    }
}

impl ToolOutput for ReadToolOutput {
    fn log_preview(&self) -> String {
        function_call_output_content_items_to_text(&self.body)
            .unwrap_or_else(|| self.code_mode_result.to_string())
    }

    fn success_for_logging(&self) -> bool {
        self.success.unwrap_or(true)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        let body = match self.body.as_slice() {
            [FunctionCallOutputContentItem::InputText { text }] => {
                FunctionCallOutputBody::Text(text.clone())
            }
            _ => FunctionCallOutputBody::ContentItems(self.body.clone()),
        };
        let output = FunctionCallOutputPayload {
            body,
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

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        self.code_mode_result.clone()
    }
}

fn image_mime_type(path: &AbsolutePathBuf) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?;
    match extension.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

async fn read_image_file(
    turn_environment: &TurnEnvironment,
    path: &AbsolutePathBuf,
    sandbox: Option<&FileSystemSandboxContext>,
    mime_type: &'static str,
) -> Result<ReadImage, FunctionCallError> {
    let bytes = read_binary_file(turn_environment, path, sandbox, "Read").await?;
    if bytes.is_empty() {
        return Err(FunctionCallError::RespondToModel(format!(
            "Read failed to read `{}`: image file is empty",
            path.display()
        )));
    }
    if !image_bytes_match_mime_type(&bytes, mime_type) {
        return Err(FunctionCallError::RespondToModel(format!(
            "Read failed to read `{}`: file contents do not match image type {mime_type}",
            path.display()
        )));
    }
    let original_size = bytes.len();
    Ok(ReadImage {
        base64: BASE64_STANDARD.encode(bytes),
        mime_type,
        original_size,
    })
}

fn image_bytes_match_mime_type(bytes: &[u8], mime_type: &str) -> bool {
    match mime_type {
        "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.len() >= 12 && bytes.starts_with(b"RIFF") && bytes[8..12] == *b"WEBP",
        _ => false,
    }
}

async fn read_optional_text_file(
    turn_environment: &TurnEnvironment,
    path: &AbsolutePathBuf,
    sandbox: Option<&FileSystemSandboxContext>,
    tool_name: &str,
) -> Result<Option<String>, FunctionCallError> {
    let fs = turn_environment.environment.get_filesystem();
    match fs.read_file(path, sandbox).await {
        Ok(bytes) => decode_utf8(bytes, path, tool_name).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(FunctionCallError::RespondToModel(format!(
            "{tool_name} failed to read `{}`: {error}",
            path.display()
        ))),
    }
}

async fn read_text_file(
    turn_environment: &TurnEnvironment,
    path: &AbsolutePathBuf,
    sandbox: Option<&FileSystemSandboxContext>,
    tool_name: &str,
) -> Result<String, FunctionCallError> {
    read_optional_text_file(turn_environment, path, sandbox, tool_name)
        .await?
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(format!(
                "{tool_name} failed to read `{}`: file does not exist",
                path.display()
            ))
        })
}

async fn read_pdf_text_file(
    turn_environment: &TurnEnvironment,
    path: &AbsolutePathBuf,
    sandbox: Option<&FileSystemSandboxContext>,
    pages: Option<&str>,
) -> Result<String, FunctionCallError> {
    let bytes = read_binary_file(turn_environment, path, sandbox, "Read").await?;
    if bytes.is_empty() {
        return Err(FunctionCallError::RespondToModel(format!(
            "Read failed to read `{}`: PDF file is empty",
            path.display()
        )));
    }
    if !bytes.starts_with(b"%PDF-") {
        return Err(FunctionCallError::RespondToModel(format!(
            "Read failed to read `{}`: file is not a valid PDF",
            path.display()
        )));
    }
    let page_range = pages.map(parse_pdf_page_range).transpose()?;
    let mut temp_file = tempfile::Builder::new()
        .suffix(".pdf")
        .tempfile()
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "Read failed to prepare PDF `{}`: {error}",
                path.display()
            ))
        })?;
    temp_file.write_all(&bytes).map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "Read failed to prepare PDF `{}`: {error}",
            path.display()
        ))
    })?;
    let temp_path = temp_file.path().to_path_buf();
    if let Some(page_range) = &page_range {
        let page_count = pdf_page_count(&temp_path).await?;
        validate_pdf_page_range(page_range, page_count)?;
    }
    let extracted = pdftotext(&temp_path, page_range.as_ref()).await?;
    if extracted.trim().is_empty() {
        return Ok(format!(
            "<system-reminder>Warning: PDF text extraction produced no text for `{}`.</system-reminder>",
            path.display()
        ));
    }
    let mut output = format!("PDF text extracted from `{}`", path.display());
    if let Some(page_range) = page_range {
        output.push_str(&format!(" pages {}", page_range.display()));
    }
    output.push_str(":\n");
    output.push_str(&extracted);
    Ok(output)
}

async fn read_binary_file(
    turn_environment: &TurnEnvironment,
    path: &AbsolutePathBuf,
    sandbox: Option<&FileSystemSandboxContext>,
    tool_name: &str,
) -> Result<Vec<u8>, FunctionCallError> {
    let fs = turn_environment.environment.get_filesystem();
    fs.read_file(path, sandbox).await.map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            FunctionCallError::RespondToModel(format!(
                "{tool_name} failed to read `{}`: file does not exist",
                path.display()
            ))
        } else {
            FunctionCallError::RespondToModel(format!(
                "{tool_name} failed to read `{}`: {error}",
                path.display()
            ))
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PdfPageRange {
    first_page: usize,
    last_page: Option<usize>,
}

impl PdfPageRange {
    fn display(self) -> String {
        match self.last_page {
            Some(last_page) if last_page == self.first_page => self.first_page.to_string(),
            Some(last_page) => format!("{}-{last_page}", self.first_page),
            None => format!("{}-", self.first_page),
        }
    }
}

fn parse_pdf_page_range(pages: &str) -> Result<PdfPageRange, FunctionCallError> {
    let trimmed = pages.trim();
    if trimmed.is_empty() {
        return Err(invalid_pdf_pages_error(pages));
    }
    if let Some(first) = trimmed.strip_suffix('-') {
        let first_page = parse_pdf_page_number(first, pages)?;
        return Ok(PdfPageRange {
            first_page,
            last_page: None,
        });
    }
    let Some((first, last)) = trimmed.split_once('-') else {
        let page = parse_pdf_page_number(trimmed, pages)?;
        return Ok(PdfPageRange {
            first_page: page,
            last_page: Some(page),
        });
    };
    let first_page = parse_pdf_page_number(first, pages)?;
    let last_page = parse_pdf_page_number(last, pages)?;
    if last_page < first_page {
        return Err(invalid_pdf_pages_error(pages));
    }
    Ok(PdfPageRange {
        first_page,
        last_page: Some(last_page),
    })
}

fn parse_pdf_page_number(value: &str, pages: &str) -> Result<usize, FunctionCallError> {
    value
        .parse::<usize>()
        .ok()
        .filter(|page| *page > 0)
        .ok_or_else(|| invalid_pdf_pages_error(pages))
}

fn invalid_pdf_pages_error(pages: &str) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!(
        "invalid PDF pages range `{pages}`; expected `5`, `1-10`, or `3-`"
    ))
}

fn validate_pdf_page_range(
    page_range: &PdfPageRange,
    page_count: usize,
) -> Result<(), FunctionCallError> {
    if page_range.first_page > page_count {
        return Err(FunctionCallError::RespondToModel(format!(
            "PDF pages range starts at page {}, but the PDF has only {page_count} pages",
            page_range.first_page
        )));
    }
    if let Some(last_page) = page_range.last_page
        && last_page > page_count
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "PDF pages range ends at page {last_page}, but the PDF has only {page_count} pages"
        )));
    }
    Ok(())
}

async fn pdf_page_count(path: &std::path::Path) -> Result<usize, FunctionCallError> {
    let output = run_poppler_command("pdfinfo", [path.as_os_str()], PDFINFO_TIMEOUT).await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .find_map(|line| {
            line.strip_prefix("Pages:")
                .and_then(|count| count.trim().parse::<usize>().ok())
        })
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "failed to determine PDF page count using pdfinfo".to_string(),
            )
        })
}

async fn pdftotext(
    path: &std::path::Path,
    page_range: Option<&PdfPageRange>,
) -> Result<String, FunctionCallError> {
    let mut command = Command::new("pdftotext");
    command.arg("-layout");
    if let Some(page_range) = page_range {
        command.arg("-f").arg(page_range.first_page.to_string());
        if let Some(last_page) = page_range.last_page {
            command.arg("-l").arg(last_page.to_string());
        }
    }
    command.arg(path).arg("-");
    let output = run_command(command, "pdftotext", PDFTOTEXT_TIMEOUT).await?;
    String::from_utf8(output.stdout).map_err(|error| {
        FunctionCallError::RespondToModel(format!("pdftotext returned non-UTF-8 output: {error}"))
    })
}

async fn run_poppler_command<I, S>(
    program: &str,
    args: I,
    timeout_duration: Duration,
) -> Result<std::process::Output, FunctionCallError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new(program);
    command.args(args);
    run_command(command, program, timeout_duration).await
}

async fn run_command(
    mut command: Command,
    program: &str,
    timeout_duration: Duration,
) -> Result<std::process::Output, FunctionCallError> {
    let output = timeout(timeout_duration, command.output())
        .await
        .map_err(|_| {
            FunctionCallError::RespondToModel(format!(
                "{program} timed out after {} seconds",
                timeout_duration.as_secs()
            ))
        })?
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                FunctionCallError::RespondToModel(format!(
                    "{program} is not installed. Install poppler (for example `brew install poppler` or `apt-get install poppler-utils`) to read PDF files."
                ))
            } else {
                FunctionCallError::RespondToModel(format!("failed to run {program}: {error}"))
            }
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(FunctionCallError::RespondToModel(poppler_failure_message(
            program, &stderr,
        )));
    }
    Ok(output)
}

fn poppler_failure_message(program: &str, stderr: &str) -> String {
    let lower_stderr = stderr.to_ascii_lowercase();
    if lower_stderr.contains("password") {
        "PDF is password-protected. Please provide an unprotected version.".to_string()
    } else if lower_stderr.contains("damaged")
        || lower_stderr.contains("corrupt")
        || lower_stderr.contains("invalid")
    {
        "PDF file is corrupted or invalid.".to_string()
    } else {
        format!("{program} failed: {stderr}")
    }
}

async fn write_text_file(
    turn_environment: &TurnEnvironment,
    path: &AbsolutePathBuf,
    content: String,
    sandbox: Option<&FileSystemSandboxContext>,
    tool_name: &str,
) -> Result<(), FunctionCallError> {
    let fs = turn_environment.environment.get_filesystem();
    fs.write_file(path, content.into_bytes(), sandbox)
        .await
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "{tool_name} failed to write `{}`: {error}",
                path.display()
            ))
        })
}

fn decode_utf8(
    bytes: Vec<u8>,
    path: &AbsolutePathBuf,
    tool_name: &str,
) -> Result<String, FunctionCallError> {
    String::from_utf8(bytes).map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "{tool_name} only supports UTF-8 text files; `{}` is not valid UTF-8: {error}",
            path.display()
        ))
    })
}

struct ReadRange {
    content: String,
    formatted_content: String,
    num_lines: usize,
    start_line: usize,
    total_lines: usize,
}

fn read_range(text: &str, offset: Option<usize>, limit: Option<usize>) -> ReadRange {
    let start_line = offset.unwrap_or(1).max(1);
    let total_lines = text.lines().count();
    let selected_lines = text
        .lines()
        .enumerate()
        .skip(start_line - 1)
        .take(limit.unwrap_or(DEFAULT_READ_LINE_LIMIT))
        .collect::<Vec<_>>();
    let shown = selected_lines.len();
    let content = selected_lines
        .iter()
        .map(|(_, line)| *line)
        .collect::<Vec<_>>()
        .join("\n");
    let mut formatted_content = selected_lines
        .iter()
        .map(|(line_index, line)| format!("{:>6}\t{line}", line_index + 1))
        .collect::<Vec<_>>()
        .join("\n");
    if start_line + shown <= total_lines {
        if !formatted_content.is_empty() {
            formatted_content.push('\n');
        }
        formatted_content.push_str(&format!(
            "... {} more lines. Use offset={} to continue.",
            total_lines - (start_line + shown - 1),
            start_line + shown
        ));
    }
    ReadRange {
        content,
        formatted_content,
        num_lines: shown,
        start_line,
        total_lines,
    }
}

fn is_full_text_read(text: &str, offset: Option<usize>, limit: Option<usize>) -> bool {
    let start_line = offset.unwrap_or(1).max(1);
    start_line == 1 && limit.unwrap_or(DEFAULT_READ_LINE_LIMIT) >= text.lines().count()
}

async fn ensure_existing_file_was_read(
    tracker: &SharedTurnDiffTracker,
    path: &AbsolutePathBuf,
    current_content: &str,
    tool_name: &str,
) -> Result<(), FunctionCallError> {
    let current_snapshot = crate::turn_diff_tracker::FileReadSnapshot::new(current_content);
    let read_snapshot = tracker
        .lock()
        .await
        .simple_file_read_snapshot(path.as_path());
    let Some(read_snapshot) = read_snapshot else {
        return Err(FunctionCallError::RespondToModel(format!(
            "{tool_name} must Read `{}` before modifying it.",
            path.display()
        )));
    };
    if read_snapshot != current_snapshot {
        return Err(FunctionCallError::RespondToModel(format!(
            "{tool_name} cannot modify `{}` because it changed after the last Read. Run Read again before modifying it.",
            path.display()
        )));
    }
    Ok(())
}

fn unified_diff(before: &str, after: &str, path: &AbsolutePathBuf) -> String {
    let path = path.display().to_string();
    TextDiff::from_lines(before, after)
        .unified_diff()
        .header(&path, &path)
        .context_radius(3)
        .to_string()
}

async fn emit_file_change_begin(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    call_id: &str,
    path: PathBuf,
    file_change: FileChange,
) -> ToolEmitter {
    let emitter = ToolEmitter::apply_patch(
        HashMap::from([(path, file_change)]),
        /*auto_approved*/ true,
    );
    let event_ctx = ToolEventCtx::new(session, turn, call_id, /*turn_diff_tracker*/ None);
    emitter.begin(event_ctx).await;
    emitter
}

async fn emit_file_change_finish(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    tracker: &crate::tools::context::SharedTurnDiffTracker,
    call_id: &str,
    emitter: ToolEmitter,
    status: Option<PatchApplyStatus>,
) -> Result<(), FunctionCallError> {
    let event_ctx = ToolEventCtx::new(session, turn, call_id, Some(tracker));
    let output = match status {
        Some(PatchApplyStatus::Completed) => successful_file_change_output(),
        Some(PatchApplyStatus::Failed) => failed_file_change_output(),
        Some(PatchApplyStatus::Declined) | None => successful_file_change_output(),
    };
    let finish_result = emitter.finish(event_ctx, Ok(output), None).await;
    if status == Some(PatchApplyStatus::Completed) {
        finish_result?;
    }
    Ok(())
}

fn successful_file_change_output() -> ExecToolCallOutput {
    ExecToolCallOutput {
        exit_code: 0,
        stdout: StreamOutput::new(String::new()),
        stderr: StreamOutput::new(String::new()),
        aggregated_output: StreamOutput::new(String::new()),
        duration: Duration::ZERO,
        timed_out: false,
    }
}

fn failed_file_change_output() -> ExecToolCallOutput {
    ExecToolCallOutput {
        exit_code: 1,
        stdout: StreamOutput::new(String::new()),
        stderr: StreamOutput::new(String::new()),
        aggregated_output: StreamOutput::new(String::new()),
        duration: Duration::ZERO,
        timed_out: false,
    }
}

async fn emit_simple_tool_started<T: serde::Serialize>(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    call_id: &str,
    tool_name: &str,
    args: &T,
) {
    session
        .emit_turn_item_started(
            turn,
            &TurnItem::McpToolCall(McpToolCallItem {
                id: call_id.to_string(),
                server: "codex++".to_string(),
                tool: tool_name.to_string(),
                arguments: serde_json::to_value(args).unwrap_or(serde_json::Value::Null),
                mcp_app_resource_uri: None,
                plugin_id: None,
                status: McpToolCallStatus::InProgress,
                result: None,
                error: None,
                duration: None,
            }),
        )
        .await;
}

async fn emit_simple_tool_completed<T: serde::Serialize>(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    call_id: &str,
    tool_name: &str,
    args: &T,
) {
    session
        .emit_turn_item_completed(
            turn,
            TurnItem::McpToolCall(McpToolCallItem {
                id: call_id.to_string(),
                server: "codex++".to_string(),
                tool: tool_name.to_string(),
                arguments: serde_json::to_value(args).unwrap_or(serde_json::Value::Null),
                mcp_app_resource_uri: None,
                plugin_id: None,
                status: McpToolCallStatus::Completed,
                result: Some(CallToolResult {
                    content: Vec::new(),
                    structured_content: None,
                    is_error: Some(false),
                    meta: None,
                }),
                error: None,
                duration: None,
            }),
        )
        .await;
}

async fn emit_simple_tool_failed<T: serde::Serialize>(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    call_id: &str,
    tool_name: &str,
    args: &T,
    message: &str,
) {
    session
        .emit_turn_item_completed(
            turn,
            TurnItem::McpToolCall(McpToolCallItem {
                id: call_id.to_string(),
                server: "codex++".to_string(),
                tool: tool_name.to_string(),
                arguments: serde_json::to_value(args).unwrap_or(serde_json::Value::Null),
                mcp_app_resource_uri: None,
                plugin_id: None,
                status: McpToolCallStatus::Failed,
                result: None,
                error: Some(McpToolCallError {
                    message: message.to_string(),
                }),
                duration: None,
            }),
        )
        .await;
}

fn deserialize_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(value) = Option::<serde_json::Value>::deserialize(deserializer)? else {
        return Ok(false);
    };
    match value {
        serde_json::Value::Bool(value) => Ok(value),
        serde_json::Value::Number(value) if value.as_u64() == Some(0) => Ok(false),
        serde_json::Value::Number(value) if value.as_u64() == Some(1) => Ok(true),
        serde_json::Value::String(value) => parse_bool_string(&value).ok_or_else(|| {
            de::Error::custom(format!("expected boolean-compatible string, got `{value}`"))
        }),
        _ => Err(de::Error::custom("expected boolean")),
    }
}

fn deserialize_optional_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_optional_integer(deserializer)
}

fn deserialize_optional_usize<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_optional_integer(deserializer)
}

fn deserialize_optional_integer<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: TryFrom<u64>,
{
    let Some(value) = Option::<serde_json::Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    let parsed = match value {
        serde_json::Value::Null => return Ok(None),
        serde_json::Value::Number(value) => value
            .as_u64()
            .ok_or_else(|| de::Error::custom("expected non-negative integer"))?,
        serde_json::Value::String(value) => value
            .parse::<u64>()
            .map_err(|_| de::Error::custom(format!("expected integer string, got `{value}`")))?,
        _ => return Err(de::Error::custom("expected integer")),
    };
    T::try_from(parsed)
        .map(Some)
        .map_err(|_| de::Error::custom("integer is out of range"))
}

fn parse_bool_string(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "y" | "on" => Some(true),
        "false" | "0" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
#[path = "simple_file_tools_tests.rs"]
mod tests;
