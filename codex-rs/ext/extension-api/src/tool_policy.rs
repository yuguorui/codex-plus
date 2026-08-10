//! A startup ceiling on tool selection and construction, independent of session identity.
//! This policy only restricts tools; permission and approval checks still apply.

use crate::ToolName;

/// A host-side allowlist for commands submitted through shell tools.
///
/// Rules accept a small, deliberately strict subset of Claude's `Bash(...)`
/// syntax. `Bash` and `Bash(*)` allow every command. `Bash(cmd)` matches the
/// exact command or a command whose first token is `cmd`. `Bash(cmd:*)` and
/// `Bash(cmd *)` are prefix rules. Multiple rules are ORed.
///
/// The clamp is fail-closed: an empty rule list, an empty rule, or any malformed
/// rule permits no commands at all.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BashCommandClamp {
    rules: Vec<BashCommandRule>,
    malformed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum BashCommandRule {
    Any,
    ExactOrFirstToken(String),
    Prefix(String),
}

impl BashCommandClamp {
    /// Parses clamp rules. Any malformed rule puts the whole clamp in a
    /// fail-closed state instead of silently dropping the bad rule.
    pub fn from_rules<I, S>(rules: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut parsed = Vec::new();
        let mut malformed = false;
        for rule in rules {
            match BashCommandRule::parse(rule.as_ref()) {
                Some(rule) => parsed.push(rule),
                None => malformed = true,
            }
        }
        Self {
            rules: parsed,
            malformed,
        }
    }

    /// Returns whether the command is permitted. An absent clamp is handled by
    /// the caller; an empty or malformed clamp always denies.
    pub fn allows_command(&self, command: &str) -> bool {
        if self.malformed || self.rules.is_empty() {
            return false;
        }
        let command = command.trim();
        if command.is_empty() {
            return false;
        }
        // Non-wildcard clamps describe a single simple command. Reject shell
        // composition before matching so an allowed prefix cannot smuggle a
        // second command through `;`, `&&`, pipes, redirects, or substitution.
        if !self
            .rules
            .iter()
            .any(|rule| matches!(rule, BashCommandRule::Any))
            && contains_shell_metachar(command)
        {
            return false;
        }
        self.rules.iter().any(|rule| rule.matches(command))
    }
}

fn contains_shell_metachar(command: &str) -> bool {
    command
        .chars()
        .any(|ch| matches!(ch, ';' | '&' | '|' | '>' | '<' | '`' | '\n' | '\r'))
        || command.contains("$(")
        || command.contains("=(")
}

impl BashCommandRule {
    fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        let inner = if raw == "Bash" || raw == "*" {
            return Some(Self::Any);
        } else if let Some(body) = raw.strip_prefix("Bash(") {
            body.strip_suffix(')')?.trim()
        } else if raw.starts_with("Bash") {
            // `Bashfoo` and `Bash(...)extra` are malformed rather than literal commands.
            return None;
        } else {
            // Workflow also accepts bare command rules such as `git status`.
            raw
        };
        if inner.is_empty() {
            return None;
        }
        if inner == "*" {
            return Some(Self::Any);
        }
        if inner.chars().any(char::is_control) {
            return None;
        }
        if let Some(prefix) = inner.strip_suffix(":*") {
            let prefix = prefix.trim_end();
            if prefix.is_empty() || prefix.contains('*') {
                return None;
            }
            return Some(Self::Prefix(prefix.to_string()));
        }
        if let Some(prefix) = inner.strip_suffix(" *") {
            let prefix = prefix.trim_end();
            if prefix.is_empty() || prefix.contains('*') {
                return None;
            }
            return Some(Self::Prefix(prefix.to_string()));
        }
        if inner.contains('*') {
            return None;
        }
        Some(Self::ExactOrFirstToken(inner.to_string()))
    }

    fn matches(&self, command: &str) -> bool {
        match self {
            Self::Any => true,
            Self::ExactOrFirstToken(expected) => {
                command == expected
                    || (!expected.chars().any(char::is_whitespace)
                        && command.split_whitespace().next() == Some(expected.as_str()))
            }
            Self::Prefix(prefix) => {
                command == prefix
                    || command.strip_prefix(prefix).is_some_and(|rest| {
                        rest.chars()
                            .next()
                            .is_some_and(|ch| ch.is_whitespace() || ch == ':')
                    })
            }
        }
    }
}

/// Supply through `ExtensionDataInit` before starting a thread. The host captures
/// the policy once; later extension-state changes cannot relax it. Callers must
/// supply it again when resuming a thread.
#[derive(Clone, Debug)]
pub struct ToolPolicy {
    /// `None` keeps ordinary tool selection; an empty list permits no tools.
    /// Names include their namespace; plain names use the default namespace.
    /// Generated tools such as Code Mode's `exec` and `wait` must also be listed.
    pub allowed_tools: Option<Vec<ToolName>>,
    /// Exact tool names that are always unavailable, even when listed in
    /// `allowed_tools`. Plain names match the default namespace only.
    pub denied_tools: Option<Vec<ToolName>>,
    /// Exact namespaces whose tools are always unavailable.
    pub denied_tool_namespaces: Option<Vec<String>>,
    /// Deny every namespaced tool. Used for Claude's `mcp__*` wildcard when
    /// non-prefixed MCP namespaces make a textual prefix insufficient.
    pub deny_all_namespaces: bool,
    /// Host-side allowlist for commands submitted through shell tools. `None`
    /// leaves command selection unchanged; `Some` is fail-closed.
    pub bash_command_clamp: Option<BashCommandClamp>,
    /// Omit core tools unless the thread and every ready environment use a managed sandbox.
    pub require_managed_sandbox: bool,
    /// Omit shell tools when unified exec is disabled, instead of using one-shot exec.
    pub require_unified_exec: bool,
    /// Advertise additional-permission arguments when the feature is enabled.
    pub expose_additional_permissions: bool,
}

impl Default for ToolPolicy {
    fn default() -> Self {
        Self {
            allowed_tools: None,
            denied_tools: None,
            denied_tool_namespaces: None,
            deny_all_namespaces: false,
            bash_command_clamp: None,
            require_managed_sandbox: false,
            require_unified_exec: false,
            expose_additional_permissions: true,
        }
    }
}

impl ToolPolicy {
    pub fn allows(&self, tool: &ToolName) -> bool {
        if self
            .denied_tools
            .as_ref()
            .is_some_and(|tools| tools.iter().any(|denied| tool_names_match(denied, tool)))
        {
            return false;
        }
        if self.deny_all_namespaces && tool.namespace.is_some() {
            return false;
        }
        if self
            .denied_tool_namespaces
            .as_ref()
            .is_some_and(|namespaces| {
                tool.namespace
                    .as_deref()
                    .is_some_and(|namespace| namespaces.iter().any(|denied| denied == namespace))
            })
        {
            return false;
        }
        self.allowed_tools
            .as_ref()
            .is_none_or(|tools| tools.iter().any(|allowed| tool_names_match(allowed, tool)))
    }

    /// Returns whether the active policy has a Bash command clamp. Callers use
    /// this to force a non-login default shell so shell startup files cannot
    /// reintroduce commands that the clamp rejected.
    pub fn has_bash_command_clamp(&self) -> bool {
        self.bash_command_clamp.is_some()
    }

    /// Returns whether the command may run. `None` means the thread has no
    /// command clamp; `Some` delegates to the fail-closed clamp.
    pub fn allows_bash_command(&self, command: &str) -> bool {
        self.bash_command_clamp
            .as_ref()
            .is_none_or(|clamp| clamp.allows_command(command))
    }
}

fn tool_names_match(lhs: &ToolName, rhs: &ToolName) -> bool {
    lhs.name == rhs.name
        && (lhs.namespace == rhs.namespace
            || (lhs.is_default_namespace() && rhs.is_default_namespace()))
}
