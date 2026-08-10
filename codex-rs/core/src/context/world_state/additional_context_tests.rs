use std::collections::BTreeMap;

use crate::context::WorkflowChildIsolation;
use crate::context::WorkflowChildOutputContract;
use crate::context::WorkflowChildPreamble;
use crate::context::WorkflowChildTask;
use crate::context_manager::updates::merge_world_state_updates;
use codex_history::ResponseItemEnvelope;
use codex_otel::SessionTelemetry;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use pretty_assertions::assert_eq;

use super::*;

#[test]
fn workflow_child_context_is_restored_from_world_state() {
    let task = format!("start:{}:end", "界".repeat(300));
    let mut context =
        BTreeMap::from([WorkflowChildPreamble::new("workflow preamble").into_additional_context()]);
    for fragment in WorkflowChildIsolation::parts("workflow isolation") {
        let (key, entry) = fragment.into_additional_context();
        context.insert(key, entry);
    }
    for fragment in WorkflowChildOutputContract::parts("workflow output contract") {
        let (key, entry) = fragment.into_additional_context();
        context.insert(key, entry);
    }
    for fragment in WorkflowChildTask::parts(task) {
        let (key, entry) = fragment.into_additional_context();
        context.insert(key, entry);
    }
    let telemetry = SessionTelemetry::new(
        ThreadId::new(),
        "gpt-test",
        "gpt-test",
        /*account_id*/ None,
        /*account_email*/ None,
        /*auth_mode*/ None,
        "test-originator".to_string(),
        /*log_user_prompts*/ false,
        "test-terminal".to_string(),
        SessionSource::Cli,
    );
    let state = AdditionalContextState::new(
        &context,
        /*tool_outputs*/ false,
        &telemetry,
        &codex_thread_context_ids_test_support::thread_context_ids(),
    );

    let (snapshot, updates) = state.render_diff(PreviousSectionState::Absent);
    let initially_inserted = merge_world_state_updates(updates)
        .into_iter()
        .map(ResponseItemEnvelope::new)
        .collect::<Vec<_>>();
    let mut retained = Vec::new();
    let snapshot = serde_json::to_value(snapshot.expect("additional context snapshot"))
        .expect("serialize additional context snapshot");
    let mut sections = serde_json::Map::new();
    sections.insert(AdditionalContextState::ID.to_string(), snapshot);
    let restored = WorldStateSnapshot::from(&sections)
        .restore_input_context(&initially_inserted, &mut retained);

    assert_eq!(restored, initially_inserted);
}
