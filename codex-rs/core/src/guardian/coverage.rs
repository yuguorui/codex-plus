//! Supplies Guardian categories for host-owned approval actions.

use crate::tools::sandboxing::ApprovalAction;
use codex_protocol::openai_models::GuardianScope;

impl ApprovalAction {
    pub(crate) fn guardian_scope(&self) -> GuardianScope {
        match self {
            Self::ExecCommand { .. } | Self::WriteStdin { .. } => GuardianScope::Shell,
            Self::ApplyPatch { .. } => GuardianScope::FileChanges,
            Self::McpToolCall {
                server,
                connector_id,
                ..
            } => GuardianScope::for_mcp_connector(server, connector_id.as_deref()),
            // Extension tools have no dedicated policy scope; treat them like the
            // other third-party tool approvals the host reviews on the user's behalf.
            Self::ExtensionTool { .. } => GuardianScope::Mcp,
            Self::NetworkAccess { .. } => GuardianScope::Network,
            Self::RequestPermissions { .. } => GuardianScope::Permissions,
        }
    }
}
