//! Returns literal model instructions from the captured model metadata.
//! Missing templates produce a warning; supplied empty templates remain valid input.

use crate::ResolvedModelMessages;
use codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT;
use codex_protocol::openai_models::ApplyPatchToolType;
use codex_protocol::openai_models::ModelInfo;

const GENERIC_FILE_EDIT_GUIDANCE: &str = "- Use the dedicated file-editing tool(s) provided in this session when available instead of ad-hoc shell redirections.";
const APPLY_PATCH_FILE_EDIT_GUIDANCE: &str = r#"- Use the `apply_patch` tool to edit files (NEVER try `applypatch` or `apply-patch`, only `apply_patch`): {"command":["apply_patch","*** Begin Patch\n*** Update File: path/to/file.py\n@@ def example():\n- pass\n+ return 123\n*** End Patch"]}"#;
const HASHLINE_FILE_EDIT_GUIDANCE: &str = "- Use `fuzz_view_edit` for text file edits. Read the target lines first, then edit, insert, or delete using the returned line-hash anchors so stale or ambiguous edits fail safely.";
const CLAUDE_CODE_FILE_EDIT_GUIDANCE: &str = "- Use `Read` and `Edit` for file inspection and edits. Read an existing file with `Read` before modifying it, then apply exact `old_string` -> `new_string` replacements with `Edit`.";

/// Renders base instructions from the caller's captured model, retaining missing-template warnings.
///
/// When the model inherits the generic Codex CLI instructions (`BASE_INSTRUCTIONS_DEFAULT`),
/// the file-editing bullet is aligned with `model_info.apply_patch_tool_type` so the prompt
/// matches the exact file-editing toolset registered for the turn.
pub fn render_model_instructions(model_info: &ModelInfo) -> String {
    let template = ResolvedModelMessages::from_model(model_info).instructions_template();
    if template.is_none() {
        tracing::warn!(
            model = %model_info.slug,
            "Model has no instruction template; returning empty instructions."
        );
    }
    let Some(template) = template else {
        return String::new();
    };
    if template != BASE_INSTRUCTIONS_DEFAULT {
        return template.to_owned();
    }
    let Some(guidance) = file_edit_guidance_for_tool_type(model_info.apply_patch_tool_type) else {
        return template.to_owned();
    };
    template.replacen(GENERIC_FILE_EDIT_GUIDANCE, guidance, 1)
}

fn file_edit_guidance_for_tool_type(tool_type: Option<ApplyPatchToolType>) -> Option<&'static str> {
    match tool_type? {
        ApplyPatchToolType::Freeform => Some(APPLY_PATCH_FILE_EDIT_GUIDANCE),
        ApplyPatchToolType::Hashline => Some(HASHLINE_FILE_EDIT_GUIDANCE),
        ApplyPatchToolType::ClaudeCode => Some(CLAUDE_CODE_FILE_EDIT_GUIDANCE),
    }
}

#[cfg(test)]
#[path = "model_instructions_tests.rs"]
mod tests;
