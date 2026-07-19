use crate::function_tool::FunctionCallError;
use crate::safety::PatchSandboxRoute;
use crate::safety::SafetyCheck;
use crate::safety::assess_file_write_safety;
use crate::sandboxing::SandboxPermissions;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::handlers::apply_granted_turn_permissions;
use crate::tools::handlers::apply_patch::write_permissions_for_paths;
use crate::tools::handlers::file_change_event::FileChangeEvent;
use crate::tools::handlers::file_change_event::file_change_event;
use crate::tools::handlers::file_edit_content::EditContentRequest;
use crate::tools::handlers::file_edit_content::MAX_EDIT_FILE_SIZE;
use crate::tools::handlers::file_edit_content::prepare_edit;
use crate::tools::handlers::file_edit_history::RestoredReadRequest;
use crate::tools::handlers::file_edit_history::history_contains_current_read;
use crate::tools::handlers::file_edit_spec::FILE_EDIT_TOOL_NAME;
use crate::tools::handlers::file_edit_spec::FileEditToolOptions;
use crate::tools::handlers::file_edit_spec::create_file_edit_tool;
use crate::tools::handlers::file_encoding::DecodedFile;
use crate::tools::handlers::file_encoding::FileEncoding;
use crate::tools::handlers::file_encoding::decode_text_file;
use crate::tools::handlers::file_encoding::encode_file;
use crate::tools::handlers::file_path::bound_function_call_error;
use crate::tools::handlers::file_path::resolve_tool_path;
use crate::tools::handlers::file_path::summarize_tool_argument;
use crate::tools::handlers::file_state::EditStateError;
use crate::tools::handlers::file_state::EditedFileState;
use crate::tools::handlers::file_state::ReadFileState;
use crate::tools::handlers::file_state::normalize_file_content;
use crate::tools::handlers::file_state::session_file_state_cache;
use crate::tools::handlers::file_system_sandbox_policy_context_for_cwd;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::handlers::verified_file_write::FileWriteCommitState;
use crate::tools::hook_names::HookToolName;
use crate::tools::orchestrator::ToolOrchestrator;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::runtimes::file_edit::FileEditRequest;
use crate::tools::runtimes::file_edit::FileEditRuntime;
use crate::tools::sandboxing::ExecApprovalRequirement;
use crate::tools::sandboxing::ToolCtx;
use crate::windows_sandbox::windows_sandbox_level_for_legacy_checks;
use codex_apply_patch::AppliedPatchDelta;
use codex_exec_server::GetMetadataOptions;
use codex_exec_server::ReadFileOptions;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::protocol::FileChange;
use codex_sandboxing::policy_transforms::effective_file_system_sandbox_policy;
use codex_tools::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub(crate) struct FileEditHandler {
    options: FileEditToolOptions,
}

impl FileEditHandler {
    pub(crate) fn new(options: FileEditToolOptions) -> Self {
        Self { options }
    }
}

impl Default for FileEditHandler {
    fn default() -> Self {
        Self::new(FileEditToolOptions::default())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileEditArgs {
    file_path: String,
    #[serde(default)]
    environment_id: Option<String>,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

impl ToolExecutor<ToolInvocation> for FileEditHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(FILE_EDIT_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_file_edit_tool(self.options)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        false
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            handle_file_edit(invocation)
                .await
                .map_err(bound_function_call_error)
        })
    }
}

impl CoreToolRuntime for FileEditHandler {
    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };
        Some(PreToolUsePayload {
            tool_name: HookToolName::new(FILE_EDIT_TOOL_NAME),
            tool_input: serde_json::from_str(arguments).ok()?,
        })
    }
}

async fn handle_file_edit(
    invocation: ToolInvocation,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        step_context,
        tracker,
        call_id,
        tool_name,
        cancellation_token,
        payload,
        ..
    } = invocation;
    let ToolPayload::Function { arguments } = payload else {
        return Err(FunctionCallError::RespondToModel(
            "Edit handler received unsupported payload".to_string(),
        ));
    };
    let args: FileEditArgs = parse_arguments(&arguments)?;
    if args.old_string == args.new_string {
        return Err(FunctionCallError::RespondToModel(
            "No changes to make: old_string and new_string are exactly the same.".to_string(),
        ));
    }

    let turn_environment =
        resolve_tool_environment(&step_context.environments, args.environment_id.as_deref())?;
    let path = resolve_tool_path(
        FILE_EDIT_TOOL_NAME,
        "file_path",
        turn_environment.cwd(),
        &args.file_path,
    )?;
    let path_summary = summarize_tool_argument(&path.inferred_native_path_string());
    let native_cwd = turn_environment.cwd().to_abs_path().map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "Edit could not resolve the environment working directory: {error}"
        ))
    })?;
    let effective_permissions = apply_granted_turn_permissions(
        session.as_ref(),
        &turn_environment.selection.environment_id,
        native_cwd.as_path(),
        SandboxPermissions::UseDefault,
        /*additional_permissions*/ None,
    )
    .await;
    let sandbox = turn_environment.sandbox_context(effective_permissions.additional_permissions);
    let fs = turn_environment.environment.get_filesystem();

    let original = match fs
        .get_metadata(&path, GetMetadataOptions::default(), Some(&sandbox))
        .await
    {
        Ok(metadata) => {
            if metadata.size > MAX_EDIT_FILE_SIZE {
                return Err(FunctionCallError::RespondToModel(format!(
                    "File is too large to edit ({} bytes). Maximum editable file size is {MAX_EDIT_FILE_SIZE} bytes.",
                    metadata.size
                )));
            }
            let bytes = fs
                .read_file(&path, ReadFileOptions::default(), Some(&sandbox))
                .await
                .map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "Failed to read file `{path_summary}`: {error}"
                    ))
                })?;
            if bytes.len() > MAX_EDIT_FILE_SIZE as usize {
                return Err(FunctionCallError::RespondToModel(format!(
                    "File is too large to edit ({} bytes). Maximum editable file size is {MAX_EDIT_FILE_SIZE} bytes.",
                    bytes.len()
                )));
            }
            let decoded = decode_text_file(bytes, &path_summary, FILE_EDIT_TOOL_NAME)?;
            Some(ExistingFile {
                expected_bytes: encode_file(&decoded.content, decoded.encoding),
                decoded,
                modified_at_ms: metadata.modified_at_ms,
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(FunctionCallError::RespondToModel(format!(
                "Failed to inspect file `{path_summary}`: {error}"
            )));
        }
    };

    let file_state_cache = session_file_state_cache(
        &session.services.session_extension_data,
        session.history_version().await,
    );
    if original.is_none() && !args.old_string.is_empty() {
        return Err(FunctionCallError::RespondToModel(format!(
            "File does not exist. Current working directory: {}.",
            turn_environment.cwd().inferred_native_path_string()
        )));
    }
    if original.is_some()
        && !args.old_string.is_empty()
        && args
            .file_path
            .trim()
            .to_ascii_lowercase()
            .ends_with(".ipynb")
    {
        return Err(FunctionCallError::RespondToModel(
            "Edit does not support Jupyter notebooks. Modify the notebook with a shell command or a JSON processor such as jq instead."
                .to_string(),
        ));
    }
    if !args.old_string.is_empty()
        && let Some(original) = original.as_ref()
    {
        let current_content = normalize_file_content(&original.decoded.content);
        match file_state_cache.validate_edit(
            &turn_environment.selection.environment_id,
            &path,
            original.modified_at_ms,
            &current_content,
        ) {
            Ok(()) => {}
            Err(EditStateError::NotRead) => {
                if history_contains_current_read(RestoredReadRequest {
                    session: session.as_ref(),
                    step_context: step_context.as_ref(),
                    environment_id: &turn_environment.selection.environment_id,
                    path: &path,
                    current_content: &current_content,
                })
                .await
                {
                    file_state_cache.record_read(
                        &turn_environment.selection.environment_id,
                        &path,
                        ReadFileState {
                            content: current_content,
                            modified_at_ms: original.modified_at_ms,
                            offset: 1,
                            limit: None,
                        },
                    );
                } else {
                    return Err(FunctionCallError::RespondToModel(
                        "File has not been read yet. Read it first before writing to it."
                            .to_string(),
                    ));
                }
            }
            Err(EditStateError::Modified) => {
                return Err(FunctionCallError::RespondToModel(
                    "File has been modified since read, either by the user or by a linter. Read it again before attempting to write it."
                        .to_string(),
                ));
            }
        }
    }

    let original_content = original
        .as_ref()
        .map(|original| original.decoded.content.as_str());
    let edit = prepare_edit(
        EditContentRequest {
            old_string: &args.old_string,
            new_string: &args.new_string,
            replace_all: args.replace_all,
        },
        original_content,
    )?;
    let encoding = original
        .as_ref()
        .map_or(FileEncoding::Utf8, |original| original.decoded.encoding);
    let updated_bytes = encode_file(&edit.updated_content, encoding);
    if updated_bytes.len() > MAX_EDIT_FILE_SIZE as usize {
        return Err(FunctionCallError::RespondToModel(format!(
            "The edited file would exceed the maximum editable file size of {MAX_EDIT_FILE_SIZE} bytes. Use smaller replacements or another editing strategy."
        )));
    }
    let FileChangeEvent { changes, delta } = file_change_event(
        &path,
        original_content,
        &edit.updated_content,
        /*context_radius*/ 3,
    );

    let cwd = turn_environment.cwd().clone();
    let approval_sandbox = turn_environment.sandbox_context(/*additional_permissions*/ None);
    let policy_context = file_system_sandbox_policy_context_for_cwd(&approval_sandbox, &cwd);
    let sandbox_route = if turn_environment.environment().is_remote() {
        PatchSandboxRoute::ExecutorManaged
    } else {
        PatchSandboxRoute::Platform(windows_sandbox_level_for_legacy_checks(
            turn_environment.config().windows_sandbox_type,
            turn_environment.config().windows_sandbox_level,
        ))
    };
    let environment_id = turn_environment.selection.environment_id.clone();
    let granted_permissions = step_context.turn.granted_permissions(&environment_id);
    let base_file_system_sandbox_policy = turn_environment
        .permission_profile()
        .file_system_sandbox_policy();
    let file_system_sandbox_policy = effective_file_system_sandbox_policy(
        &base_file_system_sandbox_policy,
        granted_permissions.as_ref(),
    );
    let matching = sandbox_route
        .prepare_matching(&file_system_sandbox_policy, &policy_context)
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "failed to prepare edit permissions: {error}"
            ))
        })?;
    let safety = assess_file_write_safety(
        &path,
        step_context.settings.approval_policy(),
        turn_environment.permission_profile(),
        &matching,
    )
    .map_err(|error| {
        FunctionCallError::RespondToModel(format!("failed to check edit permissions: {error}"))
    })?;
    let exec_approval_requirement = edit_approval_requirement(safety)?;
    let additional_permissions =
        write_permissions_for_paths(std::slice::from_ref(&path), &matching).map_err(|error| {
            FunctionCallError::RespondToModel(format!("failed to check edit permissions: {error}"))
        })?;
    let effective_permissions = apply_granted_turn_permissions(
        step_context.as_ref(),
        turn_environment,
        &cwd,
        SandboxPermissions::UseDefault,
        additional_permissions,
    );
    let changes = Arc::new(changes);
    let approval_patch = edit_approval_patch_text(&path, &changes);
    let emitter = ToolEmitter::file_change_for_environment(
        changes.as_ref().clone(),
        matches!(
            exec_approval_requirement,
            ExecApprovalRequirement::Skip { .. }
        ),
        environment_id.clone(),
    );
    let event_ctx = ToolEventCtx::new(
        session.as_ref(),
        turn.as_ref(),
        &step_context.settings.model_info,
        &call_id,
        Some(&tracker),
    );
    emitter.begin(event_ctx).await;

    let request = FileEditRequest {
        turn_environment: turn_environment.clone(),
        path,
        path_summary,
        cwd,
        expected: original
            .as_ref()
            .map(|original| original.expected_bytes.clone()),
        updated: updated_bytes,
        changes,
        approval_patch,
        exec_approval_requirement,
        additional_permissions: effective_permissions.additional_permissions,
        permissions_preapproved: effective_permissions.permissions_preapproved,
    };
    let tool_ctx = ToolCtx {
        session: session.clone(),
        step_context: step_context.clone(),
        cancellation_token,
        call_id: call_id.to_string(),
        tool_name,
    };
    let mut orchestrator = ToolOrchestrator::new();
    let mut runtime = FileEditRuntime::new();
    let result = orchestrator.run(&mut runtime, &request, &tool_ctx).await;
    let event_ctx = ToolEventCtx::new(
        session.as_ref(),
        turn.as_ref(),
        &step_context.settings.model_info,
        &call_id,
        Some(&tracker),
    );
    match result {
        Ok(result) => {
            file_state_cache.record_edit(
                &environment_id,
                &request.path,
                EditedFileState {
                    content: normalize_file_content(&edit.updated_content),
                    modified_at_ms: result.output.modified_at_ms,
                },
            );
            emitter
                .finish(
                    event_ctx,
                    Ok(edit_event_output(
                        0,
                        "Edit applied successfully".to_string(),
                    )),
                    delta.as_ref(),
                )
                .await?;
        }
        Err(error) => {
            if let Some(failure) = runtime.take_failure() {
                file_state_cache.remove(&environment_id, &request.path);
                let empty_delta = AppliedPatchDelta::default();
                let applied_delta = match failure.commit_state {
                    FileWriteCommitState::NotCommitted => Some(&empty_delta),
                    FileWriteCommitState::Committed => delta.as_ref(),
                    FileWriteCommitState::Unknown => None,
                };
                emit_edit_failure(
                    &emitter,
                    session.as_ref(),
                    turn.as_ref(),
                    &step_context.settings.model_info,
                    &tracker,
                    &call_id,
                    &failure.message,
                    applied_delta,
                )
                .await;
                return Err(FunctionCallError::RespondToModel(failure.message));
            }
            return match emitter.finish(event_ctx, Err(error), None).await {
                Err(model_error) => Err(model_error),
                Ok(_) => Err(FunctionCallError::RespondToModel(
                    "Edit approval did not produce a result".to_string(),
                )),
            };
        }
    }

    let file_path = summarize_tool_argument(&args.file_path);
    let message = if args.replace_all {
        format!(
            "The file {file_path} has been updated. All occurrences were successfully replaced."
        )
    } else {
        format!("The file {file_path} has been updated successfully.")
    };
    Ok(boxed_tool_output(FunctionToolOutput::from_text(
        message,
        Some(true),
    )))
}

fn edit_approval_requirement(
    safety: SafetyCheck,
) -> Result<ExecApprovalRequirement, FunctionCallError> {
    match safety {
        SafetyCheck::AutoApprove => Ok(ExecApprovalRequirement::Skip {
            bypass_sandbox: false,
            proposed_execpolicy_amendment: None,
        }),
        SafetyCheck::AskUser => Ok(ExecApprovalRequirement::NeedsApproval {
            reason: None,
            proposed_execpolicy_amendment: None,
        }),
        SafetyCheck::Reject { reason } => Err(FunctionCallError::RespondToModel(format!(
            "{FILE_EDIT_TOOL_NAME} rejected: {reason}"
        ))),
    }
}

/// Renders the edit as patch-shaped text for approval review surfaces only;
/// the actual write keeps using the exact encoded bytes.
fn edit_approval_patch_text(path: &PathUri, changes: &HashMap<PathBuf, FileChange>) -> String {
    let path_text = path.inferred_native_path_string();
    let (header, body) = match changes.values().next() {
        Some(FileChange::Add { content }) => {
            let mut body = String::new();
            for line in content.lines() {
                body.push('+');
                body.push_str(line);
                body.push('\n');
            }
            (format!("*** Add File: {path_text}\n"), body)
        }
        Some(FileChange::Update { unified_diff, .. }) => (
            format!("*** Update File: {path_text}\n"),
            unified_diff.clone(),
        ),
        _ => (format!("*** Update File: {path_text}\n"), String::new()),
    };
    format!("*** Begin Patch\n{header}{body}*** End Patch\n")
}

struct ExistingFile {
    decoded: DecodedFile,
    expected_bytes: Vec<u8>,
    modified_at_ms: i64,
}

fn edit_event_output(exit_code: i32, message: String) -> ExecToolCallOutput {
    let (stdout, stderr) = if exit_code == 0 {
        (message, String::new())
    } else {
        (String::new(), message)
    };
    ExecToolCallOutput {
        exit_code,
        stdout: StreamOutput::new(stdout.clone()),
        stderr: StreamOutput::new(stderr.clone()),
        aggregated_output: StreamOutput::new(if exit_code == 0 { stdout } else { stderr }),
        duration: Duration::ZERO,
        timed_out: false,
    }
}

async fn emit_edit_failure(
    emitter: &ToolEmitter,
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    model_info: &codex_protocol::openai_models::ModelInfo,
    tracker: &crate::tools::context::SharedTurnDiffTracker,
    call_id: &str,
    message: &str,
    applied_patch_delta: Option<&AppliedPatchDelta>,
) {
    let event_ctx = ToolEventCtx::new(session, turn, model_info, call_id, Some(tracker));
    let _ = emitter
        .finish(
            event_ctx,
            Ok(edit_event_output(1, message.to_string())),
            applied_patch_delta,
        )
        .await;
}

#[cfg(test)]
#[path = "file_edit_tests.rs"]
mod tests;
