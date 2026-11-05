//! Serializable capability overrides for custom Responses-compatible providers.
//!
//! Unspecified capabilities inherit the provider's existing defaults. Model and
//! turn settings may further restrict the capabilities declared here.

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

/// Overrides for the optional API features supported by a custom provider.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ModelProviderCapabilities {
    /// Whether the provider accepts explicit `access_programs.cyber` selections.
    /// Defaults to enabled for the built-in OpenAI provider and disabled otherwise.
    /// This does not grant program access or change authentication.
    pub cyber_access_programs: Option<bool>,
    /// Whether hosted web search may access the live web.
    pub external_web_access: Option<bool>,
    /// Whether the Responses endpoint accepts arbitrary OpenAI `custom` tools.
    ///
    /// DeepSeek only accepts `custom` for `apply_patch`; Code Mode's `exec`
    /// therefore has to travel as a function tool when this is `false`.
    pub supports_custom_exec_tool: Option<bool>,
    /// Whether the Responses endpoint natively supports the `tool_search` tool
    /// type and `tool_search_call` output items.
    ///
    /// Providers whose gateway accepts the request but never exposes the tool
    /// to the model (such as BigModel) must set this to `false`; Codex then
    /// transports `tool_search` as a plain function tool and re-declares the
    /// discovered tools itself. Only the full Responses tools path consults
    /// this capability; Responses Lite and chat-wire transports are unchanged.
    pub supports_native_tool_search: Option<bool>,
    /// Remote context-compaction protocol; omission preserves provider defaults.
    pub remote_compaction: Option<RemoteCompactionSupport>,
}

/// Remote context-compaction protocols supported by a model provider.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteCompactionSupport {
    /// Use local compaction instead of a provider compaction protocol.
    Unsupported,
    /// Send `compaction_trigger` items over the Responses endpoint.
    V2,
}
