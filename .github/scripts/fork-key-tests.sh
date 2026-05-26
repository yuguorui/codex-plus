#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT/codex-rs"

cargo fmt --check
cargo check --locked --workspace --lib --bins

# Workflow is a large fork-only surface, so run both crates' complete test suites.
just test -p codex-workflow -p codex-workflow-extension

# Fork-only provider and protocol surfaces. These crates are small enough to
# run completely instead of maintaining a fragile list of individual tests.
just test \
  -p codex-api \
  -p codex-protocol \
  -p codex-code-mode-runtime \
  -p codex-model-provider-info \
  -p codex-model-provider \
  -p codex-memories-write \
  -p codex-skills \
  -p codex-tools

# The app-server protocol and daemon package layout changed in this fork.
just test -p codex-app-server-protocol -p codex-app-server-daemon

# Keep app-server workflow/transport coverage at the lib level; integration
# tests that spawn the binary are covered by their dedicated CI suites.
just test -p codex-app-server --lib \
  -E 'test(workflow_events) | test(transport)'

# Keep the legacy wire/updater/UI spot checks that are not covered above.
just test \
  -p codex-cli \
  -p codex-core \
  -p codex-tui \
  -E '
    (package(codex-cli) & test(update::tests)) |
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
    (package(codex-tui) & test(side_model)) |
    (package(codex-tui) & test(open_agent_picker_selects_path_backed_agent))
  '
