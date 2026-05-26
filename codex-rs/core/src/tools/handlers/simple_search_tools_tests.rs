use super::*;
use crate::session::tests::make_session_and_context;
use crate::session::tests::make_session_and_context_with_dynamic_tools_and_rx;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolOutput;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_protocol::items::McpToolCallItem;
use codex_protocol::items::McpToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::EventMsg;
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
    ToolInvocation {
        session: Arc::new(session),
        turn: Arc::new(turn),
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: format!("call-{tool_name}"),
        tool_name: ToolName::plain(tool_name),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: arguments.to_string(),
        },
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

async fn completed_mcp_tool_call(
    rx_event: &async_channel::Receiver<codex_protocol::protocol::Event>,
) -> McpToolCallItem {
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx_event.recv())
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

#[tokio::test]
async fn glob_handler_finds_files_under_path() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(temp_dir.path().join("src/nested")).expect("mkdir");
    std::fs::write(temp_dir.path().join("src/lib.rs"), "").expect("write file");
    std::fs::write(temp_dir.path().join("src/nested/mod.rs"), "").expect("write file");
    std::fs::write(temp_dir.path().join("src/readme.md"), "").expect("write file");

    let output = GlobHandler::new(SimpleSearchToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Glob",
            json!({ "pattern": "**/*.rs", "path": "src" }),
            temp_dir.path(),
        )
        .await,
    )
    .await
    .expect("glob should succeed");

    let text = output_text(output.as_ref(), "call-Glob");
    assert!(text.contains("src/lib.rs"), "{text}");
    assert!(text.contains("src/nested/mod.rs"), "{text}");
    assert!(!text.contains("readme.md"), "{text}");
}

#[tokio::test]
async fn glob_handler_emits_codex_tool_call_item_for_ui() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("lib.rs"), "").expect("write file");
    let (invocation, rx_event) =
        invocation_with_events("Glob", json!({ "pattern": "*.rs" }), temp_dir.path()).await;

    GlobHandler::new(SimpleSearchToolOptions {
        include_environment_id: false,
    })
    .handle(invocation)
    .await
    .expect("glob should succeed");

    let item = completed_mcp_tool_call(&rx_event).await;
    assert_eq!(item.server, "codex++");
    assert_eq!(item.tool, "Glob");
    assert_eq!(item.status, McpToolCallStatus::Completed);
    assert_eq!(
        item.result,
        Some(CallToolResult {
            content: Vec::new(),
            structured_content: None,
            is_error: Some(false),
            meta: None,
        })
    );
}

#[tokio::test]
async fn glob_invalid_pattern_completes_ui_item_as_failed() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (invocation, rx_event) = invocation_with_events(
        "Glob",
        json!({ "pattern": "*.rs", "environment_id": "missing" }),
        temp_dir.path(),
    )
    .await;

    let result = GlobHandler::new(SimpleSearchToolOptions {
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
            .is_some_and(|error| error.message.contains("unknown turn environment id")),
        "{item:?}"
    );
}

#[tokio::test]
async fn glob_handler_reports_no_files_for_empty_result() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("readme.md"), "").expect("write file");

    let output = GlobHandler::new(SimpleSearchToolOptions {
        include_environment_id: false,
    })
    .handle(invocation("Glob", json!({ "pattern": "*.rs" }), temp_dir.path()).await)
    .await
    .expect("glob should succeed");

    assert_eq!(output_text(output.as_ref(), "call-Glob"), "No files found");
}

#[tokio::test]
async fn glob_handler_supports_question_mark_and_literal_dots() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(temp_dir.path().join("src")).expect("mkdir");
    for file_name in ["file1.rs", "fileA.rs", "file10.rs", "file1xrs"] {
        std::fs::write(temp_dir.path().join("src").join(file_name), "").expect("write file");
    }

    let output = GlobHandler::new(SimpleSearchToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Glob",
            json!({ "pattern": "file?.rs", "path": "src" }),
            temp_dir.path(),
        )
        .await,
    )
    .await
    .expect("glob should succeed");

    assert_eq!(
        output_text(output.as_ref(), "call-Glob"),
        "src/file1.rs\nsrc/fileA.rs"
    );
}

#[tokio::test]
async fn glob_handler_caps_results_with_note() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    for index in 0..=MAX_GLOB_RESULTS {
        std::fs::write(temp_dir.path().join(format!("file-{index:03}.rs")), "")
            .expect("write file");
    }

    let output = GlobHandler::new(SimpleSearchToolOptions {
        include_environment_id: false,
    })
    .handle(invocation("Glob", json!({ "pattern": "*.rs" }), temp_dir.path()).await)
    .await
    .expect("glob should succeed");

    let text = output_text(output.as_ref(), "call-Glob");
    assert_eq!(
        text.lines().take(MAX_GLOB_RESULTS).count(),
        MAX_GLOB_RESULTS
    );
    assert!(
        text.contains("Results truncated to 100 of 101 files."),
        "{text}"
    );
}

#[tokio::test]
async fn grep_handler_returns_files_with_matches_by_default() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("one.txt"), "needle\n").expect("write file");
    std::fs::write(temp_dir.path().join("two.txt"), "hay\n").expect("write file");

    let output = GrepHandler::new(SimpleSearchToolOptions {
        include_environment_id: false,
    })
    .handle(invocation("Grep", json!({ "pattern": "needle" }), temp_dir.path()).await)
    .await
    .expect("grep should succeed");

    let text = output_text(output.as_ref(), "call-Grep");
    assert!(text.contains("one.txt"), "{text}");
    assert!(!text.contains("two.txt"), "{text}");
}

#[tokio::test]
async fn grep_handler_emits_codex_tool_call_item_for_ui() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("one.txt"), "needle\n").expect("write file");
    let (invocation, rx_event) =
        invocation_with_events("Grep", json!({ "pattern": "needle" }), temp_dir.path()).await;

    GrepHandler::new(SimpleSearchToolOptions {
        include_environment_id: false,
    })
    .handle(invocation)
    .await
    .expect("grep should succeed");

    let item = completed_mcp_tool_call(&rx_event).await;
    assert_eq!(item.server, "codex++");
    assert_eq!(item.tool, "Grep");
    assert_eq!(item.status, McpToolCallStatus::Completed);
    assert_eq!(
        item.result,
        Some(CallToolResult {
            content: Vec::new(),
            structured_content: None,
            is_error: Some(false),
            meta: None,
        })
    );
}

#[tokio::test]
async fn grep_handler_searches_a_direct_file_path() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("one.txt"), "needle\n").expect("write file");
    std::fs::write(temp_dir.path().join("two.txt"), "needle\n").expect("write file");

    let output = GrepHandler::new(SimpleSearchToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Grep",
            json!({ "pattern": "needle", "path": "one.txt" }),
            temp_dir.path(),
        )
        .await,
    )
    .await
    .expect("grep should succeed");

    assert_eq!(output_text(output.as_ref(), "call-Grep"), "one.txt");
}

#[tokio::test]
async fn grep_handler_applies_glob_and_type_filters() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(temp_dir.path().join("src")).expect("mkdir");
    std::fs::write(temp_dir.path().join("src/lib.rs"), "needle\n").expect("write file");
    std::fs::write(temp_dir.path().join("src/lib.py"), "needle\n").expect("write file");
    std::fs::write(temp_dir.path().join("src/test.rs"), "needle\n").expect("write file");

    let output = GrepHandler::new(SimpleSearchToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Grep",
            json!({
                "pattern": "needle",
                "path": "src",
                "glob": "lib.*",
                "type": "rust",
            }),
            temp_dir.path(),
        )
        .await,
    )
    .await
    .expect("grep should succeed");

    let text = output_text(output.as_ref(), "call-Grep");
    assert!(text.contains("src/lib.rs"), "{text}");
    assert!(!text.contains("src/lib.py"), "{text}");
    assert!(!text.contains("src/test.rs"), "{text}");
}

#[tokio::test]
async fn grep_handler_supports_content_count_and_multiline() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("one.txt"), "alpha\nbeta\nalpha beta\n")
        .expect("write file");

    let content = GrepHandler::new(SimpleSearchToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Grep",
            json!({
                "pattern": "alpha",
                "output_mode": "content",
            }),
            temp_dir.path(),
        )
        .await,
    )
    .await
    .expect("grep content should succeed");
    assert!(
        output_text(content.as_ref(), "call-Grep").contains("one.txt:1:alpha"),
        "content output should include line"
    );

    let count = GrepHandler::new(SimpleSearchToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Grep",
            json!({
                "pattern": "alpha",
                "output_mode": "count",
            }),
            temp_dir.path(),
        )
        .await,
    )
    .await
    .expect("grep count should succeed");
    assert_eq!(output_text(count.as_ref(), "call-Grep"), "one.txt:2");

    let multiline = GrepHandler::new(SimpleSearchToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Grep",
            json!({
                "pattern": "alpha\\nbeta",
                "output_mode": "content",
                "multiline": true,
            }),
            temp_dir.path(),
        )
        .await,
    )
    .await
    .expect("grep multiline should succeed");
    assert!(
        output_text(multiline.as_ref(), "call-Grep").contains("one.txt:1:alpha\\nbeta"),
        "multiline output should include compact match"
    );
}

#[tokio::test]
async fn grep_handler_supports_case_insensitive_search() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("one.txt"), "Needle\n").expect("write file");

    let output = GrepHandler::new(SimpleSearchToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Grep",
            json!({
                "pattern": "needle",
                "output_mode": "content",
                "-i": true,
            }),
            temp_dir.path(),
        )
        .await,
    )
    .await
    .expect("grep should succeed");

    assert_eq!(
        output_text(output.as_ref(), "call-Grep"),
        "one.txt:1:Needle"
    );
}

#[tokio::test]
async fn grep_handler_truncates_content_with_continuation_offset() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        temp_dir.path().join("one.txt"),
        "needle 1\nneedle 2\nneedle 3\n",
    )
    .expect("write file");

    let output = GrepHandler::new(SimpleSearchToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Grep",
            json!({
                "pattern": "needle",
                "output_mode": "content",
                "head_limit": 2,
            }),
            temp_dir.path(),
        )
        .await,
    )
    .await
    .expect("grep should succeed");

    assert_eq!(
        output_text(output.as_ref(), "call-Grep"),
        "one.txt:1:needle 1\none.txt:2:needle 2\n\nResults truncated. Use offset=2 to continue."
    );
}

#[tokio::test]
async fn grep_handler_allows_unlimited_results_with_zero_head_limit() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        temp_dir.path().join("one.txt"),
        "needle 1\nneedle 2\nneedle 3\n",
    )
    .expect("write file");

    let output = GrepHandler::new(SimpleSearchToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Grep",
            json!({
                "pattern": "needle",
                "output_mode": "content",
                "head_limit": 0,
            }),
            temp_dir.path(),
        )
        .await,
    )
    .await
    .expect("grep should succeed");

    assert_eq!(
        output_text(output.as_ref(), "call-Grep"),
        "one.txt:1:needle 1\none.txt:2:needle 2\none.txt:3:needle 3"
    );
}

#[tokio::test]
async fn grep_handler_accepts_claude_context_pagination_and_flags() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        temp_dir.path().join("one.txt"),
        "before\nNeedle\nmiddle\nneedle\nlast\n",
    )
    .expect("write file");

    let output = GrepHandler::new(SimpleSearchToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Grep",
            json!({
                "pattern": "needle",
                "output_mode": "content",
                "-i": true,
                "-n": false,
                "context": 1,
                "head_limit": 2,
                "offset": 1,
            }),
            temp_dir.path(),
        )
        .await,
    )
    .await
    .expect("grep should succeed");

    assert_eq!(
        output_text(output.as_ref(), "call-Grep"),
        "one.txt:Needle\none.txt:middle\n\nResults truncated. Use offset=3 to continue."
    );
}

#[tokio::test]
async fn grep_context_long_overrides_short_before_after_flags() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        temp_dir.path().join("one.txt"),
        "first\nbefore\nneedle\nafter\nlast\n",
    )
    .expect("write file");

    let output = GrepHandler::new(SimpleSearchToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Grep",
            json!({
                "pattern": "needle",
                "output_mode": "content",
                "-B": 0,
                "-A": 0,
                "-C": 0,
                "context": 1,
            }),
            temp_dir.path(),
        )
        .await,
    )
    .await
    .expect("grep should succeed");

    assert_eq!(
        output_text(output.as_ref(), "call-Grep"),
        "one.txt:2:before\none.txt:3:needle\none.txt:4:after"
    );
}

#[tokio::test]
async fn grep_handler_excludes_vcs_directories() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(temp_dir.path().join(".git")).expect("mkdir");
    std::fs::write(temp_dir.path().join(".git/config"), "needle\n").expect("write file");
    std::fs::write(temp_dir.path().join("visible.txt"), "needle\n").expect("write file");

    let output = GrepHandler::new(SimpleSearchToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Grep",
            json!({ "pattern": "needle", "output_mode": "files_with_matches" }),
            temp_dir.path(),
        )
        .await,
    )
    .await
    .expect("grep should succeed");

    assert_eq!(output_text(output.as_ref(), "call-Grep"), "visible.txt");
}

#[tokio::test]
async fn grep_handler_rejects_invalid_regex() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("one.txt"), "needle\n").expect("write file");

    let result = GrepHandler::new(SimpleSearchToolOptions {
        include_environment_id: false,
    })
    .handle(invocation("Grep", json!({ "pattern": "[" }), temp_dir.path()).await)
    .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected invalid regex error");
    };
    assert!(message.contains("invalid grep pattern"), "{message}");
}

#[tokio::test]
async fn grep_invalid_regex_completes_ui_item_as_failed() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("one.txt"), "needle\n").expect("write file");
    let (invocation, rx_event) =
        invocation_with_events("Grep", json!({ "pattern": "[" }), temp_dir.path()).await;

    let result = GrepHandler::new(SimpleSearchToolOptions {
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
            .is_some_and(|error| error.message.contains("invalid grep pattern")),
        "{item:?}"
    );
}

#[tokio::test]
async fn grep_handler_ignores_non_utf8_files() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("binary.dat"), [0xff, 0xfe]).expect("write file");
    std::fs::write(temp_dir.path().join("text.txt"), "needle\n").expect("write file");

    let output = GrepHandler::new(SimpleSearchToolOptions {
        include_environment_id: false,
    })
    .handle(invocation("Grep", json!({ "pattern": "needle" }), temp_dir.path()).await)
    .await
    .expect("grep should succeed");

    assert_eq!(output_text(output.as_ref(), "call-Grep"), "text.txt");
}

#[tokio::test]
async fn grep_handler_reports_no_matches_for_content_and_count_modes() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("one.txt"), "hay\n").expect("write file");

    for output_mode in ["content", "count"] {
        let output = GrepHandler::new(SimpleSearchToolOptions {
            include_environment_id: false,
        })
        .handle(
            invocation(
                "Grep",
                json!({ "pattern": "needle", "output_mode": output_mode }),
                temp_dir.path(),
            )
            .await,
        )
        .await
        .expect("grep should succeed");

        assert_eq!(
            output_text(output.as_ref(), "call-Grep"),
            "No matches found"
        );
    }
}

#[tokio::test]
async fn grep_handler_maps_known_and_literal_type_filters() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("component.tsx"), "needle\n").expect("write file");
    std::fs::write(temp_dir.path().join("component.ts"), "needle\n").expect("write file");
    std::fs::write(temp_dir.path().join("component.rs"), "needle\n").expect("write file");

    let typescript = GrepHandler::new(SimpleSearchToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Grep",
            json!({ "pattern": "needle", "type": "typescript" }),
            temp_dir.path(),
        )
        .await,
    )
    .await
    .expect("grep should succeed");
    assert_eq!(
        output_text(typescript.as_ref(), "call-Grep"),
        "component.ts\ncomponent.tsx"
    );

    let literal_extension = GrepHandler::new(SimpleSearchToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Grep",
            json!({ "pattern": "needle", "type": ".tsx" }),
            temp_dir.path(),
        )
        .await,
    )
    .await
    .expect("grep should succeed");
    assert_eq!(
        output_text(literal_extension.as_ref(), "call-Grep"),
        "component.tsx"
    );
}

#[tokio::test]
async fn grep_handler_accepts_claude_semantic_booleans_and_numbers() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp_dir.path().join("one.txt"), "before\nNeedle\nafter\n").expect("write file");

    let output = GrepHandler::new(SimpleSearchToolOptions {
        include_environment_id: false,
    })
    .handle(
        invocation(
            "Grep",
            json!({
                "pattern": "needle",
                "output_mode": "content",
                "-i": "true",
                "-n": "false",
                "-A": "1",
                "head_limit": "2",
                "offset": "0",
                "multiline": "false",
            }),
            temp_dir.path(),
        )
        .await,
    )
    .await
    .expect("grep should succeed");

    assert_eq!(
        output_text(output.as_ref(), "call-Grep"),
        "one.txt:Needle\none.txt:after"
    );
}
