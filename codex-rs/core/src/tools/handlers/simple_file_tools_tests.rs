use super::*;
use crate::session::tests::make_session_and_context;
use crate::session::tests::make_session_and_context_with_dynamic_tools_and_rx;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolOutput;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_protocol::items::McpToolCallItem;
use codex_protocol::items::McpToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandBeginEvent;
use codex_protocol::protocol::ExecCommandEndEvent;
use codex_protocol::protocol::PatchApplyStatus;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

fn output_text(output: &dyn ToolOutput, call_id: &str) -> String {
    let item = output.to_response_item(
        call_id,
        &ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    );
    let ResponseInputItem::FunctionCallOutput { output, .. } = item else {
        panic!("expected function call output");
    };
    let FunctionCallOutputBody::Text(text) = output.body else {
        panic!("expected text output");
    };
    text
}

fn set_turn_cwd(turn: &mut crate::session::turn_context::TurnContext, cwd: &Path) {
    let cwd = AbsolutePathBuf::from_absolute_path(cwd).expect("absolute cwd");
    turn.environments
        .turn_environments
        .first_mut()
        .expect("default local turn environment")
        .cwd = cwd;
    turn.permission_profile = PermissionProfile::Disabled;
}

async fn invocation(tool_name: &str, arguments: serde_json::Value, cwd: &Path) -> ToolInvocation {
    let (session, mut turn) = make_session_and_context().await;
    set_turn_cwd(&mut turn, cwd);
    invocation_from_parts(
        Arc::new(session),
        Arc::new(turn),
        Arc::new(Mutex::new(TurnDiffTracker::new())),
        tool_name,
        arguments,
    )
}

fn invocation_from_parts(
    session: Arc<crate::session::session::Session>,
    turn: Arc<crate::session::turn_context::TurnContext>,
    tracker: Arc<Mutex<TurnDiffTracker>>,
    tool_name: &str,
    arguments: serde_json::Value,
) -> ToolInvocation {
    ToolInvocation {
        session,
        turn,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker,
        call_id: format!("call-{tool_name}"),
        tool_name: ToolName::plain(tool_name),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: arguments.to_string(),
        },
    }
}

struct ToolTestContext {
    session: Arc<crate::session::session::Session>,
    turn: Arc<crate::session::turn_context::TurnContext>,
    tracker: Arc<Mutex<TurnDiffTracker>>,
}

impl ToolTestContext {
    async fn new(cwd: &Path) -> Self {
        let (session, mut turn) = make_session_and_context().await;
        set_turn_cwd(&mut turn, cwd);
        Self {
            session: Arc::new(session),
            turn: Arc::new(turn),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        }
    }

    async fn with_events(
        cwd: &Path,
    ) -> (
        Self,
        async_channel::Receiver<codex_protocol::protocol::Event>,
    ) {
        let (session, mut turn, rx_event) =
            make_session_and_context_with_dynamic_tools_and_rx(Vec::new()).await;
        set_turn_cwd(
            Arc::get_mut(&mut turn).expect("only test should hold turn context"),
            cwd,
        );
        (
            Self {
                session,
                turn,
                tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            },
            rx_event,
        )
    }

    fn invocation(&self, tool_name: &str, arguments: serde_json::Value) -> ToolInvocation {
        invocation_from_parts(
            Arc::clone(&self.session),
            Arc::clone(&self.turn),
            Arc::clone(&self.tracker),
            tool_name,
            arguments,
        )
    }

    async fn read(&self, file_path: &str) {
        ReadHandler::new(SimpleFileToolOptions {
            include_environment_id: false,
        })
        .handle(self.invocation("Read", json!({ "file_path": file_path })))
        .await
        .expect("read should succeed");
    }
}

async fn invocation_with_events(
    tool_name: &str,
    arguments: serde_json::Value,
    cwd: &Path,
) -> (
    ToolInvocation,
    async_channel::Receiver<codex_protocol::protocol::Event>,
) {
    let (session, mut turn, rx_event) =
        make_session_and_context_with_dynamic_tools_and_rx(Vec::new()).await;
    set_turn_cwd(
        Arc::get_mut(&mut turn).expect("only test should hold turn context"),
        cwd,
    );
    let invocation = ToolInvocation {
        session,
        turn,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: format!("call-{tool_name}"),
        tool_name: ToolName::plain(tool_name),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: arguments.to_string(),
        },
    };
    (invocation, rx_event)
}

async fn completed_file_change(
    rx_event: &async_channel::Receiver<codex_protocol::protocol::Event>,
) -> codex_protocol::items::FileChangeItem {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(1), rx_event.recv())
            .await
            .expect("file change event")
            .expect("event channel open");
        if let EventMsg::ItemCompleted(completed) = event.msg
            && let TurnItem::FileChange(item) = completed.item
        {
            return item;
        }
    }
}

async fn started_file_change(
    rx_event: &async_channel::Receiver<codex_protocol::protocol::Event>,
) -> codex_protocol::items::FileChangeItem {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(1), rx_event.recv())
            .await
            .expect("file change event")
            .expect("event channel open");
        if let EventMsg::ItemStarted(started) = event.msg
            && let TurnItem::FileChange(item) = started.item
        {
            return item;
        }
    }
}

async fn completed_mcp_tool_call(
    rx_event: &async_channel::Receiver<codex_protocol::protocol::Event>,
) -> McpToolCallItem {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(1), rx_event.recv())
            .await
            .expect("mcp tool call event")
            .expect("event channel open");
        if let EventMsg::ItemCompleted(completed) = event.msg
            && let TurnItem::McpToolCall(item) = completed.item
        {
            return item;
        }
    }
}

async fn bash_exec_events(
    rx_event: &async_channel::Receiver<codex_protocol::protocol::Event>,
) -> (ExecCommandBeginEvent, ExecCommandEndEvent) {
    let mut begin = None;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(1), rx_event.recv())
            .await
            .expect("exec command event")
            .expect("event channel open");
        match event.msg {
            EventMsg::ExecCommandBegin(event) => begin = Some(event),
            EventMsg::ExecCommandEnd(end) => {
                return (begin.expect("begin event before end"), end);
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn read_returns_line_numbered_page() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        temp_dir.path().join("sample.txt"),
        "one\ntwo\nthree\nfour\n",
    )
    .expect("write file");

    let output = ReadHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Read",
            json!({ "file_path": "sample.txt", "offset": 2, "limit": 2 }),
            temp_dir.path(),
        )
        .await,
    )
    .await
    .expect("read should succeed");

    assert_eq!(
        output_text(output.as_ref(), "call-Read"),
        "     2\ttwo\n     3\tthree\n... 1 more lines. Use offset=4 to continue."
    );
}

#[tokio::test]
async fn read_emits_codex_tool_call_item_for_ui() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("sample.txt"), "one\n").expect("write file");
    let (invocation, rx_event) = invocation_with_events(
        "Read",
        json!({ "file_path": "sample.txt" }),
        temp_dir.path(),
    )
    .await;

    ReadHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(invocation)
    .await
    .expect("read should succeed");

    let item = completed_mcp_tool_call(&rx_event).await;
    assert_eq!(item.server, "codex++");
    assert_eq!(item.tool, "Read");
}

#[tokio::test]
async fn read_reports_missing_file() {
    let temp_dir = tempfile::tempdir().expect("tempdir");

    let result = ReadHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Read",
            json!({ "file_path": "missing.txt" }),
            temp_dir.path(),
        )
        .await,
    )
    .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected missing-file error");
    };
    assert!(message.contains("file does not exist"), "{message}");
}

#[tokio::test]
async fn read_missing_file_completes_ui_item_as_failed() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (invocation, rx_event) = invocation_with_events(
        "Read",
        json!({ "file_path": "missing.txt" }),
        temp_dir.path(),
    )
    .await;

    let result = ReadHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(invocation)
    .await;

    assert!(matches!(result, Err(FunctionCallError::RespondToModel(_))));
    let item = completed_mcp_tool_call(&rx_event).await;
    assert_eq!(item.status, McpToolCallStatus::Failed);
    assert!(
        item.error
            .as_ref()
            .is_some_and(|error| error.message.contains("file does not exist")),
        "{item:?}"
    );
}

#[tokio::test]
async fn read_returns_empty_output_when_offset_is_past_end() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("sample.txt"), "one\ntwo\n").expect("write file");

    let output = ReadHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Read",
            json!({ "file_path": "sample.txt", "offset": 99, "limit": 10 }),
            temp_dir.path(),
        )
        .await,
    )
    .await
    .expect("read should succeed");

    assert_eq!(output_text(output.as_ref(), "call-Read"), "");
}

#[tokio::test]
async fn read_rejects_non_utf8_file() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("binary.dat"), [0xff, 0xfe]).expect("write file");

    let result = ReadHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Read",
            json!({ "file_path": "binary.dat" }),
            temp_dir.path(),
        )
        .await,
    )
    .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected utf-8 error");
    };
    assert!(message.contains("not valid UTF-8"), "{message}");
}

#[tokio::test]
async fn read_rejects_pdf_extension_with_non_pdf_bytes() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("sample.pdf"), "not a pdf\n").expect("write file");

    let result = ReadHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Read",
            json!({ "file_path": "sample.pdf" }),
            temp_dir.path(),
        )
        .await,
    )
    .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected invalid PDF error");
    };
    assert!(message.contains("file is not a valid PDF"), "{message}");
}

#[tokio::test]
async fn read_rejects_empty_pdf_file() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("empty.pdf"), []).expect("write file");

    let result = ReadHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(invocation("Read", json!({ "file_path": "empty.pdf" }), temp_dir.path()).await)
    .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected empty PDF error");
    };
    assert!(message.contains("PDF file is empty"), "{message}");
}

#[tokio::test]
async fn read_pdf_rejects_page_range_past_end() {
    if which::which("pdfinfo").is_err() || which::which("pdftotext").is_err() {
        return;
    }
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        temp_dir.path().join("sample.pdf"),
        minimal_pdf_bytes("Hello PDF"),
    )
    .expect("write pdf");

    let result = ReadHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Read",
            json!({ "file_path": "sample.pdf", "pages": "2" }),
            temp_dir.path(),
        )
        .await,
    )
    .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected page range error");
    };
    assert!(message.contains("starts at page 2"), "{message}");
}

#[tokio::test]
async fn read_rejects_pdf_pages_parameter() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("sample.txt"), "text\n").expect("write file");

    let result = ReadHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Read",
            json!({ "file_path": "sample.txt", "pages": "1-2" }),
            temp_dir.path(),
        )
        .await,
    )
    .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected pages unsupported error");
    };
    assert!(
        message.contains("only supported for PDF files"),
        "{message}"
    );
}

#[tokio::test]
async fn read_pdf_pages_extracts_text_with_poppler() {
    if which::which("pdfinfo").is_err() || which::which("pdftotext").is_err() {
        return;
    }
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        temp_dir.path().join("sample.pdf"),
        minimal_pdf_bytes("Hello PDF"),
    )
    .expect("write pdf");

    let output = ReadHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Read",
            json!({ "file_path": "sample.pdf", "pages": "1" }),
            temp_dir.path(),
        )
        .await,
    )
    .await
    .expect("pdf read should succeed");

    let text = output_text(output.as_ref(), "call-Read");
    assert!(text.contains("PDF text extracted"), "{text}");
    assert!(text.contains("Hello PDF"), "{text}");
}

#[tokio::test]
async fn read_defaults_to_claude_line_limit() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let content = (1..=DEFAULT_READ_LINE_LIMIT + 1)
        .map(|line| format!("line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(temp_dir.path().join("long.txt"), content).expect("write file");

    let output = ReadHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(invocation("Read", json!({ "file_path": "long.txt" }), temp_dir.path()).await)
    .await
    .expect("read should succeed");

    let text = output_text(output.as_ref(), "call-Read");
    assert!(text.contains("  2000\tline 2000"), "{text}");
    assert!(!text.contains("line 2001"), "{text}");
    assert!(
        text.contains("... 1 more lines. Use offset=2001 to continue."),
        "{text}"
    );
}

#[tokio::test]
async fn edit_rejects_existing_file_without_prior_read() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("sample.txt"), "before\n").expect("write file");

    let result = EditHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Edit",
            json!({
                "file_path": "sample.txt",
                "old_string": "before",
                "new_string": "after",
            }),
            temp_dir.path(),
        )
        .await,
    )
    .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected read-before-edit error");
    };
    assert!(message.contains("must Read"), "{message}");
    assert_eq!(
        std::fs::read_to_string(temp_dir.path().join("sample.txt")).expect("read file"),
        "before\n"
    );
}

#[tokio::test]
async fn edit_rejects_file_changed_after_read() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("sample.txt"), "before\n").expect("write file");
    let context = ToolTestContext::new(temp_dir.path()).await;
    context.read("sample.txt").await;
    std::fs::write(temp_dir.path().join("sample.txt"), "before changed\n").expect("write file");

    let result = EditHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(context.invocation(
        "Edit",
        json!({
            "file_path": "sample.txt",
            "old_string": "before changed",
            "new_string": "after",
        }),
    ))
    .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected stale-read error");
    };
    assert!(message.contains("changed after the last Read"), "{message}");
    assert_eq!(
        std::fs::read_to_string(temp_dir.path().join("sample.txt")).expect("read file"),
        "before changed\n"
    );
}

#[tokio::test]
async fn partial_read_does_not_authorize_edit() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("sample.txt"), "one\ntwo\n").expect("write file");
    let context = ToolTestContext::new(temp_dir.path()).await;
    ReadHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(context.invocation("Read", json!({ "file_path": "sample.txt", "limit": 1 })))
    .await
    .expect("partial read should succeed");

    let result = EditHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(context.invocation(
        "Edit",
        json!({
            "file_path": "sample.txt",
            "old_string": "one",
            "new_string": "uno",
        }),
    ))
    .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected partial-read guard error");
    };
    assert!(message.contains("must Read"), "{message}");
}

#[tokio::test]
async fn edit_updates_read_snapshot_for_followup_edit() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("sample.txt"), "alpha beta\n").expect("write file");
    let context = ToolTestContext::new(temp_dir.path()).await;
    context.read("sample.txt").await;

    EditHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(context.invocation(
        "Edit",
        json!({
            "file_path": "sample.txt",
            "old_string": "alpha",
            "new_string": "omega",
        }),
    ))
    .await
    .expect("first edit should succeed");
    EditHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(context.invocation(
        "Edit",
        json!({
            "file_path": "sample.txt",
            "old_string": "beta",
            "new_string": "delta",
        }),
    ))
    .await
    .expect("second edit should use updated snapshot");

    assert_eq!(
        std::fs::read_to_string(temp_dir.path().join("sample.txt")).expect("read file"),
        "omega delta\n"
    );
}

#[tokio::test]
async fn edit_rejects_multiple_matches_without_replace_all() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("sample.txt"), "same\nsame\n").expect("write file");
    let context = ToolTestContext::new(temp_dir.path()).await;
    context.read("sample.txt").await;

    let result = EditHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(context.invocation(
        "Edit",
        json!({
            "file_path": "sample.txt",
            "old_string": "same",
            "new_string": "changed",
        }),
    ))
    .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected multiple-match error");
    };
    assert!(message.contains("found 2 matches"), "{message}");
}

#[tokio::test]
async fn edit_rejects_empty_old_string() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("sample.txt"), "same\n").expect("write file");

    let result = EditHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Edit",
            json!({
                "file_path": "sample.txt",
                "old_string": "",
                "new_string": "changed",
            }),
            temp_dir.path(),
        )
        .await,
    )
    .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected empty old_string error");
    };
    assert!(
        message.contains("old_string must not be empty"),
        "{message}"
    );
}

#[tokio::test]
async fn edit_rejects_missing_old_string() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("sample.txt"), "alpha\n").expect("write file");
    let context = ToolTestContext::new(temp_dir.path()).await;
    context.read("sample.txt").await;

    let result = EditHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(context.invocation(
        "Edit",
        json!({
            "file_path": "sample.txt",
            "old_string": "beta",
            "new_string": "changed",
        }),
    ))
    .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected no-match error");
    };
    assert!(message.contains("found no match"), "{message}");
}

#[tokio::test]
async fn edit_reports_missing_file_without_creating_it() {
    let temp_dir = tempfile::tempdir().expect("tempdir");

    let result = EditHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Edit",
            json!({
                "file_path": "missing.txt",
                "old_string": "before",
                "new_string": "after",
            }),
            temp_dir.path(),
        )
        .await,
    )
    .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected missing file error");
    };
    assert!(message.contains("file does not exist"), "{message}");
    assert!(!temp_dir.path().join("missing.txt").exists());
}

#[tokio::test]
async fn edit_replace_all_updates_every_match() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("sample.txt"), "same\nsame\n").expect("write file");
    let context = ToolTestContext::new(temp_dir.path()).await;
    context.read("sample.txt").await;

    EditHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(context.invocation(
        "Edit",
        json!({
            "file_path": "sample.txt",
            "old_string": "same",
            "new_string": "changed",
            "replace_all": true,
        }),
    ))
    .await
    .expect("replace_all edit should succeed");

    assert_eq!(
        std::fs::read_to_string(temp_dir.path().join("sample.txt")).expect("read file"),
        "changed\nchanged\n"
    );
}

#[tokio::test]
async fn edit_replaces_the_single_exact_match_only() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        temp_dir.path().join("sample.txt"),
        "prefix target\npretarget\n",
    )
    .expect("write file");
    let context = ToolTestContext::new(temp_dir.path()).await;
    context.read("sample.txt").await;

    EditHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(context.invocation(
        "Edit",
        json!({
            "file_path": "sample.txt",
            "old_string": "prefix target",
            "new_string": "updated",
        }),
    ))
    .await
    .expect("single edit should succeed");

    assert_eq!(
        std::fs::read_to_string(temp_dir.path().join("sample.txt")).expect("read file"),
        "updated\npretarget\n"
    );
}

#[tokio::test]
async fn edit_writes_file_and_emits_update_diff() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("sample.txt"), "before\nkeep\n").expect("write file");
    let (context, rx_event) = ToolTestContext::with_events(temp_dir.path()).await;
    context.read("sample.txt").await;
    let invocation = context.invocation(
        "Edit",
        json!({
            "file_path": "sample.txt",
            "old_string": "before",
            "new_string": "after",
        }),
    );
    let output = EditHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(invocation)
    .await
    .expect("edit should succeed");

    assert_eq!(
        std::fs::read_to_string(temp_dir.path().join("sample.txt")).expect("read file"),
        "after\nkeep\n"
    );
    assert_eq!(
        output_text(output.as_ref(), "call-Edit"),
        format!("Edited `{}`.", temp_dir.path().join("sample.txt").display())
    );

    let started = started_file_change(&rx_event).await;
    assert_eq!(started.tool_name.as_deref(), Some("Edit"));
    assert_eq!(started.status, None);
    let file_change = completed_file_change(&rx_event).await;
    assert_eq!(file_change.tool_name.as_deref(), Some("Edit"));
    let change = file_change
        .changes
        .get(&temp_dir.path().join("sample.txt"))
        .expect("edit should emit a file change");
    let FileChange::Update { unified_diff, .. } = change else {
        panic!("expected update file change, got {change:?}");
    };
    assert!(unified_diff.contains("-before"), "{unified_diff}");
    assert!(unified_diff.contains("+after"), "{unified_diff}");
}

#[tokio::test]
async fn write_rejects_existing_file_without_prior_read() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("existing.txt"), "old\n").expect("write file");

    let result = WriteHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Write",
            json!({
                "file_path": "existing.txt",
                "content": "new\n",
            }),
            temp_dir.path(),
        )
        .await,
    )
    .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected read-before-write error");
    };
    assert!(message.contains("must Read"), "{message}");
    assert_eq!(
        std::fs::read_to_string(temp_dir.path().join("existing.txt")).expect("read file"),
        "old\n"
    );
}

#[tokio::test]
async fn write_rejects_file_changed_after_read() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("existing.txt"), "old\n").expect("write file");
    let context = ToolTestContext::new(temp_dir.path()).await;
    context.read("existing.txt").await;
    std::fs::write(temp_dir.path().join("existing.txt"), "changed\n").expect("write file");

    let result = WriteHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(context.invocation(
        "Write",
        json!({
            "file_path": "existing.txt",
            "content": "new\n",
        }),
    ))
    .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected stale-read error");
    };
    assert!(message.contains("changed after the last Read"), "{message}");
    assert_eq!(
        std::fs::read_to_string(temp_dir.path().join("existing.txt")).expect("read file"),
        "changed\n"
    );
}

#[tokio::test]
async fn write_overwrites_existing_file() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("existing.txt"), "old\n").expect("write file");
    let (context, rx_event) = ToolTestContext::with_events(temp_dir.path()).await;
    context.read("existing.txt").await;
    let invocation = context.invocation(
        "Write",
        json!({
            "file_path": "existing.txt",
            "content": "new\n",
        }),
    );
    WriteHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(invocation)
    .await
    .expect("write should succeed");

    assert_eq!(
        std::fs::read_to_string(temp_dir.path().join("existing.txt")).expect("read file"),
        "new\n"
    );

    let file_change = completed_file_change(&rx_event).await;
    assert_eq!(file_change.tool_name.as_deref(), Some("Write"));
    let change = file_change
        .changes
        .get(&temp_dir.path().join("existing.txt"))
        .expect("write should emit an update file change");
    let FileChange::Update { unified_diff, .. } = change else {
        panic!("expected update file change, got {change:?}");
    };
    assert!(unified_diff.contains("-old"), "{unified_diff}");
    assert!(unified_diff.contains("+new"), "{unified_diff}");
}

#[tokio::test]
async fn write_refuses_to_overwrite_non_utf8_existing_file() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let path = temp_dir.path().join("binary.dat");
    std::fs::write(&path, [0xff, 0xfe]).expect("write file");

    let result = WriteHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Write",
            json!({
                "file_path": "binary.dat",
                "content": "text\n",
            }),
            temp_dir.path(),
        )
        .await,
    )
    .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected non-UTF-8 error");
    };
    assert!(message.contains("not valid UTF-8"), "{message}");
    assert_eq!(std::fs::read(&path).expect("read file"), vec![0xff, 0xfe]);
}

#[tokio::test]
async fn write_reports_filesystem_errors() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (invocation, rx_event) = invocation_with_events(
        "Write",
        json!({
            "file_path": "missing-parent/file.txt",
            "content": "text\n",
        }),
        temp_dir.path(),
    )
    .await;

    let result = WriteHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(invocation)
    .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected filesystem error");
    };
    assert!(message.contains("failed to write"), "{message}");
    let started = started_file_change(&rx_event).await;
    assert_eq!(started.tool_name.as_deref(), Some("Write"));
    assert_eq!(started.status, None);
    let completed = completed_file_change(&rx_event).await;
    assert_eq!(completed.tool_name.as_deref(), Some("Write"));
    assert_eq!(completed.status, Some(PatchApplyStatus::Failed));
}

#[tokio::test]
async fn write_creates_file_and_emits_add_change() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (invocation, rx_event) = invocation_with_events(
        "Write",
        json!({
            "file_path": "created.txt",
            "content": "hello\n",
        }),
        temp_dir.path(),
    )
    .await;

    WriteHandler::new(SimpleFileToolOptions {
        include_environment_id: false,
    })
    .handle(invocation)
    .await
    .expect("write should succeed");

    assert_eq!(
        std::fs::read_to_string(temp_dir.path().join("created.txt")).expect("read file"),
        "hello\n"
    );
    let file_change = completed_file_change(&rx_event).await;
    assert_eq!(file_change.tool_name.as_deref(), Some("Write"));
    let change = file_change
        .changes
        .get(&temp_dir.path().join("created.txt"))
        .expect("write should emit an add file change");
    assert_eq!(
        change,
        &FileChange::Add {
            content: "hello\n".to_string()
        }
    );
}

#[tokio::test]
async fn bash_executes_command_in_turn_cwd() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (invocation, rx_event) = invocation_with_events(
        "Bash",
        json!({
            "command": "printf smoke > bash-created.txt",
            "timeout": 1000,
        }),
        temp_dir.path(),
    )
    .await;
    BashHandler::new(
        SimpleFileToolOptions {
            include_environment_id: false,
        },
        ExecCommandHandlerOptions {
            allow_login_shell: false,
            exec_permission_approvals_enabled: false,
            include_environment_id: false,
        },
    )
    .handle(invocation)
    .await
    .expect("bash should execute");

    assert_eq!(
        std::fs::read_to_string(temp_dir.path().join("bash-created.txt")).expect("read file"),
        "smoke"
    );
    let (begin, end) = bash_exec_events(&rx_event).await;
    assert_eq!(begin.tool_name.as_deref(), Some("Bash"));
    assert_eq!(end.tool_name.as_deref(), Some("Bash"));
}

#[tokio::test]
async fn bash_timeout_terminates_command_instead_of_only_yielding() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let start = std::time::Instant::now();
    let output = BashHandler::new(
        SimpleFileToolOptions {
            include_environment_id: false,
        },
        ExecCommandHandlerOptions {
            allow_login_shell: false,
            exec_permission_approvals_enabled: false,
            include_environment_id: false,
        },
    )
    .handle(
        invocation(
            "Bash",
            json!({
                "command": "sleep 2; printf late > timed-out.txt",
                "timeout": 50,
            }),
            temp_dir.path(),
        )
        .await,
    )
    .await
    .expect("bash should return timeout output");

    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert!(start.elapsed() < std::time::Duration::from_secs(1));
    assert!(!temp_dir.path().join("timed-out.txt").exists());
    let text = output_text(output.as_ref(), "call-Bash");
    assert!(text.contains("Command timed out"), "{text}");
}

#[tokio::test]
async fn bash_pre_tool_use_uses_bash_hook_contract() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let invocation = invocation(
        "Bash",
        json!({
            "command": "echo hi",
            "description": "say hi",
            "timeout": 50,
            "run_in_background": true,
        }),
        temp_dir.path(),
    )
    .await;
    let handler = BashHandler::new(
        SimpleFileToolOptions {
            include_environment_id: false,
        },
        ExecCommandHandlerOptions {
            allow_login_shell: false,
            exec_permission_approvals_enabled: false,
            include_environment_id: false,
        },
    );

    assert_eq!(
        handler.pre_tool_use_payload(&invocation),
        Some(PreToolUsePayload {
            tool_name: HookToolName::bash(),
            tool_input: json!({ "command": "echo hi" }),
        })
    );
}

#[test]
fn bash_args_accept_claude_sandbox_override_field() {
    let args: BashArgs = parse_arguments(
        &json!({
            "command": "id",
            "dangerouslyDisableSandbox": true,
            "timeout": "1000",
            "run_in_background": "true",
        })
        .to_string(),
    )
    .expect("args should parse");

    assert!(args.dangerously_disable_sandbox);
    assert_eq!(args.timeout, Some(1000));
    assert!(args.run_in_background);
}

#[test]
fn parse_pdf_pages_accepts_single_range_and_open_ended() {
    assert_eq!(
        parse_pdf_page_range("5").expect("single page"),
        PdfPageRange {
            first_page: 5,
            last_page: Some(5),
        }
    );
    assert_eq!(
        parse_pdf_page_range("1-10").expect("page range"),
        PdfPageRange {
            first_page: 1,
            last_page: Some(10),
        }
    );
    assert_eq!(
        parse_pdf_page_range("3-").expect("open-ended range"),
        PdfPageRange {
            first_page: 3,
            last_page: None,
        }
    );
}

#[test]
fn parse_pdf_pages_rejects_invalid_ranges() {
    for pages in ["", "0", "4-2", "x-y"] {
        let Err(FunctionCallError::RespondToModel(message)) = parse_pdf_page_range(pages) else {
            panic!("expected invalid pages error for {pages}");
        };
        assert!(message.contains("invalid PDF pages range"), "{message}");
    }
}

#[test]
fn validate_pdf_pages_rejects_ranges_outside_pdf() {
    let Err(FunctionCallError::RespondToModel(start_message)) = validate_pdf_page_range(
        &PdfPageRange {
            first_page: 4,
            last_page: Some(4),
        },
        3,
    ) else {
        panic!("expected start page error");
    };
    assert!(
        start_message.contains("starts at page 4"),
        "{start_message}"
    );

    let Err(FunctionCallError::RespondToModel(end_message)) = validate_pdf_page_range(
        &PdfPageRange {
            first_page: 2,
            last_page: Some(5),
        },
        3,
    ) else {
        panic!("expected end page error");
    };
    assert!(end_message.contains("ends at page 5"), "{end_message}");
}

#[tokio::test]
async fn poppler_command_reports_missing_program() {
    let result = run_poppler_command(
        "__codex_missing_poppler_test_binary__",
        std::iter::empty::<&str>(),
        Duration::from_secs(1),
    )
    .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected missing program error");
    };
    assert!(message.contains("is not installed"), "{message}");
}

#[tokio::test]
async fn poppler_command_reports_timeout() {
    let mut command = Command::new("sleep");
    command.arg("1");

    let result = run_command(command, "pdftotext", Duration::from_millis(1)).await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected timeout error");
    };
    assert!(message.contains("timed out"), "{message}");
}

#[test]
fn poppler_failure_message_maps_password_errors() {
    assert_eq!(
        poppler_failure_message("pdftotext", "Command Line Error: Incorrect password"),
        "PDF is password-protected. Please provide an unprotected version."
    );
}

#[test]
fn poppler_failure_message_maps_corrupt_pdf_errors() {
    for stderr in [
        "Syntax Error: Damaged xref table",
        "Syntax Error: corrupt object stream",
        "Syntax Error: Invalid trailer",
    ] {
        assert_eq!(
            poppler_failure_message("pdfinfo", stderr),
            "PDF file is corrupted or invalid."
        );
    }
}

#[test]
fn poppler_failure_message_includes_unclassified_stderr() {
    assert_eq!(
        poppler_failure_message("pdftotext", "unsupported feature\n"),
        "pdftotext failed: unsupported feature\n"
    );
}

fn minimal_pdf_bytes(text: &str) -> Vec<u8> {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('(', "\\(")
        .replace(')', "\\)");
    let stream = format!("BT /F1 24 Tf 72 720 Td ({escaped}) Tj ET");
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()),
    ];
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
    }
    let xref_offset = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

#[tokio::test]
async fn bash_hook_rewrite_updates_command() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let invocation = invocation(
        "Bash",
        json!({
            "command": "echo before",
            "description": "say before",
        }),
        temp_dir.path(),
    )
    .await;
    let handler = BashHandler::new(
        SimpleFileToolOptions {
            include_environment_id: false,
        },
        ExecCommandHandlerOptions {
            allow_login_shell: false,
            exec_permission_approvals_enabled: false,
            include_environment_id: false,
        },
    );

    let updated = handler
        .with_updated_hook_input(invocation, json!({ "command": "echo after" }))
        .expect("rewrite should succeed");
    let ToolPayload::Function { arguments } = updated.payload else {
        panic!("expected function payload");
    };

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&arguments).expect("json"),
        json!({
            "command": "echo after",
            "description": "say before",
        })
    );
}
