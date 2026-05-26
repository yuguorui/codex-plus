#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT/codex-rs"

# Use the symbol-stripped dev-small profile by default to speed up linking and
# keep disk usage low during daily rebases while sharing one target directory
# across cargo check and nextest.
CARGO_PROFILE="${CODEX_FORK_CARGO_PROFILE:-dev-small}"

cargo fmt --check
cargo check --locked --profile "$CARGO_PROFILE" --workspace --lib --bins

# Workflow is a large fork-only surface, so run both crates' complete test suites.
just test --cargo-profile "$CARGO_PROFILE" -p codex-workflow -p codex-workflow-extension

# Fork-only provider and protocol surfaces. These crates are small enough to
# run completely instead of maintaining a fragile list of individual tests.
just test \
  --cargo-profile "$CARGO_PROFILE" \
  -p codex-api \
  -p codex-protocol \
  -p codex-code-mode-runtime \
  -p codex-model-provider-info \
  -p codex-model-provider \
  -p codex-memories-write \
  -p codex-skills \
  -p codex-tools

# The app-server protocol and daemon package layout changed in this fork.
just test --cargo-profile "$CARGO_PROFILE" -p codex-app-server-protocol -p codex-app-server-daemon

# Keep app-server workflow/transport coverage at the lib level; integration
# tests that spawn the binary are covered by their dedicated CI suites.
just test --cargo-profile "$CARGO_PROFILE" -p codex-app-server --lib \
  -E 'test(workflow_events) | test(transport)'

# Keep the legacy wire/updater/UI spot checks that are not covered above.
just test \
  --cargo-profile "$CARGO_PROFILE" \
  -p codex-cli \
  -p codex-core \
  -p codex-tui \
  -E '
    (package(codex-cli) & test(update::tests)) |
    (package(codex-cli) & test(workflow_cmd::tests)) |
    (package(codex-core) & test(config_schema_matches_fixture)) |
    (package(codex-core) & test(exec_command_enforces_bash_command_clamp_before_execution)) |
    (package(codex-core) & test(exec_command_clamp_forces_a_non_login_default_shell)) |
    (package(codex-core) & test(fresh_workflow_subagent_persists_ownership_edge_and_close_status)) |
    (package(codex-core) & test(chat_wire_api_posts_to_chat_completions_and_merges_extra_body)) |
    (package(codex-core) & test(anthropic_wire_api_posts_to_messages_and_merges_extra_body)) |
    (package(codex-core) & test(anthropic_wire_api_tool_call_round_trip_sends_tool_result)) |
    (package(codex-core) & test(responses_request_carries_model_extra_body)) |
    (package(codex-core) & test(responses_request_uses_function_exec_when_provider_rejects_custom_exec)) |
    (package(codex-core) & test(function_transport_extracts_raw_javascript)) |
    (package(codex-core) & test(function_transport_rejects_malformed_arguments)) |
    (package(codex-core) & test(code_mode_handler_accepts_custom_and_function_payloads)) |
    (package(codex-core) & test(tools::handlers::tool_search::tests)) |
    (package(codex-core) & test(tool_search_terminal_outputs_pair_with_the_call_transport)) |
    (package(codex-core) & test(aborted_tool_output_pairs_with_the_call_transport)) |
    (package(codex-core) & test(build_tool_call_routes_function_tool_search_to_search_payload)) |
    (package(codex-core) & test(build_tool_call_rejects_malformed_function_tool_search_arguments)) |
    (package(codex-core) & test(activated_tool_search_tools_are_redeclared_for_function_backed_providers)) |
    (package(codex-core) & test(compaction_checkpoint_recovers_tool_search_activations_on_bounded_resume)) |
    (package(codex-core) & test(hashline)) |
    (package(codex-core) & test(cancel_pending_user_inputs_removes_steers_without_history_events)) |
    (package(codex-core) & test(steer_waits_for_non_native_sse_response_to_complete)) |
    (package(codex-core) & test(multi_agent_config_precedence_overrides_remote_model_selector)) |
    (package(codex-core) & test(claude_file_tools_require_read_track_edits_and_reject_external_changes)) |
    (package(codex-tui) & test(side_model)) |
    (package(codex-tui) & test(open_agent_picker_selects_path_backed_agent)) |
    (package(codex-tui) & test(pending_input_preview::tests)) |
    (package(codex-tui) & test(pending_steer_cancel_hint_follows_effective_bindings)) |
    (package(codex-tui) & test(rapid_double_esc_requests_all_pending_steer_cancellation)) |
    (package(codex-tui) & test(confirmed_steer_cancellation_restores_all_messages_in_order)) |
    (package(codex-tui) & test(local_tool_history_validation_error)) |
    (package(codex-tui) & test(pets::) & test(bongo)) |
    (package(codex-tui) & test(ascii_bongo_pet_renders_visible_braille_cells_in_chat_area)) |
    (package(codex-tui) & test(slash_pets_renders_ascii_bongo_preview_without_image_protocols))
  '
