//! Captures synchronous evidence once for the common preparation pass.
//! Reviewer-history token measurement is needed only for captured images.

use codex_guardian_context::TranscriptSelection;
use codex_guardian_reviewer::GuardianPromptItems;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ImageReference;
use codex_protocol::openai_models::InputModality;
use codex_protocol::openai_models::ModelInfo;
use codex_tools::normalize_output_image_detail;
use std::borrow::Cow;
use tracing::warn;

use super::super::GuardianApprovalRequest;
use super::super::prompt::GuardianPromptMode;
use super::super::prompt::GuardianTranscriptHistory;
use super::super::prompt::collect_guardian_prompt_items_with_parent_turn;
use super::GuardianReviewSession;
use super::GuardianReviewSessionParams;
use crate::context::ContextualUserFragment;
use crate::context::GuardianExtensionApproval;
use crate::context_manager::ContextManager;
use crate::image_preparation::ImagePreparationMode;
use crate::image_preparation::resize_image;
use crate::image_preparation::unified_image_budget_enabled;

const GUARDIAN_MAX_APPROVAL_REASON_TOKENS: usize = 512;

pub(super) async fn build_guardian_prompt_items(
    params: &GuardianReviewSessionParams,
    reviewer: &GuardianReviewSession,
    model_info: &ModelInfo,
    reviewer_history: &ContextManager,
    history: GuardianTranscriptHistory<'_>,
    mode: GuardianPromptMode,
    reviewed_node_repl_evidence_sequence: u64,
) -> anyhow::Result<GuardianPromptItems> {
    let collected = collect_guardian_prompt_items_with_parent_turn(
        params.parent_session.as_ref(),
        history,
        Some(&params.parent_context),
        params.reasons.clone(),
        params.request.clone(),
        mode,
        reviewed_node_repl_evidence_sequence,
    )
    .await?;
    let context_window = model_info.resolved_context_window().map(|supported| {
        params
            .spawn_config
            .model_context_window
            .unwrap_or(supported)
            .min(supported)
            .saturating_mul(model_info.effective_context_window_percent.clamp(0, 100))
            / 100
    });
    let context_window =
        context_window.filter(|limit| *limit > 0 && !model_info.used_fallback_model_metadata);
    let image_mode = if unified_image_budget_enabled(&params.spawn_config.features, model_info) {
        ImagePreparationMode::UnifiedBudget
    } else {
        ImagePreparationMode::DetailBased
    };
    let history_tokens = if context_window.is_some() && collected.sections.has_images() {
        let base_instructions = reviewer.session.get_base_instructions().await;
        reviewer_history
            .estimate_token_count_with_base_instructions(&base_instructions)
            .unwrap_or(i64::MAX)
            .max(reviewer.session.get_total_token_usage().await)
    } else {
        i64::MAX
    };
    let image_preparer = ReviewImagePreparer {
        model_info,
        mode: image_mode,
    };
    let extension_action_intro = match &params.request {
        GuardianApprovalRequest::ExtensionTool { .. } => {
            let (selection, _) = mode.select(
                collected.sections.transcript_entries(),
                collected.history_version,
            );
            Some(match selection {
                TranscriptSelection::Full(_) => {
                    "The Codex agent has requested the following action:\n"
                }
                TranscriptSelection::Delta { .. } => {
                    "The Codex agent has requested the following next action:\n"
                }
            })
        }
        _ => None,
    };
    let mut prompt_items = codex_guardian_reviewer::compose_guardian_prompt_with_preparation(
        collected.sections,
        &collected.profile,
        collected.mode,
        collected.history_version,
        &collected.session_id,
        collected.node_repl_evidence_sequence,
        codex_guardian_context::SyncPreparation {
            reviewer_history: reviewer_history.annotated_items(),
            context_window,
            history_tokens,
            supports_images: model_info.input_modalities.contains(&InputModality::Image),
            image_preparer: &image_preparer,
            estimate_tokens: crate::context_manager::estimate_item_token_count,
        },
    )
    .await?;
    if let Some(extension_action_intro) = extension_action_intro
        && let GuardianApprovalRequest::ExtensionTool {
            tool_name,
            artifact,
            ..
        } = &params.request
    {
        prompt_items
            .context
            .push_user_text(extension_action_intro.to_string());
        prompt_items
            .context
            .push_user_text(">>> APPROVAL REQUEST START\n".to_string());
        if let Some(reason) = params
            .reasons
            .retry
            .as_ref()
            .or(params.reasons.approval.as_ref())
        {
            let reason =
                codex_guardian_context::truncate_text(reason, GUARDIAN_MAX_APPROVAL_REASON_TOKENS);
            prompt_items
                .context
                .push_user_text("Retry reason:\n".to_string());
            prompt_items.context.push_user_text(format!("{reason}\n\n"));
        }
        prompt_items.context.push_user_text(
            "Review the complete content-addressed extension action with `read_guardian_approval_artifact`. Continue reading from each returned offset until the artifact is complete, then assess that exact action.\n"
                .to_string(),
        );
        prompt_items.context.push_user_text(
            GuardianExtensionApproval::new(tool_name, artifact.sha256(), artifact.byte_length())
                .render()
                .into_text()
                .unwrap_or_default(),
        );
        prompt_items
            .context
            .push_user_text(">>> APPROVAL REQUEST END\n".to_string());
    }
    Ok(prompt_items)
}

struct ReviewImagePreparer<'a> {
    model_info: &'a ModelInfo,
    mode: ImagePreparationMode,
}

impl codex_guardian_context::SyncImagePreparer for ReviewImagePreparer<'_> {
    fn prepare<'image>(
        &self,
        image: &'image ImageReference,
        detail: &mut Option<ImageDetail>,
    ) -> Option<Cow<'image, ImageReference>> {
        *detail = match normalize_output_image_detail(self.model_info, *detail) {
            _ if self.mode == ImagePreparationMode::UnifiedBudget => Some(ImageDetail::Original),
            Some(ImageDetail::Low) => Some(ImageDetail::High),
            detail => detail,
        };
        match image {
            ImageReference::Inline { image_url } => {
                match resize_image(image_url, detail, self.mode) {
                    Ok(Some(prepared)) => Some(Cow::Owned(ImageReference::Inline {
                        image_url: prepared.into_data_url(),
                    })),
                    Ok(None) => Some(Cow::Borrowed(image)),
                    Err(error) => {
                        warn!(%error, "failed to prepare guardian review image");
                        None
                    }
                }
            }
            ImageReference::File { .. } => Some(Cow::Borrowed(image)),
        }
    }
}
