#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT/codex-rs"

cargo fmt --check
cargo check --locked --workspace --lib --bins

# Workflow is a large fork-only surface, so run both crates' complete test suites.
just test -p codex-workflow -p codex-workflow-extension

# Run the other fork-critical tests in one Cargo invocation to avoid repeated
# package/feature graph builds.
just test \
  -p codex-api \
  -p codex-cli \
  -p codex-app-server-daemon \
  -p codex-core \
  -p codex-model-provider-info \
  -p codex-tools \
  -p codex-tui \
  -p codex-memories-write \
  -E '
    (package(codex-api) & (test(finishes_reasoning_before_answer_content) | test(anthropic_))) |
    (package(codex-cli) & test(update::tests)) |
    (package(codex-app-server-daemon) & test(codex_plus)) |
    (package(codex-core) & test(config_schema_matches_fixture)) |
    (package(codex-core) & test(fresh_workflow_subagent_persists_ownership_edge_and_close_status)) |
    (package(codex-core) & test(chat_wire_api_posts_to_chat_completions_and_merges_extra_body)) |
    (package(codex-core) & test(anthropic_wire_api_posts_to_messages_and_merges_extra_body)) |
    (package(codex-core) & test(anthropic_wire_api_tool_call_round_trip_sends_tool_result)) |
    (package(codex-core) & test(responses_request_carries_model_extra_body)) |
    (package(codex-core) & test(responses_request_uses_function_exec_when_provider_rejects_custom_exec)) |
    (package(codex-core) & test(function_transport_extracts_raw_javascript)) |
    (package(codex-core) & test(function_transport_rejects_malformed_arguments)) |
    (package(codex-core) & test(code_mode_handler_accepts_custom_and_function_payloads)) |
    (package(codex-core) & test(multi_agent_config_precedence_overrides_remote_model_selector)) |
    (package(codex-core) & test(claude_file_tools_require_read_track_edits_and_reject_external_changes)) |
    (package(codex-model-provider-info) & test(custom_exec_tool_support_defaults_true_and_can_be_disabled)) |
    (package(codex-tools) & test(function_exec_transport_rewrites_exec_and_keeps_apply_patch_custom)) |
    (package(codex-tui) & test(side_model)) |
    (package(codex-tui) & test(open_agent_picker_selects_path_backed_agent)) |
    (package(codex-memories-write) & test(memories_startup_phase1_provider_default_drives_request_model))
  '
