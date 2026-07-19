//! File Edit runtime: executes verified exact-byte writes under the orchestrator.
//!
//! Approval and sandbox escalation are shared with apply_patch, while the write
//! itself keeps Edit's UTF-16, BOM, CRLF, and missing-final-newline semantics by
//! going through `write_file_verified` instead of patch text application.
use crate::exec::is_likely_sandbox_denied;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::handlers::file_edit_spec::FILE_EDIT_TOOL_NAME;
use crate::tools::handlers::verified_file_write::ExpectedFileContents;
use crate::tools::handlers::verified_file_write::FileWriteCommitState;
use crate::tools::handlers::verified_file_write::VerifiedFileWrite;
use crate::tools::handlers::verified_file_write::VerifiedFileWriteError;
use crate::tools::handlers::verified_file_write::verified_write_error_message;
use crate::tools::handlers::verified_file_write::write_file_verified;
use crate::tools::sandboxing::Approvable;
use crate::tools::sandboxing::ApprovalAction;
use crate::tools::sandboxing::ExecApprovalRequirement;
use crate::tools::sandboxing::SandboxAttempt;
use crate::tools::sandboxing::Sandboxable;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;
use crate::tools::sandboxing::ToolRuntime;
use crate::tools::sandboxing::executor_windows_sandbox_selection;
use codex_exec_server::FileSystemSandboxContext;
use codex_exec_server::GetMetadataOptions;
use codex_protocol::error::CodexErr;
use codex_protocol::error::SandboxErr;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::FileChange;
use codex_sandboxing::SandboxType;
use codex_sandboxing::SandboxablePreference;
use codex_sandboxing::is_likely_executor_managed_sandbox_denied;
use codex_sandboxing::policy_transforms::effective_permission_profile;
use codex_utils_path_uri::PathUri;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub(crate) struct FileEditRequest {
    pub turn_environment: TurnEnvironment,
    pub path: PathUri,
    pub path_summary: String,
    pub cwd: PathUri,
    /// Exact bytes the file must contain before the write; `None` requires the
    /// file to be missing.
    pub expected: Option<Vec<u8>>,
    pub updated: Vec<u8>,
    pub changes: Arc<HashMap<PathBuf, FileChange>>,
    /// Patch-shaped rendering of the edit used only for approval review surfaces.
    pub approval_patch: String,
    pub exec_approval_requirement: ExecApprovalRequirement,
    pub additional_permissions: Option<AdditionalPermissionProfile>,
    pub permissions_preapproved: bool,
}

impl FileEditRequest {
    fn approval_action(&self, call_id: &str) -> ApprovalAction {
        ApprovalAction::ApplyPatch {
            id: call_id.to_string(),
            environment_id: self.turn_environment.selection.environment_id.clone(),
            cwd: self.cwd.clone(),
            files: vec![self.path.clone()],
            patch: self.approval_patch.clone(),
            changes: Arc::clone(&self.changes),
            permissions_preapproved: self.permissions_preapproved,
        }
    }
}

pub(crate) struct FileEditRuntimeOutput {
    pub modified_at_ms: i64,
}

pub(crate) struct FileEditWriteFailure {
    pub message: String,
    pub commit_state: FileWriteCommitState,
}

#[derive(Default)]
pub(crate) struct FileEditRuntime {
    failure: Option<FileEditWriteFailure>,
}

impl FileEditRuntime {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn take_failure(&mut self) -> Option<FileEditWriteFailure> {
        self.failure.take()
    }

    fn record_failure(&mut self, message: String, commit_state: FileWriteCommitState) -> ToolError {
        self.failure = Some(FileEditWriteFailure {
            message: message.clone(),
            commit_state,
        });
        ToolError::Rejected(message)
    }

    fn file_system_sandbox_context_for_attempt(
        req: &FileEditRequest,
        attempt: &SandboxAttempt<'_>,
    ) -> Option<FileSystemSandboxContext> {
        if !attempt.sandbox_requested {
            return None;
        }

        let permissions = effective_permission_profile(
            attempt.exec_server_permissions,
            req.additional_permissions.as_ref(),
        );
        Some(FileSystemSandboxContext {
            sandbox_override: attempt.sandbox_override,
            permissions,
            cwd: attempt.sandbox_cwd.clone(),
            workspace_roots: attempt.workspace_roots.to_vec(),
            user_home_dir: req.turn_environment.user_home_dir.clone(),
            temporary_directories: None,
            windows_sandbox_selection: executor_windows_sandbox_selection(
                attempt.windows_sandbox_type,
                attempt.windows_sandbox_level,
                attempt.sandbox_cwd,
            ),
            windows_sandbox_proxy_settings_mode: None,
            use_legacy_landlock: attempt.use_legacy_landlock,
        })
    }
}

impl Sandboxable for FileEditRuntime {
    fn sandbox_preference(&self) -> SandboxablePreference {
        SandboxablePreference::Auto
    }
    fn escalate_on_failure(&self) -> bool {
        true
    }
}

impl Approvable<FileEditRequest> for FileEditRuntime {
    fn approval_action(
        &self,
        req: &FileEditRequest,
        call_id: &str,
    ) -> std::io::Result<ApprovalAction> {
        Ok(req.approval_action(call_id))
    }

    fn wants_no_sandbox_approval(&self, policy: AskForApproval) -> bool {
        match policy {
            AskForApproval::Never => false,
            AskForApproval::Granular(granular_config) => granular_config.allows_sandbox_approval(),
            AskForApproval::OnRequest => true,
            AskForApproval::UnlessTrusted => true,
        }
    }

    // Edit approvals are decided upstream by assess_file_write_safety, mirroring
    // how apply_patch delegates its policy decision to the orchestrator.
    fn exec_approval_requirement(&self, req: &FileEditRequest) -> Option<ExecApprovalRequirement> {
        Some(req.exec_approval_requirement.clone())
    }
}

impl ToolRuntime<FileEditRequest, FileEditRuntimeOutput> for FileEditRuntime {
    fn turn_environment<'a>(&self, req: &'a FileEditRequest) -> &'a TurnEnvironment {
        &req.turn_environment
    }

    fn uses_executor_managed_process_sandbox(&self, req: &FileEditRequest) -> bool {
        req.turn_environment.environment().is_remote()
    }

    fn sandbox_cwd<'a>(&self, req: &'a FileEditRequest) -> Option<&'a PathUri> {
        Some(&req.cwd)
    }

    async fn run(
        &mut self,
        req: &FileEditRequest,
        attempt: &SandboxAttempt<'_>,
        _ctx: &ToolCtx,
    ) -> Result<FileEditRuntimeOutput, ToolError> {
        let fs = req.turn_environment.environment().get_filesystem();
        let sandbox = Self::file_system_sandbox_context_for_attempt(req, attempt);
        let expected = req
            .expected
            .as_deref()
            .map_or(ExpectedFileContents::Missing, ExpectedFileContents::Present);
        if let Err(error) = write_file_verified(VerifiedFileWrite {
            fs: fs.as_ref(),
            path: &req.path,
            sandbox: sandbox.as_ref(),
            expected,
            updated: &req.updated,
        })
        .await
        {
            let message =
                verified_write_error_message(&error, FILE_EDIT_TOOL_NAME, &req.path_summary);
            let commit_state = error.commit_state();
            let remote = self.uses_executor_managed_process_sandbox(req);
            let local_backend = if attempt.sandbox_requested && !remote {
                attempt.manager.select_initial(
                    attempt.permissions,
                    self.sandbox_preference(),
                    attempt.windows_sandbox_type,
                    attempt.enforce_managed_network,
                )
            } else {
                SandboxType::None
            };
            if let Some(output) = sandbox_denied_output(attempt, local_backend, &error, &message) {
                self.failure = Some(FileEditWriteFailure {
                    message,
                    commit_state,
                });
                return Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied {
                    output: Box::new(output),
                    network_policy_decision: None,
                })));
            }
            return Err(self.record_failure(message, commit_state));
        }
        match fs
            .get_metadata(&req.path, GetMetadataOptions::default(), sandbox.as_ref())
            .await
        {
            Ok(metadata) => Ok(FileEditRuntimeOutput {
                modified_at_ms: metadata.modified_at_ms,
            }),
            Err(error) => {
                let message = format!(
                    "{FILE_EDIT_TOOL_NAME} could not inspect the written file `{}`: {error}",
                    req.path_summary
                );
                Err(self.record_failure(message, FileWriteCommitState::Committed))
            }
        }
    }
}

fn sandbox_denied_output(
    attempt: &SandboxAttempt<'_>,
    local_backend: SandboxType,
    error: &VerifiedFileWriteError,
    message: &str,
) -> Option<ExecToolCallOutput> {
    let io_error = match error {
        VerifiedFileWriteError::ReadBeforeWrite(error)
        | VerifiedFileWriteError::CreateParent(error)
        | VerifiedFileWriteError::Write(error) => error,
        VerifiedFileWriteError::UnexpectedContents
        | VerifiedFileWriteError::ReadAfterWrite(_)
        | VerifiedFileWriteError::UnexpectedWrittenContents => return None,
    };
    let output = ExecToolCallOutput {
        exit_code: 1,
        stdout: StreamOutput::new(String::new()),
        stderr: StreamOutput::new(io_error.to_string()),
        aggregated_output: StreamOutput::new(message.to_string()),
        duration: Duration::ZERO,
        timed_out: false,
    };
    let denied = if local_backend == SandboxType::None {
        attempt.sandbox_requested && is_likely_executor_managed_sandbox_denied(&output)
    } else {
        is_likely_sandbox_denied(local_backend, &output)
    };
    denied.then_some(output)
}
