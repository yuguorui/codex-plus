use super::*;
use codex_protocol::items::TurnItem;
use codex_protocol::parse_command::ParsedCommand;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandStatus;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::PatchApplyStatus;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::session::tests::make_session_and_context_with_rx;
use crate::turn_diff_tracker::TurnDiffTracker;

fn anchored(line_no: usize, content: &str) -> String {
    format!(
        "{line_no}:{}|{content}",
        format_short_hash(short_hash(content))
    )
}

#[test]
fn hashes_match_hashline_examples() {
    assert_eq!(xxh32(b"", 0), 0x02cc_5d05);
    assert_eq!(xxh32(b"abc", 0), 0x32d1_53ff);
    assert_eq!(
        short_hash("return decoded   "),
        short_hash("return decoded")
    );
}

#[test]
fn read_output_uses_dense_anchor_format() {
    let doc = Document::parse("alpha\n  beta\n");

    assert_eq!(
        read_output(&doc, None, DEFAULT_CONTEXT_LINES).unwrap(),
        format!("{}\n{}\n", anchored(1, "alpha"), anchored(2, "  beta"))
    );
}

#[test]
fn render_preserves_missing_trailing_newline() {
    let doc = Document::parse("alpha\nbeta");

    assert_eq!(doc.render(), "alpha\nbeta");
}

#[test]
fn edit_replaces_anchor_range_with_multiline_content() {
    let mut doc = Document::parse("alpha\nbeta\ngamma\ndelta\n");
    let args = HashlineArgs {
        action: HashlineAction::Edit,
        path: "demo.txt".to_string(),
        environment_id: None,
        anchor: Some(format!(
            "2:{}..3:{}",
            format_short_hash(short_hash("beta")),
            format_short_hash(short_hash("gamma"))
        )),
        content: Some("B\nC".to_string()),
        before: false,
        context: None,
    };

    let changed = apply_mutation(&mut doc, &args, args.content.as_deref()).unwrap();

    assert_eq!(changed, (1, 2, "Edited".to_string()));
    assert_eq!(doc.render(), "alpha\nB\nC\ndelta\n");
}

#[test]
fn hashline_unified_diff_counts_changed_lines() {
    let diff = hashline_unified_diff("alpha\nbeta\ngamma\ndelta\n", "alpha\nB\nC\ndelta\n");

    assert_diff_line_counts(&diff, 2, 2);
    assert_eq!(
        diff,
        "@@ -1,4 +1,4 @@\n alpha\n-beta\n-gamma\n+B\n+C\n delta\n"
    );
}

#[test]
fn hashline_unified_diff_counts_inserted_lines() {
    let diff = hashline_unified_diff("alpha\nbeta\n", "alpha\none\ntwo\nbeta\n");

    assert_diff_line_counts(&diff, 2, 0);
    assert_eq!(diff, "@@ -1,2 +1,4 @@\n alpha\n+one\n+two\n beta\n");
}

#[test]
fn hashline_unified_diff_counts_deleted_lines() {
    let diff = hashline_unified_diff("alpha\none\ntwo\nbeta\n", "alpha\nbeta\n");

    assert_diff_line_counts(&diff, 0, 2);
    assert_eq!(diff, "@@ -1,4 +1,2 @@\n alpha\n-one\n-two\n beta\n");
}

#[test]
fn hashline_unified_diff_counts_replaced_lines() {
    let diff = hashline_unified_diff("alpha\none\ntwo\nbeta\n", "alpha\nONE\nTWO\nbeta\n");

    assert_diff_line_counts(&diff, 2, 2);
    assert_eq!(
        diff,
        "@@ -1,4 +1,4 @@\n alpha\n-one\n-two\n+ONE\n+TWO\n beta\n"
    );
}

#[test]
fn qualified_anchor_relocates_to_unique_hash_match() {
    let doc = Document::parse("new\nalpha\nbeta\ngamma\n");
    let anchor = format!("2:{}", format_short_hash(short_hash("beta")));

    let resolved = resolve_anchor(&doc, parse_anchor(&anchor).unwrap()).unwrap();

    assert_eq!(resolved.index, 2);
    assert!(resolved.relocated);
}

#[tokio::test]
async fn mutation_emits_standard_file_change_item() {
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
    let changes = HashMap::from([(
        PathBuf::from("demo.txt"),
        FileChange::Update {
            unified_diff: hashline_unified_diff("alpha\nbeta\n", "alpha\nBETA\n"),
            move_path: None,
        },
    )]);
    let emitter = ToolEmitter::apply_patch_for_environment(changes, /*auto_approved*/ true, String::new());
    let event_ctx = ToolEventCtx::new(
        session.as_ref(),
        turn.as_ref(),
        "call-hashline",
        Some(&tracker),
    );
    emitter.begin(event_ctx).await;
    let event_ctx = ToolEventCtx::new(
        session.as_ref(),
        turn.as_ref(),
        "call-hashline",
        Some(&tracker),
    );
    emitter
        .finish(
            event_ctx,
            Ok(hashline_output(0, String::new(), String::new())),
            None,
        )
        .await
        .expect("hashline file change succeeds");

    let started = rx_event.recv().await.expect("item started event");
    assert!(matches!(
        started.msg,
        EventMsg::ItemStarted(event)
            if matches!(
                &event.item,
                TurnItem::FileChange(file_change)
                    if file_change.id == "call-hashline"
                        && file_change.status.is_none()
                        && file_change.auto_approved == Some(true)
            )
    ));

    let completed = loop {
        let event = tokio::time::timeout(Duration::from_secs(1), rx_event.recv())
            .await
            .expect("item completed event")
            .expect("channel open");
        if let EventMsg::ItemCompleted(completed) = event.msg {
            break completed;
        }
    };
    let TurnItem::FileChange(file_change) = completed.item else {
        panic!("expected completed file change item");
    };
    assert_eq!(file_change.id, "call-hashline");
    assert_eq!(file_change.status, Some(PatchApplyStatus::Completed));
    assert!(file_change.auto_approved.is_none());
}

#[tokio::test]
async fn read_emits_standard_explored_command_item() {
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let path =
        AbsolutePathBuf::try_from(PathBuf::from("/tmp/hashline_demo.txt")).expect("absolute path");
    let cwd = turn
        .environments
        .primary()
        .expect("primary environment")
        .cwd
        .clone();
    let emitter = hashline_read_emitter(HashlineAction::Read, &path, cwd);
    let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), "call-hashline", None);
    emitter.begin(event_ctx).await;
    let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), "call-hashline", None);
    emitter
        .finish(
            event_ctx,
            Ok(hashline_output(
                0,
                "1:aa|alpha\n".to_string(),
                String::new(),
            )),
            None,
        )
        .await
        .expect("hashline read event succeeds");

    let started = rx_event.recv().await.expect("exec begin event");
    let EventMsg::ExecCommandBegin(begin) = started.msg else {
        panic!("expected exec command begin");
    };
    assert_eq!(begin.call_id, "call-hashline");
    assert_eq!(
        begin.parsed_cmd,
        vec![ParsedCommand::Read {
            cmd: "hashline read /tmp/hashline_demo.txt".to_string(),
            name: "hashline_demo.txt".to_string(),
            path: PathBuf::from("/tmp/hashline_demo.txt"),
        }]
    );

    let completed = loop {
        let event = tokio::time::timeout(Duration::from_secs(1), rx_event.recv())
            .await
            .expect("exec command end event")
            .expect("channel open");
        if let EventMsg::ExecCommandEnd(end) = event.msg {
            break end;
        }
    };
    assert_eq!(completed.status, ExecCommandStatus::Completed);
    assert_eq!(completed.stdout, "1:aa|alpha\n");
    assert_eq!(completed.parsed_cmd, begin.parsed_cmd);
}

fn assert_diff_line_counts(diff: &str, expected_added: usize, expected_removed: usize) {
    let (added, removed) = diff
        .lines()
        .filter(|line| !line.starts_with("+++") && !line.starts_with("---"))
        .fold((0, 0), |(added, removed), line| {
            if line.starts_with('+') {
                (added + 1, removed)
            } else if line.starts_with('-') {
                (added, removed + 1)
            } else {
                (added, removed)
            }
        });

    assert_eq!((added, removed), (expected_added, expected_removed));
}

#[test]
fn bare_hash_rejects_ambiguous_matches() {
    let doc = Document::parse("alpha\nbeta\nalpha\n");
    let anchor = format_short_hash(short_hash("alpha"));

    let error = resolve_anchor(&doc, parse_anchor(&anchor).unwrap()).unwrap_err();

    match error {
        FunctionCallError::RespondToModel(message) => {
            assert!(message.contains("ambiguous"));
            assert!(message.contains("1, 3"));
        }
        FunctionCallError::Fatal(message) => panic!("unexpected fatal error: {message}"),
    }
}

#[test]
fn stale_anchor_reports_fresh_line_context() {
    let doc = Document::parse("alpha\nDELTA\ngamma\n");
    let stale = format!("2:{}", format_short_hash(short_hash("beta")));

    let error = resolve_anchor(&doc, parse_anchor(&stale).unwrap()).unwrap_err();

    match error {
        FunctionCallError::RespondToModel(message) => {
            assert!(message.contains("content changed"));
            assert!(message.contains(">>> 2:"));
            assert!(message.contains("|DELTA"));
        }
        FunctionCallError::Fatal(message) => panic!("unexpected fatal error: {message}"),
    }
}
