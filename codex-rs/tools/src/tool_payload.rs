use std::borrow::Cow;

use codex_protocol::models::SearchToolCallParams;

/// Wire shape that delivered a `tool_search` call.
///
/// Outputs must mirror the shape the call arrived in: native transports pair a
/// `tool_search_call` with a `tool_search_output`, while function-backed
/// transports declared `tool_search` as a plain function and require a
/// matching `function_call_output`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolSearchTransport {
    /// The provider emitted a native `tool_search_call` output item.
    Native,
    /// The provider called `tool_search` as a regular function.
    Function,
}

/// Canonical payload shapes accepted by model-visible tool runtimes.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolPayload {
    Function {
        arguments: String,
    },
    ToolSearch {
        arguments: SearchToolCallParams,
        transport: ToolSearchTransport,
    },
    Custom {
        input: String,
    },
}

impl ToolPayload {
    pub fn log_payload(&self) -> Cow<'_, str> {
        match self {
            ToolPayload::Function { arguments } => Cow::Borrowed(arguments),
            ToolPayload::ToolSearch { arguments, .. } => Cow::Owned(arguments.query.clone()),
            ToolPayload::Custom { input } => Cow::Borrowed(input),
        }
    }
}
