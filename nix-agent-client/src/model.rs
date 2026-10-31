//! What a frontend sees of a conversation with an agent. These types carry no
//! protocol detail, so a frontend never has to know the wire format.

use agent_client_protocol::schema::v1 as acp;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    Other,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// Something a tool call produced or wants to change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolContent {
    Text(String),
    /// A file the tool changes. `old` is `None` for a file that is being created.
    Diff {
        path: String,
        old: Option<String>,
        new: String,
    },
}

/// A tool the agent uses (reading a file, editing one, running a command).
/// Updates to it arrive as new snapshots with the same `id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub title: String,
    pub kind: ToolKind,
    pub status: ToolStatus,
    pub content: Vec<ToolContent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanEntry {
    pub text: String,
    pub status: PlanStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

impl OptionKind {
    pub fn allows(self) -> bool {
        matches!(self, OptionKind::AllowOnce | OptionKind::AllowAlways)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    pub kind: OptionKind,
}

/// The agent asks before doing something. Answer with [`crate::Handle::answer`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Permission {
    pub id: u64,
    pub tool: ToolCall,
    pub options: Vec<PermissionOption>,
}

/// A way of working the agent offers, e.g. "ask before editing" or "plan only".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mode {
    pub id: String,
    pub name: String,
    pub description: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}

impl StopReason {
    /// Why the answer ended, when that isn't just "it was done".
    pub fn explanation(self) -> Option<&'static str> {
        match self {
            StopReason::EndTurn => None,
            StopReason::MaxTokens => Some("The agent stopped because it reached its length limit."),
            StopReason::MaxTurnRequests => Some("The agent stopped because it reached its limit of steps."),
            StopReason::Refusal => Some("The agent declined to continue."),
            StopReason::Cancelled => Some("Stopped."),
        }
    }
}

/// Everything the agent side tells the frontend, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The agent is running and a session is open.
    Ready { agent: String, modes: Vec<Mode>, mode: Option<String> },
    /// A piece of the agent's answer, to be appended to what came before.
    Message(String),
    /// A piece of the agent's reasoning.
    Thought(String),
    /// A tool call began or changed.
    Tool(ToolCall),
    /// The agent's plan, replacing any earlier one.
    Plan(Vec<PlanEntry>),
    /// The agent waits for an answer before it goes on.
    Permission(Permission),
    /// The way of working changed.
    Mode(String),
    /// The agent finished answering.
    TurnEnded(StopReason),
    /// The agent could not answer; the session is still usable.
    TurnFailed(String),
    /// The agent could not be started, or went away. Nothing follows.
    Closed(Option<String>),
}

// ------------------------------------------------------------ conversion --

impl From<acp::ToolKind> for ToolKind {
    fn from(kind: acp::ToolKind) -> Self {
        match kind {
            acp::ToolKind::Read => ToolKind::Read,
            acp::ToolKind::Edit => ToolKind::Edit,
            acp::ToolKind::Delete => ToolKind::Delete,
            acp::ToolKind::Move => ToolKind::Move,
            acp::ToolKind::Search => ToolKind::Search,
            acp::ToolKind::Execute => ToolKind::Execute,
            acp::ToolKind::Think => ToolKind::Think,
            acp::ToolKind::Fetch => ToolKind::Fetch,
            acp::ToolKind::SwitchMode => ToolKind::SwitchMode,
            _ => ToolKind::Other,
        }
    }
}

impl From<acp::ToolCallStatus> for ToolStatus {
    fn from(status: acp::ToolCallStatus) -> Self {
        match status {
            acp::ToolCallStatus::Pending => ToolStatus::Pending,
            acp::ToolCallStatus::InProgress => ToolStatus::InProgress,
            acp::ToolCallStatus::Completed => ToolStatus::Completed,
            acp::ToolCallStatus::Failed => ToolStatus::Failed,
            _ => ToolStatus::Pending,
        }
    }
}

impl From<acp::StopReason> for StopReason {
    fn from(reason: acp::StopReason) -> Self {
        match reason {
            acp::StopReason::MaxTokens => StopReason::MaxTokens,
            acp::StopReason::MaxTurnRequests => StopReason::MaxTurnRequests,
            acp::StopReason::Refusal => StopReason::Refusal,
            acp::StopReason::Cancelled => StopReason::Cancelled,
            _ => StopReason::EndTurn,
        }
    }
}

impl From<acp::PermissionOptionKind> for OptionKind {
    fn from(kind: acp::PermissionOptionKind) -> Self {
        match kind {
            acp::PermissionOptionKind::AllowOnce => OptionKind::AllowOnce,
            acp::PermissionOptionKind::AllowAlways => OptionKind::AllowAlways,
            acp::PermissionOptionKind::RejectAlways => OptionKind::RejectAlways,
            // Anything unknown must not read as permission.
            _ => OptionKind::RejectOnce,
        }
    }
}

impl From<acp::PlanEntryStatus> for PlanStatus {
    fn from(status: acp::PlanEntryStatus) -> Self {
        match status {
            acp::PlanEntryStatus::InProgress => PlanStatus::InProgress,
            acp::PlanEntryStatus::Completed => PlanStatus::Completed,
            _ => PlanStatus::Pending,
        }
    }
}

fn content(items: Vec<acp::ToolCallContent>) -> Vec<ToolContent> {
    items
        .into_iter()
        .filter_map(|item| match item {
            acp::ToolCallContent::Content(c) => match c.content {
                acp::ContentBlock::Text(text) if !text.text.trim().is_empty() => Some(ToolContent::Text(text.text)),
                _ => None,
            },
            acp::ToolCallContent::Diff(diff) => Some(ToolContent::Diff {
                path: diff.path.display().to_string(),
                old: diff.old_text,
                new: diff.new_text,
            }),
            _ => None,
        })
        .collect()
}

impl From<acp::ToolCall> for ToolCall {
    fn from(call: acp::ToolCall) -> Self {
        Self {
            id: call.tool_call_id.0.to_string(),
            title: call.title,
            kind: call.kind.into(),
            status: call.status.into(),
            content: content(call.content),
        }
    }
}

impl ToolCall {
    /// A call known only from an update, until its first full description arrives.
    fn blank(id: String) -> Self {
        Self { id, title: String::new(), kind: ToolKind::Other, status: ToolStatus::Pending, content: Vec::new() }
    }

    /// Applies an update: only the fields it carries change.
    pub(crate) fn merge(&mut self, update: acp::ToolCallUpdate) {
        let fields = update.fields;
        if let Some(title) = fields.title {
            self.title = title;
        }
        if let Some(kind) = fields.kind {
            self.kind = kind.into();
        }
        if let Some(status) = fields.status {
            self.status = status.into();
        }
        if let Some(items) = fields.content {
            self.content = content(items);
        }
    }

    /// The call as an update describes it, on top of what was known before.
    pub(crate) fn updated(before: Option<&ToolCall>, update: acp::ToolCallUpdate) -> Self {
        let mut call = before.cloned().unwrap_or_else(|| Self::blank(update.tool_call_id.0.to_string()));
        call.merge(update);
        call
    }
}

// ------------------------------------------------------------------- diff --

/// A unified diff of what a tool call changes in a file, for showing before
/// the change is allowed. A new file is a diff against nothing.
pub fn unified_diff(old: Option<&str>, new: &str) -> String {
    similar::TextDiff::from_lines(old.unwrap_or(""), new).unified_diff().context_radius(3).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(id: &str, fields: acp::ToolCallUpdateFields) -> acp::ToolCallUpdate {
        acp::ToolCallUpdate::new(id.to_owned(), fields)
    }

    #[test]
    fn an_update_changes_only_what_it_carries() {
        let call: ToolCall = acp::ToolCall::new("t1".to_owned(), "Edit flake.nix").kind(acp::ToolKind::Edit).into();
        assert_eq!((call.status, call.kind), (ToolStatus::Pending, ToolKind::Edit));

        let done = ToolCall::updated(
            Some(&call),
            update("t1", acp::ToolCallUpdateFields::new().status(acp::ToolCallStatus::Completed)),
        );
        assert_eq!(done.title, "Edit flake.nix");
        assert_eq!(done.kind, ToolKind::Edit);
        assert_eq!(done.status, ToolStatus::Completed);
    }

    #[test]
    fn an_update_for_an_unknown_call_starts_one() {
        let call = ToolCall::updated(
            None,
            update("t9", acp::ToolCallUpdateFields::new().title("Run it").status(acp::ToolCallStatus::InProgress)),
        );
        assert_eq!((call.id.as_str(), call.title.as_str(), call.status), ("t9", "Run it", ToolStatus::InProgress));
    }

    #[test]
    fn diffs_and_text_are_kept_and_empty_text_is_not() {
        let call: ToolCall = acp::ToolCall::new("t".to_owned(), "x")
            .content(vec![
                acp::ToolCallContent::from("  "),
                acp::ToolCallContent::from("output"),
                acp::ToolCallContent::Diff(acp::Diff::new("/etc/nixos/a.nix", "new\n").old_text("old\n".to_owned())),
            ])
            .into();
        assert_eq!(call.content.len(), 2);
        assert_eq!(call.content[0], ToolContent::Text("output".to_owned()));
        assert!(matches!(&call.content[1], ToolContent::Diff { path, old: Some(_), .. } if path == "/etc/nixos/a.nix"));
    }

    #[test]
    fn an_unknown_permission_is_never_read_as_allowing() {
        assert!(OptionKind::from(acp::PermissionOptionKind::AllowOnce).allows());
        assert!(OptionKind::from(acp::PermissionOptionKind::AllowAlways).allows());
        assert!(!OptionKind::from(acp::PermissionOptionKind::RejectOnce).allows());
        assert!(!OptionKind::from(acp::PermissionOptionKind::RejectAlways).allows());
    }

    #[test]
    fn writes_a_unified_diff() {
        let diff = unified_diff(Some("a\nb\nc\n"), "a\nB\nc\n");
        assert!(diff.contains("@@ -1,3 +1,3 @@") && diff.contains("-b\n+B\n"), "{diff}");

        let created = unified_diff(None, "x\ny\n");
        assert!(created.contains("@@ -0,0 +1,2 @@") && created.contains("+x\n+y\n"), "{created}");
        assert_eq!(unified_diff(Some("same\n"), "same\n"), "");
    }
}
