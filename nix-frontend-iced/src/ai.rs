//! The assistant page: ask an AI agent about the configuration, or ask it to
//! change it. The agent is any program that speaks the Agent Client Protocol;
//! `nix-agent-client` runs it and this module shows the conversation.
//!
//! Nothing the agent wants to do is allowed without the user's say-so, and its
//! changes land in the configuration's files, where the git page shows them
//! before anything is committed.

use std::sync::Mutex;

use iced::keyboard::{key::Named, Key};
use iced::widget::text::Wrapping;
use iced::widget::{button, column, container, markdown, row, scrollable, space, text, text_editor, tooltip, Text};
use iced::{Alignment, Background, Border, Color, Element, Font, Length};
use iced_fonts::lucide;
use nix_agent_client::{
    unified_diff, Config, Event, Handle, Mode, OptionKind, Permission, PlanEntry, PlanStatus, ToolCall, ToolContent,
    ToolKind, ToolStatus,
};
use nix_backend_client::diff::{self, Line};

use crate::git::diff_row;
use crate::store::{card_with, failure};
use crate::theme::{self, ACCENT, ACCENT_ALT, DANGER, MUTED, SUCCESS, TEAL, WARNING};
use crate::view::{clipped, copy_button, ghost_button, icon_tile, BOLD};
use crate::{App, Conn, CopyTarget, Message};

const TRANSCRIPT_HEIGHT: f32 = 540.0;
const INPUT_HEIGHT: f32 = 84.0;
/// Longest diff or tool output shown inside a tool call, in lines.
const MAX_SHOWN_LINES: usize = 120;

/// Things worth asking, for an empty conversation.
const SUGGESTIONS: [&str; 4] = [
    "Explain how this configuration is organised.",
    "Which services are enabled on this machine?",
    "Is there anything outdated or redundant in here?",
    "Add a dev shell with git and ripgrep to my packages.",
];

/// What the agent's side of the conversation is up to.
pub enum Phase {
    /// Not started; it starts when the page is opened.
    Idle,
    Starting,
    Ready,
    /// Could not be started, or went away.
    Failed(String),
}

/// What a tool call shows.
pub enum Shown {
    Text(String),
    Diff { path: String, lines: Vec<Line>, added: usize, removed: usize },
}

/// A tool call as it is drawn.
pub struct ToolView {
    pub call: ToolCall,
    pub shown: Vec<Shown>,
}

impl ToolView {
    fn new(call: ToolCall) -> Self {
        let shown = call
            .content
            .iter()
            .map(|content| match content {
                ToolContent::Text(text) => Shown::Text(text.clone()),
                ToolContent::Diff { path, old, new } => {
                    let lines = diff::parse(&unified_diff(old.as_deref(), new));
                    let (added, removed) = diff::counts(&lines);
                    Shown::Diff { path: path.clone(), lines, added, removed }
                }
            })
            .collect();
        Self { call, shown }
    }

    /// Whether this call changes the configuration.
    fn changes_files(&self) -> bool {
        matches!(self.call.kind, ToolKind::Edit | ToolKind::Delete | ToolKind::Move)
    }
}

pub enum Entry {
    User(String),
    /// The agent's answer; it grows while the agent writes.
    Agent {
        text: String,
        rendered: markdown::Content,
    },
    Thought(String),
    Tool(ToolView),
    Note {
        text: String,
        error: bool,
    },
    /// The agent changed files; offers to look at them.
    Review,
}

pub struct Permit {
    pub id: u64,
    pub tool: ToolView,
    pub options: Vec<nix_agent_client::PermissionOption>,
}

/// Events from the agent that are waiting to be shown. The agent can write
/// faster than the window wants to redraw, so they are taken in batches:
/// only the push that finds the list empty needs to wake the UI.
#[derive(Default)]
pub struct Mailbox(Mutex<Vec<Event>>);

impl Mailbox {
    /// Whether the UI has to be woken for this.
    pub fn push(&self, event: Event) -> bool {
        let mut queue = self.0.lock().unwrap_or_else(|e| e.into_inner());
        queue.push(event);
        queue.len() == 1
    }

    pub fn take(&self) -> Vec<Event> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

pub struct Ai {
    pub phase: Phase,
    /// Counts sessions, so that what a finished one still sends is recognised and dropped.
    pub session: u64,
    pub mailbox: std::sync::Arc<Mailbox>,
    handle: Option<Handle>,
    pub agent: String,
    pub modes: Vec<Mode>,
    pub mode: Option<String>,
    pub entries: Vec<Entry>,
    pub plan: Vec<PlanEntry>,
    pub permits: Vec<Permit>,
    pub input: text_editor::Content,
    /// The agent is working on an answer.
    pub busy: bool,
    /// The agent changed files since the last question.
    changed_files: bool,
}

impl Ai {
    pub fn new() -> Self {
        Self {
            phase: Phase::Idle,
            session: 0,
            mailbox: Default::default(),
            handle: None,
            agent: String::new(),
            modes: Vec::new(),
            mode: None,
            entries: Vec::new(),
            plan: Vec::new(),
            permits: Vec::new(),
            input: text_editor::Content::new(),
            busy: false,
            changed_files: false,
        }
    }

    /// Ends the conversation and the agent with it. What was typed stays.
    pub fn stop(&mut self) {
        let input = std::mem::replace(&mut self.input, text_editor::Content::new());
        let session = self.session + 1;
        *self = Self { session, input, ..Self::new() };
    }

    /// Gets ready for a new session: forgets the old conversation, and returns
    /// what the events of the new one are told apart by.
    pub fn begin(&mut self, handle: Handle) -> (u64, std::sync::Arc<Mailbox>) {
        self.stop();
        self.phase = Phase::Starting;
        self.handle = Some(handle);
        self.mailbox = Default::default();
        (self.session, self.mailbox.clone())
    }

    pub fn fail(&mut self, reason: String) {
        self.phase = Phase::Failed(reason);
    }

    /// Something is on its way: the agent starting, or an answer being written.
    pub fn working(&self) -> bool {
        matches!(self.phase, Phase::Starting) || self.busy
    }

    pub fn can_send(&self) -> bool {
        matches!(self.phase, Phase::Ready)
            && !self.busy
            && self.handle.is_some()
            && !self.input.text().trim().is_empty()
    }

    /// Sends what is typed, or `question` if there is one.
    pub fn ask(&mut self, question: Option<String>) -> bool {
        let text = question.unwrap_or_else(|| self.input.text().trim().to_owned());
        let (Phase::Ready, false, Some(handle)) = (&self.phase, self.busy, &self.handle) else {
            return false;
        };
        if text.trim().is_empty() {
            return false;
        }
        handle.prompt(text.clone());
        self.entries.push(Entry::User(text));
        self.input = text_editor::Content::new();
        self.plan.clear();
        self.busy = true;
        self.changed_files = false;
        true
    }

    pub fn cancel(&mut self) {
        if let Some(handle) = &self.handle {
            handle.cancel();
        }
        // The agent is told to refuse what it asked; there is nothing left to answer.
        self.permits.clear();
    }

    pub fn answer(&mut self, id: u64, option: Option<String>) {
        if let Some(handle) = &self.handle {
            handle.answer(id, option);
        }
        self.permits.retain(|permit| permit.id != id);
    }

    pub fn set_mode(&mut self, mode: String) {
        if let Some(handle) = &self.handle {
            handle.set_mode(mode);
        }
    }

    /// Takes in what the agent sent.
    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Ready { agent, modes, mode } => {
                self.phase = Phase::Ready;
                self.agent = agent;
                self.modes = modes;
                self.mode = mode;
            }
            Event::Message(chunk) => match self.entries.last_mut() {
                Some(Entry::Agent { text, rendered }) => {
                    text.push_str(&chunk);
                    rendered.push_str(&chunk);
                }
                _ => self.entries.push(Entry::Agent { rendered: markdown::Content::parse(&chunk), text: chunk }),
            },
            Event::Thought(chunk) => match self.entries.last_mut() {
                Some(Entry::Thought(text)) => text.push_str(&chunk),
                _ => self.entries.push(Entry::Thought(chunk)),
            },
            Event::Tool(call) => {
                let view = ToolView::new(call);
                self.changed_files |= view.changes_files() && view.call.status == ToolStatus::Completed;
                let existing =
                    self.entries.iter_mut().rev().find_map(|entry| {
                        matches!(entry, Entry::Tool(t) if t.call.id == view.call.id).then_some(entry)
                    });
                match existing {
                    Some(entry) => *entry = Entry::Tool(view),
                    None => self.entries.push(Entry::Tool(view)),
                }
            }
            Event::Plan(plan) => self.plan = plan,
            Event::Permission(Permission { id, tool, options }) => {
                self.permits.push(Permit { id, tool: ToolView::new(tool), options });
            }
            Event::Mode(mode) => self.mode = Some(mode),
            Event::TurnEnded(reason) => {
                self.finish_turn();
                if let Some(explanation) = reason.explanation() {
                    self.entries.push(Entry::Note { text: explanation.to_owned(), error: false });
                }
            }
            Event::TurnFailed(error) => {
                self.finish_turn();
                self.entries.push(Entry::Note { text: error, error: true });
            }
            Event::Closed(reason) => {
                self.finish_turn();
                self.handle = None;
                self.phase = Phase::Failed(reason.unwrap_or_else(|| "The agent exited.".to_owned()));
            }
        }
    }

    fn finish_turn(&mut self) {
        self.busy = false;
        self.permits.clear();
        if std::mem::take(&mut self.changed_files) {
            self.entries.push(Entry::Review);
        }
    }

    /// The text of the agent's answer number `index` among the entries.
    pub fn answer_text(&self, index: usize) -> Option<&str> {
        match self.entries.get(index)? {
            Entry::Agent { text, .. } => Some(text),
            _ => None,
        }
    }
}

/// What to start the agent with.
pub fn config(app: &App) -> Result<Config, String> {
    let dir = app.effective_config_dir();
    let cwd = std::path::PathBuf::from(dir.strip_prefix("path:").unwrap_or(dir));
    if !cwd.is_dir() {
        return Err(format!("{dir} is not a directory on this machine, so the agent has nothing to work on."));
    }
    Ok(Config { command: app.effective_agent().to_owned(), cwd, host: app.effective_host_name().to_owned() })
}

// ------------------------------------------------------------------ views --

pub fn page(app: &App) -> Element<'_, Message> {
    let ai = &app.ai;
    match &ai.phase {
        Phase::Failed(reason) => unavailable(app, reason),
        Phase::Idle | Phase::Starting => {
            let starting = crate::store::working(
                &app.fx,
                format!("Starting {}…", app.effective_agent()),
                "The agent starts in your configuration directory and can read it from then on.",
            );
            column![title_bar(app), starting].spacing(16).into()
        }
        Phase::Ready => {
            let mut page = column![title_bar(app)].spacing(16);
            page = page.push(transcript(app));
            if !ai.plan.is_empty() {
                page = page.push(plan_card(&ai.plan));
            }
            for permit in &ai.permits {
                page = page.push(permission_card(permit));
            }
            page.push(input_card(app)).into()
        }
    }
}

fn title_bar(app: &App) -> Element<'_, Message> {
    let ai = &app.ai;
    let subtitle = match (&ai.phase, ai.agent.as_str()) {
        (Phase::Ready, name) if !name.is_empty() => name.to_owned(),
        _ => app.effective_agent().to_owned(),
    };
    let mut bar = row![
        icon_tile(lucide::sparkles(), ACCENT_ALT, 20.0, 42.0, 0.4),
        column![text("Assistant").size(20).font(BOLD), text(subtitle).size(12).color(MUTED)].spacing(2),
        space::horizontal(),
    ]
    .spacing(14)
    .align_y(Alignment::Center);

    if ai.modes.len() > 1 {
        let mut modes = row![].spacing(4);
        for mode in &ai.modes {
            let active = ai.mode.as_deref() == Some(mode.id.as_str());
            let chip = button(text(mode.name.clone()).size(12))
                .padding([5, 10])
                .style(move |_, status| choice_style(ACCENT, active, status))
                .on_press_maybe((!active).then(|| Message::AiMode(mode.id.clone())));
            modes = modes.push(describe(chip, &mode.description));
        }
        bar = bar.push(modes);
    }
    if matches!(ai.phase, Phase::Ready) {
        bar = bar.push(
            button(
                row![lucide::message_square_plus().size(13), text("New conversation").size(13)]
                    .spacing(8)
                    .align_y(Alignment::Center),
            )
            .padding([8, 14])
            .style(ghost_button)
            .on_press(Message::AiRestart),
        );
    }
    bar.into()
}

/// A tooltip with `description`, if there is one.
fn describe<'a>(content: impl Into<Element<'a, Message>>, description: &str) -> Element<'a, Message> {
    if description.is_empty() {
        return content.into();
    }
    tooltip(
        content,
        container(text(description.to_owned()).size(12).wrapping(Wrapping::WordOrGlyph))
            .max_width(360)
            .padding(8)
            .style(theme::card),
        tooltip::Position::Bottom,
    )
    .into()
}

fn unavailable<'a>(app: &'a App, reason: &str) -> Element<'a, Message> {
    let actions = row![
        button(row![lucide::rotate_ccw().size(13), text("Try again").size(13)].spacing(8).align_y(Alignment::Center))
            .padding([8, 14])
            .style(theme::primary_button)
            .on_press(Message::AiRestart),
        button(row![lucide::settings().size(13), text("Settings").size(13)].spacing(8).align_y(Alignment::Center))
            .padding([8, 14])
            .style(ghost_button)
            .on_press(Message::OpenSettings),
    ]
    .spacing(8);
    column![
        title_bar(app),
        failure("The assistant is not available", reason),
        card_with(
            column![
                text(format!("Command: {}", app.effective_agent())).size(13).font(Font::MONOSPACE),
                text(
                    "Nix Manager starts an AI agent that speaks the Agent Client Protocol. Install one \
                     (for example claude-code-acp) and make sure it is on your PATH, or set the command to use in Settings.",
                )
                .size(13)
                .color(MUTED),
                actions,
            ]
            .spacing(10)
            .into(),
            20,
        ),
    ]
    .spacing(16)
    .into()
}

fn transcript(app: &App) -> Element<'_, Message> {
    let ai = &app.ai;
    let mut entries = column![].spacing(14).padding(4);
    if ai.entries.is_empty() {
        entries = entries.push(suggestions(ai.busy));
    }
    for (index, entry) in ai.entries.iter().enumerate() {
        entries = entries.push(entry_view(app, index, entry));
    }
    if ai.busy && ai.permits.is_empty() {
        entries = entries.push(crate::view::bar(None, &app.fx, ACCENT_ALT, theme::RAISED, 3.0));
    }

    container(scrollable(entries).anchor_bottom().height(TRANSCRIPT_HEIGHT).width(Length::Fill))
        .padding(14)
        .width(Length::Fill)
        .style(theme::card)
        .into()
}

fn suggestions<'a>(busy: bool) -> Element<'a, Message> {
    let mut list = column![
        text("Ask about your configuration, or ask for a change.").size(15).font(BOLD),
        text("The agent reads your files, explains them, and edits them when you ask. It asks before it changes anything, and you review the result on the git page before committing.")
            .size(13)
            .color(MUTED),
    ]
    .spacing(6);
    for suggestion in SUGGESTIONS {
        list = list.push(
            button(row![lucide::corner_down_right().size(12).color(MUTED), text(suggestion).size(13)].spacing(8))
                .padding([6, 10])
                .style(ghost_button)
                .on_press_maybe((!busy).then(|| Message::AiAsk(suggestion.to_owned()))),
        );
    }
    list.into()
}

fn entry_view<'a>(app: &'a App, index: usize, entry: &'a Entry) -> Element<'a, Message> {
    match entry {
        Entry::User(question) => row![
            space::horizontal().width(Length::FillPortion(1)),
            container(text(question.clone()).size(14).wrapping(Wrapping::WordOrGlyph))
                .padding([8, 14])
                .max_width(640)
                .style(|_| container::Style {
                    background: Some(Background::Color(theme::with_alpha(ACCENT, 0.16))),
                    border: Border { color: theme::with_alpha(ACCENT, 0.4), width: 1.0, radius: 14.0.into() },
                    ..Default::default()
                }),
        ]
        .into(),
        Entry::Agent { rendered, .. } => {
            let style = markdown_style();
            column![
                markdown::view(rendered.items(), style).map(|url| Message::OpenUrl(url.to_string())),
                row![space::horizontal(), copy_button(app, CopyTarget::AiAnswer(index), false)],
            ]
            .spacing(2)
            .into()
        }
        Entry::Thought(thought) => row![
            lucide::brain().size(12).color(MUTED),
            text(thought.trim().to_owned())
                .size(12)
                .color(theme::with_alpha(MUTED, 0.85))
                .wrapping(Wrapping::WordOrGlyph),
        ]
        .spacing(8)
        .into(),
        Entry::Tool(tool) => tool_card(tool),
        Entry::Note { text: note, error } => {
            let (icon, tint) = if *error { (lucide::circle_x(), DANGER) } else { (lucide::info(), MUTED) };
            row![icon.size(13).color(tint), text(note.clone()).size(13).color(tint).wrapping(Wrapping::WordOrGlyph)]
                .spacing(8)
                .into()
        }
        Entry::Review => container(
            row![
                lucide::git_branch().size(14).color(WARNING),
                text("The agent changed your configuration.").size(13).width(Length::Fill),
                button(text("Review changes").size(13))
                    .padding([5, 12])
                    .style(theme::primary_button)
                    .on_press(Message::OpenGit),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        )
        .padding([8, 14])
        .style(theme::chip(WARNING))
        .into(),
    }
}

fn markdown_style() -> markdown::Settings {
    let mut style = markdown::Style::from_palette(theme::theme().palette());
    style.inline_code_color = TEAL;
    style.inline_code_highlight = markdown::Highlight {
        background: Background::Color(theme::RAISED),
        border: Border { radius: 4.0.into(), ..Default::default() },
    };
    markdown::Settings::with_text_size(14, style)
}

fn tool_icon(kind: ToolKind) -> Text<'static> {
    match kind {
        ToolKind::Read => lucide::file_text(),
        ToolKind::Edit => lucide::file_pen(),
        ToolKind::Delete => lucide::trash(),
        ToolKind::Move => lucide::move_right(),
        ToolKind::Search => lucide::search(),
        ToolKind::Execute => lucide::terminal(),
        ToolKind::Think => lucide::brain(),
        ToolKind::Fetch => lucide::globe(),
        ToolKind::SwitchMode | ToolKind::Other => lucide::wrench(),
    }
}

fn status_icon(status: ToolStatus) -> Text<'static> {
    match status {
        ToolStatus::Pending | ToolStatus::InProgress => lucide::loader().size(13).color(WARNING),
        ToolStatus::Completed => lucide::check().size(13).color(SUCCESS),
        ToolStatus::Failed => lucide::circle_x().size(13).color(DANGER),
    }
}

fn tool_card(tool: &ToolView) -> Element<'_, Message> {
    let header = row![
        status_icon(tool.call.status),
        tool_icon(tool.call.kind).size(13).color(MUTED),
        clipped(text(tool.call.title.clone()).size(13).font(Font::MONOSPACE)),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let mut card = column![header].spacing(8);
    for shown in &tool.shown {
        card = card.push(shown_view(shown));
    }
    container(card).padding([8, 12]).width(Length::Fill).style(theme::well).into()
}

fn shown_view(shown: &Shown) -> Element<'_, Message> {
    match shown {
        Shown::Text(output) => {
            let lines: Vec<&str> = output.lines().collect();
            let mut shown_text = lines.iter().take(MAX_SHOWN_LINES).copied().collect::<Vec<_>>().join("\n");
            if lines.len() > MAX_SHOWN_LINES {
                shown_text.push_str(&format!("\n… {} more lines", lines.len() - MAX_SHOWN_LINES));
            }
            text(shown_text).size(12).font(Font::MONOSPACE).color(MUTED).wrapping(Wrapping::WordOrGlyph).into()
        }
        Shown::Diff { path, lines, added, removed } => {
            let mut rows = column![];
            for line in lines.iter().take(MAX_SHOWN_LINES) {
                rows = rows.push(diff_row(line));
            }
            if lines.len() > MAX_SHOWN_LINES {
                rows = rows.push(
                    container(text(format!("… {} more lines", lines.len() - MAX_SHOWN_LINES)).size(12).color(MUTED))
                        .padding([4, 10]),
                );
            }
            column![
                row![
                    clipped(text(path.clone()).size(12).font(Font::MONOSPACE).color(MUTED)),
                    text(format!("+{added}")).size(12).font(Font::MONOSPACE).color(SUCCESS),
                    text(format!("−{removed}")).size(12).font(Font::MONOSPACE).color(DANGER),
                ]
                .spacing(8),
                container(rows).clip(true).style(theme::solid(theme::BG, 8.0)),
            ]
            .spacing(4)
            .into()
        }
    }
}

fn plan_card(plan: &[PlanEntry]) -> Element<'_, Message> {
    let done = plan.iter().filter(|entry| entry.status == PlanStatus::Completed).count();
    let mut list = column![row![
        lucide::list_checks().size(14).color(ACCENT),
        text(format!("Plan · {done} of {}", plan.len())).size(14).font(BOLD),
    ]
    .spacing(8)
    .align_y(Alignment::Center)]
    .spacing(6);
    for entry in plan {
        let (icon, tint) = match entry.status {
            PlanStatus::Completed => (lucide::circle_check(), SUCCESS),
            PlanStatus::InProgress => (lucide::loader(), WARNING),
            PlanStatus::Pending => (lucide::circle(), MUTED),
        };
        list = list.push(
            row![
                icon.size(13).color(tint),
                text(entry.text.clone()).size(13).wrapping(Wrapping::WordOrGlyph).width(Length::Fill)
            ]
            .spacing(8),
        );
    }
    card_with(list.into(), 14)
}

/// The agent waits for an answer.
fn permission_card(permit: &Permit) -> Element<'_, Message> {
    let mut choices = row![].spacing(8);
    for option in &permit.options {
        let tint = if option.kind.allows() { SUCCESS } else { DANGER };
        let solid = option.kind == OptionKind::AllowOnce;
        choices = choices.push(
            button(text(option.name.clone()).size(13))
                .padding([7, 14])
                .style(move |_, status| choice_style(tint, solid, status))
                .on_press(Message::AiAnswer(permit.id, Some(option.id.clone()))),
        );
    }

    let content = column![
        row![
            lucide::shield_alert().size(16).color(WARNING),
            text("The agent asks for permission").size(14).font(BOLD),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
        tool_card(&permit.tool),
        choices,
    ]
    .spacing(10);
    container(content).padding(14).width(Length::Fill).style(theme::chip(WARNING)).into()
}

/// A button in `tint`; filled when `strong`.
fn choice_style(tint: Color, strong: bool, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    let fill = match (strong, hovered) {
        (true, true) => theme::with_alpha(tint, 0.38),
        (true, false) => theme::with_alpha(tint, 0.26),
        (false, true) => theme::with_alpha(tint, 0.14),
        (false, false) => Color::TRANSPARENT,
    };
    button::Style {
        background: Some(Background::Color(fill)),
        text_color: tint,
        border: Border { color: theme::with_alpha(tint, 0.5), width: 1.0, radius: 10.0.into() },
        ..Default::default()
    }
}

fn input_card(app: &App) -> Element<'_, Message> {
    let ai = &app.ai;
    let online = matches!(app.conn, Conn::Online(_));
    let waiting = !ai.permits.is_empty();

    let editor = text_editor(&ai.input)
        .placeholder(if waiting {
            "The agent is waiting for your answer above."
        } else {
            "Ask about your configuration, or ask for a change…  (Enter sends, Shift+Enter starts a new line)"
        })
        .on_action(Message::AiInput)
        .key_binding(|press| match press.key.as_ref() {
            Key::Named(Named::Enter) if !press.modifiers.shift() => Some(text_editor::Binding::Custom(Message::AiSend)),
            _ => text_editor::Binding::from_key_press(press),
        })
        .size(14)
        .padding(10)
        .height(INPUT_HEIGHT)
        .wrapping(Wrapping::WordOrGlyph)
        .style(crate::git::message_style);

    let action: Element<_> = if ai.busy {
        button(row![lucide::square().size(13), text("Stop").size(14).font(BOLD)].spacing(8).align_y(Alignment::Center))
            .padding([9, 18])
            .style(|_, status| choice_style(DANGER, true, status))
            .on_press(Message::AiCancel)
            .into()
    } else {
        button(row![lucide::send().size(14), text("Send").size(14).font(BOLD)].spacing(8).align_y(Alignment::Center))
            .padding([9, 18])
            .style(theme::primary_button)
            .on_press_maybe((ai.can_send() && online).then_some(Message::AiSend))
            .into()
    };

    let note = "Changes are made in the files of your configuration; nothing is rebuilt or committed for you.";
    card_with(
        column![editor, row![clipped(text(note).size(12).color(MUTED)), action].spacing(12).align_y(Alignment::Center)]
            .spacing(10)
            .into(),
        14,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Page;
    use nix_agent_client::{PermissionOption, StopReason};

    fn call(id: &str, kind: ToolKind, status: ToolStatus) -> ToolCall {
        ToolCall { id: id.to_owned(), title: format!("tool {id}"), kind, status, content: Vec::new() }
    }

    fn ready() -> Ai {
        let mut ai = Ai::new();
        ai.apply(Event::Ready { agent: "fake 1".to_owned(), modes: Vec::new(), mode: None });
        ai
    }

    #[test]
    fn the_answer_is_written_in_pieces_into_one_entry() {
        let mut ai = ready();
        ai.apply(Event::Message("Hello ".to_owned()));
        ai.apply(Event::Message("**world**".to_owned()));
        assert_eq!(ai.entries.len(), 1);
        assert_eq!(ai.answer_text(0), Some("Hello **world**"));

        // Something else in between makes the next piece a new entry.
        ai.apply(Event::Tool(call("t", ToolKind::Read, ToolStatus::Pending)));
        ai.apply(Event::Message("More".to_owned()));
        assert_eq!(ai.entries.len(), 3);
        assert_eq!(ai.answer_text(2), Some("More"));
        assert_eq!(ai.answer_text(1), None);
    }

    #[test]
    fn a_tool_call_is_updated_in_place() {
        let mut ai = ready();
        ai.apply(Event::Tool(call("a", ToolKind::Read, ToolStatus::Pending)));
        ai.apply(Event::Tool(call("b", ToolKind::Search, ToolStatus::Pending)));
        ai.apply(Event::Tool(call("a", ToolKind::Read, ToolStatus::Completed)));
        assert_eq!(ai.entries.len(), 2);
        assert!(matches!(&ai.entries[0], Entry::Tool(t) if t.call.status == ToolStatus::Completed));
    }

    #[test]
    fn an_edit_ends_with_an_offer_to_review_it() {
        let mut ai = ready();
        ai.apply(Event::Tool(call("e", ToolKind::Edit, ToolStatus::Completed)));
        ai.apply(Event::TurnEnded(StopReason::EndTurn));
        assert!(matches!(ai.entries.last(), Some(Entry::Review)));

        // Reading alone does not.
        let mut ai = ready();
        ai.apply(Event::Tool(call("r", ToolKind::Read, ToolStatus::Completed)));
        ai.apply(Event::TurnEnded(StopReason::EndTurn));
        assert!(!ai.entries.iter().any(|e| matches!(e, Entry::Review)));

        // An edit that failed changed nothing.
        let mut ai = ready();
        ai.apply(Event::Tool(call("e", ToolKind::Edit, ToolStatus::Failed)));
        ai.apply(Event::TurnEnded(StopReason::EndTurn));
        assert!(!ai.entries.iter().any(|e| matches!(e, Entry::Review)));
    }

    #[test]
    fn a_diff_is_prepared_for_drawing() {
        let mut edit = call("e", ToolKind::Edit, ToolStatus::Pending);
        edit.content = vec![ToolContent::Diff {
            path: "a.nix".to_owned(),
            old: Some("one\ntwo\n".to_owned()),
            new: "one\n2\n".to_owned(),
        }];
        let view = ToolView::new(edit);
        assert!(view.changes_files());
        assert!(matches!(&view.shown[..], [Shown::Diff { added: 1, removed: 1, .. }]));
    }

    #[test]
    fn a_turn_ending_clears_what_waited_and_says_why_it_stopped() {
        let mut ai = ready();
        ai.busy = true;
        ai.apply(Event::Permission(Permission {
            id: 7,
            tool: call("e", ToolKind::Edit, ToolStatus::Pending),
            options: vec![PermissionOption {
                id: "ok".to_owned(),
                name: "Allow".to_owned(),
                kind: OptionKind::AllowOnce,
            }],
        }));
        assert_eq!(ai.permits.len(), 1);

        ai.apply(Event::TurnEnded(StopReason::Cancelled));
        assert!(!ai.busy && ai.permits.is_empty());
        assert!(matches!(ai.entries.last(), Some(Entry::Note { error: false, .. })));

        ai.busy = true;
        ai.apply(Event::TurnFailed("boom".to_owned()));
        assert!(!ai.busy);
        assert!(matches!(ai.entries.last(), Some(Entry::Note { text, error: true }) if text == "boom"));
    }

    #[test]
    fn the_agent_going_away_is_a_failure_with_its_reason() {
        let mut ai = ready();
        ai.busy = true;
        ai.apply(Event::Closed(Some("No such file".to_owned())));
        assert!(!ai.busy);
        assert!(matches!(&ai.phase, Phase::Failed(reason) if reason == "No such file"));
        assert!(!ai.can_send());
    }

    #[test]
    fn nothing_is_sent_before_the_agent_is_ready_or_while_it_is_busy() {
        let mut ai = Ai::new();
        ai.input = text_editor::Content::with_text("hi");
        assert!(!ai.ask(None), "not started");

        let mut ai = ready();
        assert!(!ai.ask(Some("hi".to_owned())), "ready, but no agent behind it in this test");
    }

    #[test]
    fn events_are_taken_in_batches() {
        let mailbox = Mailbox::default();
        assert!(mailbox.push(Event::Message("a".to_owned())), "the first wakes the UI");
        assert!(!mailbox.push(Event::Message("b".to_owned())), "the rest wait for it");
        assert_eq!(mailbox.take().len(), 2);
        assert!(mailbox.push(Event::Message("c".to_owned())), "and after it was taken, the next wakes it again");
    }

    #[test]
    fn stopping_forgets_the_conversation_but_not_what_was_typed() {
        let mut ai = ready();
        ai.input = text_editor::Content::with_text("draft");
        ai.apply(Event::Message("x".to_owned()));
        let before = ai.session;

        ai.stop();
        assert!(ai.entries.is_empty() && matches!(ai.phase, Phase::Idle));
        assert_eq!(ai.input.text().trim(), "draft");
        assert_eq!(ai.session, before + 1);
    }

    #[test]
    fn every_state_of_the_page_can_be_built() {
        let (mut app, _) = App::new();
        app.page = Page::Ai;
        let _ = crate::view::view(&app);

        app.ai.phase = Phase::Starting;
        let _ = crate::view::view(&app);
        app.ai.phase = Phase::Failed("boom".to_owned());
        let _ = crate::view::view(&app);

        app.ai = ready();
        let _ = crate::view::view(&app);
        app.ai.modes = vec![
            Mode { id: "ask".to_owned(), name: "Ask".to_owned(), description: "Ask before editing".to_owned() },
            Mode { id: "plan".to_owned(), name: "Plan".to_owned(), description: String::new() },
        ];
        app.ai.mode = Some("ask".to_owned());
        app.ai.entries.push(Entry::User("hi".to_owned()));
        app.ai.apply(Event::Thought("hmm".to_owned()));
        app.ai.apply(Event::Message(
            "# Title\n\nSome `code` and a [link](https://example.com).\n\n```nix\n{ }\n```".to_owned(),
        ));
        let mut edit = call("e", ToolKind::Edit, ToolStatus::InProgress);
        edit.content = vec![
            ToolContent::Text("output\nmore".to_owned()),
            ToolContent::Diff { path: "a.nix".to_owned(), old: None, new: "x\n".repeat(300) },
        ];
        app.ai.apply(Event::Tool(edit.clone()));
        app.ai.apply(Event::Plan(vec![
            PlanEntry { text: "one".to_owned(), status: PlanStatus::Completed },
            PlanEntry { text: "two".to_owned(), status: PlanStatus::InProgress },
            PlanEntry { text: "three".to_owned(), status: PlanStatus::Pending },
        ]));
        app.ai.apply(Event::Permission(Permission {
            id: 1,
            tool: edit,
            options: vec![
                PermissionOption { id: "a".to_owned(), name: "Allow".to_owned(), kind: OptionKind::AllowOnce },
                PermissionOption { id: "n".to_owned(), name: "Deny".to_owned(), kind: OptionKind::RejectOnce },
            ],
        }));
        app.ai.apply(Event::TurnFailed("it broke".to_owned()));
        app.ai.entries.push(Entry::Review);
        app.ai.busy = true;
        let _ = crate::view::view(&app);
    }
}
