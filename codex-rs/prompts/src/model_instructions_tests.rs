//! Covers literal instruction templates and missing-versus-empty inputs.

use super::*;
use codex_protocol::openai_models::ConfigShellToolType;
use codex_protocol::openai_models::ModelMessages;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::TruncationPolicyConfig;
use pretty_assertions::assert_eq;

fn test_model(model_messages: Option<ModelMessages>) -> ModelInfo {
    ModelInfo {
        slug: "test-model".to_string(),
        display_name: "Test Model".to_string(),
        description: None,
        default_reasoning_level: None,
        supported_reasoning_levels: vec![],
        shell_type: ConfigShellToolType::UnifiedExec,
        visibility: ModelVisibility::List,
        supported_in_api: true,
        priority: 1,
        additional_speed_tiers: vec![],
        service_tiers: vec![],
        default_service_tier: None,
        available_access_programs: None,
        availability_nux: None,
        upgrade: None,
        model_messages,
        include_skills_usage_instructions: false,
        include_plugin_usage_instructions: false,
        include_apps_usage_instructions: false,
        supports_reasoning_summary_parameter: true,
        default_reasoning_summary: Default::default(),
        support_verbosity: false,
        default_verbosity: None,
        apply_patch_tool_type: None,
        web_search_tool_type: Default::default(),
        truncation_policy: TruncationPolicyConfig::bytes(/*limit*/ 10_000),
        supports_image_detail_original: false,
        context_window: None,
        max_context_window: None,
        auto_compact_token_limit: None,
        comp_hash: None,
        effective_context_window_percent: 95,
        experimental_supported_tools: vec![],
        input_modalities: vec![],
        extra_body: None,
        supports_search_tool: false,
        supports_experimental_context: false,
        use_responses_lite: false,
        supports_reasoning_effort_updates: false,
        guardian: None,
        node_repl_auto_review_required: false,
        node_repl_disabled: false,
        auto_review_model_override: None,
        model_specialty: None,
        tool_mode: None,
        multi_agent_version: None,
        multi_agent_reasoning_effort: None,
        multi_agent_tools_in_code_mode: None,
    }
}

#[test]
fn renders_literal_templates() {
    let template = "Hello {{ personality }}\n\n# Personality\n\nFixed instructions";
    let model = test_model(Some(ModelMessages {
        instructions_template: Some(template.to_string()),
        ..Default::default()
    }));
    assert_eq!(
        (
            ResolvedModelMessages::from_model(&model).instructions_template(),
            render_model_instructions(&model)
        ),
        (Some(template), template.to_string()),
    );
}

#[test]
fn missing_and_empty_templates_render_empty_but_retain_presence() {
    for (messages, template) in [
        (None, None),
        (Some(ModelMessages::default()), None),
        (
            Some(ModelMessages {
                instructions_template: Some(String::new()),
                ..Default::default()
            }),
            Some(""),
        ),
    ] {
        let model = test_model(messages);
        assert_eq!(
            (
                render_model_instructions(&model),
                ResolvedModelMessages::from_model(&model).instructions_template()
            ),
            (String::new(), template),
        );
    }
}

#[test]
fn default_instructions_align_file_edit_guidance_with_apply_patch_tool_type() {
    let base_messages = || {
        Some(ModelMessages {
            instructions_template: Some(BASE_INSTRUCTIONS_DEFAULT.to_string()),
            ..Default::default()
        })
    };

    let none_model = test_model(base_messages());
    let rendered_none = render_model_instructions(&none_model);
    assert_eq!(rendered_none, BASE_INSTRUCTIONS_DEFAULT);
    assert!(rendered_none.contains(GENERIC_FILE_EDIT_GUIDANCE));

    for (tool_type, expected_needle, forbidden) in [
        (
            ApplyPatchToolType::Freeform,
            "Use the `apply_patch` tool to edit files",
            ["fuzz_view_edit", "Use `Read` and `Edit`"],
        ),
        (
            ApplyPatchToolType::Hashline,
            "Use `fuzz_view_edit` for text file edits",
            ["Use the `apply_patch` tool", "Use `Read` and `Edit`"],
        ),
        (
            ApplyPatchToolType::ClaudeCode,
            "Use `Read` and `Edit` for file inspection and edits",
            ["fuzz_view_edit", "Use the `apply_patch` tool"],
        ),
    ] {
        let mut model = test_model(base_messages());
        model.apply_patch_tool_type = Some(tool_type);
        let rendered = render_model_instructions(&model);
        assert!(
            rendered.contains(expected_needle),
            "expected `{expected_needle}` for {tool_type:?}"
        );
        for unexpected in forbidden {
            assert!(
                !rendered.contains(unexpected),
                "did not expect `{unexpected}` for {tool_type:?}"
            );
        }
    }

    // Custom catalog prompts are preserved verbatim regardless of apply_patch_tool_type.
    let mut custom = test_model(Some(ModelMessages {
        instructions_template: Some("Custom model instructions.".to_string()),
        ..Default::default()
    }));
    custom.apply_patch_tool_type = Some(ApplyPatchToolType::ClaudeCode);
    assert_eq!(
        render_model_instructions(&custom),
        "Custom model instructions."
    );
}
