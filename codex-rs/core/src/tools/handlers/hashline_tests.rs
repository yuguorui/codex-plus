use super::*;

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
fn qualified_anchor_relocates_to_unique_hash_match() {
    let doc = Document::parse("new\nalpha\nbeta\ngamma\n");
    let anchor = format!("2:{}", format_short_hash(short_hash("beta")));

    let resolved = resolve_anchor(&doc, parse_anchor(&anchor).unwrap()).unwrap();

    assert_eq!(resolved.index, 2);
    assert!(resolved.relocated);
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
