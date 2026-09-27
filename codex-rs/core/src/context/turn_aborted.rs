use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TurnAborted {
    pub(crate) guidance: String,
}

impl TurnAborted {
    pub(crate) const INTERRUPTED_GUIDANCE: &'static str = "The user interrupted the previous turn on purpose. This means the agent stopped waiting for pending results; underlying work may be running, completed, failed, or partially applied. Do not take further action based solely on the interruption; identify the exact operation and non-destructively verify its outcome or current state first.";
    pub(crate) const INTERRUPTED_DEVELOPER_GUIDANCE: &'static str = "The previous turn was interrupted on purpose. This means the agent stopped waiting for pending results; underlying work may be running, completed, failed, or partially applied. Do not take further action based solely on the interruption; identify the exact operation and non-destructively verify its outcome or current state first.";

    pub(crate) fn new(guidance: impl Into<String>) -> Self {
        Self {
            guidance: guidance.into(),
        }
    }
}

impl ContextualUserFragment for TurnAborted {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("generic.turn_aborted".to_string())
    }

    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<turn_aborted>", "</turn_aborted>")
    }

    fn body(&self) -> String {
        format!("\n{}\n", self.guidance)
    }
}
