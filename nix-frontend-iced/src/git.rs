//! The git page: what changed in the configuration, staging and unstaging it,
//! committing, and a diff of any changed file.

use iced::keyboard::{key::Named, Key};
use iced::widget::text::Wrapping;
use iced::widget::{button, column, container, row, scrollable, space, text, text_editor, tooltip, Text};
use iced::{Alignment, Background, Border, Color, Element, Font, Length};
use iced_fonts::lucide;
use nix_backend_client::diff::{self, Kind, Line};
use nix_backend_client::{GitChange, GitFile, GitStatusReply};

use crate::store::{card_with, failure, notice, working, Banner, Load};
use crate::theme::{self, ACCENT, ACCENT_ALT, DANGER, MUTED, SUCCESS, TEAL, TEXT, WARNING};
use crate::view::{centered_icon, clipped, copy_button, ghost_button, icon_tile, BOLD};
use crate::{App, Conn, CopyTarget, Message};

/// A diff longer than this many lines is shown cut off: every line is a widget.
const MAX_DIFF_ROWS: usize = 1500;

const MESSAGE_HEIGHT: f32 = 110.0;
const DIFF_HEIGHT: f32 = 600.0;
/// Wide enough for the five digit line numbers of two columns.
const GUTTER_WIDTH: f32 = 84.0;

/// The file whose diff is open, and which side of staging it is on.
#[derive(Clone, Debug, PartialEq)]
pub struct Selected {
    pub path: String,
    pub staged: bool,
}

/// A file's diff, ready to draw.
pub struct Diff {
    /// What the backend sent, for copying.
    pub raw: String,
    pub lines: Vec<Line>,
    /// The backend cut the diff off for being too large.
    pub truncated: bool,
    pub added: usize,
    pub removed: usize,
}

impl Diff {
    pub fn new(raw: String, truncated: bool) -> Self {
        let lines = diff::parse(&raw);
        let (added, removed) = diff::counts(&lines);
        Self { raw, lines, truncated, added, removed }
    }
}

pub struct Git {
    pub status: Load<GitStatusReply>,
    /// The status is being fetched again; what is shown stays until it arrives.
    pub refreshing: bool,
    /// The commit message being typed.
    pub message: text_editor::Content,
    pub selected: Option<Selected>,
    pub diff: Load<Diff>,
    /// Counts diff requests, so that the answer to an old one is recognised and dropped.
    pub diff_seq: u64,
    /// A stage, unstage or commit is on its way.
    pub working: bool,
    pub banner: Option<Banner>,
}

impl Git {
    pub fn new() -> Self {
        Self {
            status: Load::Idle,
            refreshing: false,
            message: text_editor::Content::new(),
            selected: None,
            diff: Load::Idle,
            diff_seq: 0,
            working: false,
            banner: None,
        }
    }

    /// Forgets everything that came from the previous target, keeping the
    /// message that was being typed.
    pub fn reset(&mut self) {
        *self = Self { message: std::mem::replace(&mut self.message, text_editor::Content::new()), ..Self::new() };
    }

    /// Whether anything on this page is waiting for the backend.
    pub fn busy(&self) -> bool {
        self.status.loading() || self.diff.loading() || self.working
    }

    pub fn files(&self) -> &[GitFile] {
        self.status.ready().map_or(&[], |status| status.files.as_slice())
    }

    pub fn staged(&self) -> impl Iterator<Item = &GitFile> {
        self.files().iter().filter(|file| file.is_staged())
    }

    pub fn unstaged(&self) -> impl Iterator<Item = &GitFile> {
        self.files().iter().filter(|file| file.is_unstaged())
    }

    /// The file the open diff belongs to, if it is still listed on that side.
    pub fn selected_file(&self) -> Option<&GitFile> {
        let selected = self.selected.as_ref()?;
        self.files().iter().find(|file| {
            file.path == selected.path && if selected.staged { file.is_staged() } else { file.is_unstaged() }
        })
    }

    /// Takes in a new status. The open diff stays on its file: if the file
    /// moved to the other side of staging the selection follows it, and if it is
    /// no longer changed the selection is dropped.
    pub fn set_status(&mut self, status: GitStatusReply) {
        self.status = Load::Ready(status);
        self.refreshing = false;
        if self.selected_file().is_some() {
            return;
        }
        let moved = self.selected.take().and_then(|selected| {
            let file = self.files().iter().find(|file| file.path == selected.path)?;
            let staged = !selected.staged;
            (if staged { file.is_staged() } else { file.is_unstaged() })
                .then_some(Selected { path: selected.path, staged })
        });
        if moved.is_none() {
            self.diff = Load::Idle;
        }
        self.selected = moved;
    }

    pub fn can_commit(&self) -> bool {
        !self.working && self.staged().next().is_some() && !self.message.text().trim().is_empty()
    }
}

// ------------------------------------------------------------------ views --

pub fn page(app: &App) -> Element<'_, Message> {
    let git = &app.git;
    let body: Element<_> = if !matches!(app.conn, Conn::Online(_)) {
        notice(lucide::unplug(), MUTED, "Backend offline", "Git is driven by the backend. It reconnects automatically.")
    } else {
        match &git.status {
            Load::Idle | Load::Loading => {
                working(&app.fx, "Reading the repository…".to_owned(), "Looking for changed, staged and new files.")
            }
            Load::Failed(error) => column![
                failure("Could not read the repository", error),
                button(
                    row![lucide::rotate_ccw().size(13), text("Try again").size(13)]
                        .spacing(8)
                        .align_y(Alignment::Center)
                )
                .padding([6, 12])
                .style(ghost_button)
                .on_press(Message::GitReload),
            ]
            .spacing(10)
            .into(),
            Load::Ready(status) if !status.is_repo => notice(
                lucide::git_branch(),
                MUTED,
                "Not a git repository",
                &format!(
                    "{} isn't inside a git repository. Run `git init` there to start tracking changes.",
                    app.effective_config_dir()
                ),
            ),
            Load::Ready(status) => repository(app, status),
        }
    };

    match &git.banner {
        Some(banner) => column![banner_view(banner), body].spacing(16).into(),
        None => body,
    }
}

fn repository<'a>(app: &'a App, status: &'a GitStatusReply) -> Element<'a, Message> {
    let git = &app.git;
    let left =
        column![commit_card(app), section(app, true), section(app, false)].spacing(16).width(Length::FillPortion(2));
    let right = container(diff_panel(app)).width(Length::FillPortion(3));
    let mut page =
        column![branch_bar(app, status), row![left, right].spacing(16).align_y(Alignment::Start)].spacing(16);
    if git.files().is_empty() {
        page = page.push(notice(
            lucide::circle_check(),
            SUCCESS,
            "Working tree clean",
            "Nothing has changed since the last commit.",
        ));
    }
    page.into()
}

fn branch_bar<'a>(app: &'a App, status: &'a GitStatusReply) -> Element<'a, Message> {
    let name = if status.detached { "detached HEAD".to_owned() } else { status.branch.clone() };
    let chip = |icon: Text<'a>, label: String, tint: Color| {
        container(
            row![icon.size(12).color(tint), text(label).size(12).color(tint)].spacing(6).align_y(Alignment::Center),
        )
        .padding([4, 10])
        .style(theme::chip(tint))
    };

    let mut bar = row![chip(lucide::git_branch(), name, ACCENT)].spacing(8).align_y(Alignment::Center);
    if !status.upstream.is_empty() {
        bar = bar.push(text(format!("tracks {}", status.upstream)).size(12).color(MUTED));
    }
    if status.ahead > 0 {
        bar = bar.push(chip(lucide::arrow_up(), status.ahead.to_string(), TEAL));
    }
    if status.behind > 0 {
        bar = bar.push(chip(lucide::arrow_down(), status.behind.to_string(), WARNING));
    }
    bar.push(space::horizontal())
        .push(
            button(
                row![
                    lucide::refresh_cw().size(13),
                    text(if app.git.refreshing { "Refreshing" } else { "Refresh" }).size(13)
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            )
            .padding([8, 14])
            .style(ghost_button)
            .on_press_maybe((!app.git.refreshing).then_some(Message::GitReload)),
        )
        .into()
}

fn commit_card(app: &App) -> Element<'_, Message> {
    let git = &app.git;
    let staged = git.staged().count();
    let has_message = !git.message.text().trim().is_empty();

    let editor = text_editor(&git.message)
        .placeholder("Commit message")
        .on_action(Message::GitMessage)
        .key_binding(|press| match press.key.as_ref() {
            Key::Named(Named::Enter) if press.modifiers.command() => {
                Some(text_editor::Binding::Custom(Message::GitCommit))
            }
            _ => text_editor::Binding::from_key_press(press),
        })
        .size(14)
        .padding(10)
        .height(MESSAGE_HEIGHT)
        .wrapping(Wrapping::WordOrGlyph)
        .style(message_style);

    let hint = match (staged, has_message) {
        (0, _) => "Stage files to commit them".to_owned(),
        (n, false) => format!("Write a message to commit {n} file{}", plural(n)),
        (n, true) => format!("Commits {n} staged file{} · Ctrl+Enter", plural(n)),
    };
    let commit = button(
        row![lucide::git_commit_horizontal().size(14), text("Commit").size(14).font(BOLD)]
            .spacing(8)
            .align_y(Alignment::Center),
    )
    .padding([9, 18])
    .style(theme::primary_button)
    .on_press_maybe(git.can_commit().then_some(Message::GitCommit));

    card_with(
        column![
            editor,
            row![clipped(text(hint).size(12).color(MUTED)), commit].spacing(12).align_y(Alignment::Center),
        ]
        .spacing(10)
        .into(),
        14,
    )
}

fn message_style(_: &iced::Theme, status: text_editor::Status) -> text_editor::Style {
    let border = match status {
        text_editor::Status::Focused { .. } => ACCENT,
        text_editor::Status::Hovered => theme::mix(theme::BORDER, ACCENT, 0.4),
        _ => theme::BORDER,
    };
    text_editor::Style {
        background: Background::Color(theme::RAISED),
        border: Border { color: border, width: 1.0, radius: 10.0.into() },
        placeholder: theme::with_alpha(MUTED, 0.7),
        value: TEXT,
        selection: theme::with_alpha(ACCENT, 0.35),
    }
}

/// The staged or the unstaged files, as a card with a button for moving them all.
fn section(app: &App, staged: bool) -> Element<'_, Message> {
    let git = &app.git;
    let files: Vec<&GitFile> = if staged { git.staged().collect() } else { git.unstaged().collect() };
    let (title, icon, tint) = if staged {
        ("Staged changes", lucide::circle_check(), SUCCESS)
    } else {
        ("Changes", lucide::file_pen(), WARNING)
    };

    let all = (!files.is_empty()).then(|| {
        let (label, icon) = if staged { ("Unstage all", lucide::minus()) } else { ("Stage all", lucide::plus()) };
        button(row![icon.size(12), text(label).size(12)].spacing(6).align_y(Alignment::Center))
            .padding([5, 10])
            .style(ghost_button)
            .on_press_maybe((!git.working).then_some(Message::GitStage(Vec::new(), !staged)))
    });
    let mut header = row![
        icon.size(14).color(tint),
        text(format!("{title} · {}", files.len())).size(14).font(BOLD),
        space::horizontal(),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    if let Some(all) = all {
        header = header.push(all);
    }

    let mut list = column![].spacing(2);
    for file in &files {
        list = list.push(file_row(app, file, staged));
    }
    if files.is_empty() {
        let empty = if staged { "Nothing is staged." } else { "No unstaged changes." };
        list = list.push(container(text(empty).size(13).color(MUTED)).padding([6, 8]));
    }

    card_with(column![container(header).padding([4, 8]), list].spacing(6).into(), 8)
}

fn file_row<'a>(app: &App, file: &'a GitFile, staged: bool) -> Element<'a, Message> {
    let change = if staged { file.staged_change() } else { file.unstaged_change() };
    let selected = app.git.selected.as_ref().is_some_and(|s| s.staged == staged && s.path == file.path);

    let (dir, name) = file.path.rsplit_once('/').map_or(("", file.path.as_str()), |(dir, name)| (dir, name));
    let detail = if file.old_path.is_empty() { dir.to_owned() } else { format!("from {}", file.old_path) };
    let mut label = column![clipped(text(name).size(14))].spacing(1).width(Length::Fill);
    if !detail.is_empty() {
        label = label.push(clipped(text(detail).size(11).color(MUTED)));
    }

    let (icon, caption) = if staged { (lucide::minus(), "Unstage") } else { (lucide::plus(), "Stage") };
    let toggle = tooltip(
        button(centered_icon(icon, 13.0, MUTED))
            .padding(7)
            .style(ghost_button)
            .on_press_maybe((!app.git.working).then(|| Message::GitStage(file.paths(), !staged))),
        container(text(caption).size(12)).padding(6).style(theme::card),
        tooltip::Position::Left,
    );

    row![
        button(row![badge(change), label].spacing(10).align_y(Alignment::Center))
            .padding([6, 8])
            .width(Length::Fill)
            .style(move |_, status| row_style(selected, status))
            .on_press(Message::GitSelect(Selected { path: file.path.clone(), staged })),
        toggle,
    ]
    .spacing(4)
    .align_y(Alignment::Center)
    .into()
}

fn row_style(selected: bool, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: Some(Background::Color(if selected {
            theme::with_alpha(ACCENT, 0.14)
        } else if hovered {
            theme::RAISED
        } else {
            Color::TRANSPARENT
        })),
        text_color: TEXT,
        border: Border {
            color: if selected { theme::with_alpha(ACCENT, 0.4) } else { Color::TRANSPARENT },
            width: 1.0,
            radius: 10.0.into(),
        },
        ..Default::default()
    }
}

/// The letter that tells what happened to a file.
fn badge<'a>(change: GitChange) -> Element<'a, Message> {
    let (letter, tint, meaning) = match change {
        GitChange::Added => ("A", SUCCESS, "Added"),
        GitChange::Modified => ("M", WARNING, "Modified"),
        GitChange::Deleted => ("D", DANGER, "Deleted"),
        GitChange::Renamed => ("R", ACCENT, "Renamed"),
        GitChange::Copied => ("C", ACCENT, "Copied"),
        GitChange::TypeChanged => ("T", ACCENT_ALT, "Type changed"),
        GitChange::Untracked => ("U", TEAL, "New file, not tracked yet"),
        GitChange::Conflicted => ("!", DANGER, "Merge conflict"),
        GitChange::None => ("", MUTED, ""),
    };
    tooltip(
        container(text(letter).size(12).font(Font { weight: iced::font::Weight::Bold, ..Font::MONOSPACE }).color(tint))
            .width(22)
            .height(22)
            .center(22)
            .style(theme::chip(tint)),
        container(text(meaning).size(12)).padding(6).style(theme::card),
        tooltip::Position::Right,
    )
    .into()
}

// ------------------------------------------------------------------- diff --

fn diff_panel(app: &App) -> Element<'_, Message> {
    let git = &app.git;
    let Some(selected) = &git.selected else {
        return notice(
            lucide::file_diff(),
            ACCENT,
            "Select a file",
            "Pick a changed file on the left to see what changed in it.",
        );
    };

    let side = if selected.staged { ("staged", SUCCESS) } else { ("not staged", WARNING) };
    let mut header = row![
        icon_tile(lucide::file_diff(), ACCENT_ALT, 14.0, 30.0, 0.0),
        container(clipped(
            text(selected.path.clone()).size(14).font(Font { weight: iced::font::Weight::Semibold, ..Font::MONOSPACE })
        ))
        .width(Length::Fill),
        container(text(side.0).size(11).color(side.1)).padding([3, 8]).style(theme::chip(side.1)),
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    let body: Element<_> = match &git.diff {
        Load::Idle | Load::Loading => working(&app.fx, "Reading the changes…".to_owned(), ""),
        Load::Failed(error) => text(error.clone()).size(13).color(DANGER).into(),
        Load::Ready(diff) => {
            header = header
                .push(text(format!("+{}", diff.added)).size(13).font(Font::MONOSPACE).color(SUCCESS))
                .push(text(format!("−{}", diff.removed)).size(13).font(Font::MONOSPACE).color(DANGER))
                .push(copy_button(app, CopyTarget::Diff, false));
            diff_lines(diff)
        }
    };

    card_with(column![header, body].spacing(12).into(), 14)
}

fn diff_lines(diff: &Diff) -> Element<'_, Message> {
    if diff.lines.is_empty() {
        return container(text("No changes to show.").size(13).color(MUTED)).padding([6, 4]).into();
    }

    let mut rows = column![];
    for line in diff.lines.iter().take(MAX_DIFF_ROWS) {
        rows = rows.push(diff_row(line));
    }
    let hidden = diff.lines.len().saturating_sub(MAX_DIFF_ROWS);
    if hidden > 0 || diff.truncated {
        let note = if hidden > 0 {
            format!("{hidden} more lines are not shown. Use Copy to get the whole diff.")
        } else {
            "The diff is too large to show in full.".to_owned()
        };
        rows = rows.push(container(text(note).size(12).color(MUTED)).padding([8, 10]));
    }

    container(scrollable(rows).height(DIFF_HEIGHT).width(Length::Fill)).style(theme::well).clip(true).into()
}

fn diff_row(line: &Line) -> Element<'_, Message> {
    let number = |n: Option<u32>| n.map_or_else(|| "    ".to_owned(), |n| format!("{n:>4}"));
    let (sign, tint, fill) = match line.kind {
        Kind::Added => ("+", SUCCESS, Some(theme::with_alpha(SUCCESS, 0.13))),
        Kind::Removed => ("-", DANGER, Some(theme::with_alpha(DANGER, 0.13))),
        Kind::Hunk => ("", ACCENT, Some(theme::with_alpha(ACCENT, 0.1))),
        Kind::Note => ("", MUTED, None),
        Kind::Context => (" ", MUTED, None),
    };
    let mono = |content: String| text(content).size(12).font(Font::MONOSPACE);

    let content: Element<_> = match line.kind {
        Kind::Hunk | Kind::Note => {
            mono(line.text.clone()).color(tint).wrapping(Wrapping::WordOrGlyph).width(Length::Fill).into()
        }
        _ => row![
            mono(sign.to_owned()).color(tint),
            mono(if line.text.is_empty() { " ".to_owned() } else { line.text.clone() })
                .wrapping(Wrapping::WordOrGlyph)
                .width(Length::Fill),
        ]
        .spacing(8)
        .into(),
    };
    let gutter = match line.kind {
        Kind::Hunk | Kind::Note => String::new(),
        _ => format!("{} {}", number(line.old), number(line.new)),
    };

    let row = row![mono(gutter).color(theme::with_alpha(MUTED, 0.7)).width(GUTTER_WIDTH), content].spacing(10);
    let row = container(row).padding([1, 10]).width(Length::Fill);
    match fill {
        Some(fill) => row.style(theme::solid(fill, 0.0)).into(),
        None => row.into(),
    }
}

fn banner_view(banner: &Banner) -> Element<'_, Message> {
    let (icon, tint) = if banner.error { (lucide::circle_x(), DANGER) } else { (lucide::circle_check(), SUCCESS) };
    container(
        row![
            icon.size(16).color(tint),
            text(banner.text.clone()).size(13).width(Length::Fill),
            button(lucide::x().size(14).color(MUTED))
                .padding(6)
                .style(ghost_button)
                .on_press(Message::GitDismissBanner),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .padding([10, 16])
    .width(Length::Fill)
    .style(theme::chip(tint))
    .into()
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, staged: GitChange, unstaged: GitChange) -> GitFile {
        GitFile { path: path.to_owned(), old_path: String::new(), staged: staged as i32, unstaged: unstaged as i32 }
    }

    fn git_with(files: Vec<GitFile>, selected: Option<(&str, bool)>) -> Git {
        let mut git = Git::new();
        git.status = Load::Ready(GitStatusReply { is_repo: true, files, ..Default::default() });
        git.selected = selected.map(|(path, staged)| Selected { path: path.to_owned(), staged });
        git
    }

    fn status(files: Vec<GitFile>) -> GitStatusReply {
        GitStatusReply { is_repo: true, files, ..Default::default() }
    }

    #[test]
    fn a_file_changed_twice_is_in_both_lists() {
        let git = git_with(
            vec![
                file("a", GitChange::Modified, GitChange::Modified),
                file("b", GitChange::None, GitChange::Untracked),
                file("c", GitChange::Added, GitChange::None),
            ],
            None,
        );
        assert_eq!(git.staged().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["a", "c"]);
        assert_eq!(git.unstaged().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    }

    #[test]
    fn staging_the_open_file_moves_the_diff_with_it() {
        let mut git = git_with(vec![file("a", GitChange::None, GitChange::Modified)], Some(("a", false)));
        git.diff = Load::Ready(Diff::new(String::new(), false));

        git.set_status(status(vec![file("a", GitChange::Modified, GitChange::None)]));
        assert_eq!(git.selected, Some(Selected { path: "a".to_owned(), staged: true }));
        assert!(git.diff.ready().is_some(), "the old diff stays until the new one arrives");
    }

    #[test]
    fn the_open_file_stays_put_when_it_is_still_changed() {
        let mut git = git_with(vec![file("a", GitChange::Modified, GitChange::Modified)], Some(("a", false)));
        git.set_status(status(vec![file("a", GitChange::Modified, GitChange::Modified)]));
        assert_eq!(git.selected, Some(Selected { path: "a".to_owned(), staged: false }));
    }

    #[test]
    fn a_file_that_is_no_longer_changed_closes_its_diff() {
        let mut git = git_with(vec![file("a", GitChange::Modified, GitChange::None)], Some(("a", true)));
        git.diff = Load::Ready(Diff::new(String::new(), false));

        git.set_status(status(Vec::new()));
        assert_eq!(git.selected, None);
        assert!(matches!(git.diff, Load::Idle));
    }

    #[test]
    fn a_commit_needs_a_message_and_a_staged_file() {
        let mut git = git_with(vec![file("a", GitChange::None, GitChange::Modified)], None);
        git.message = text_editor::Content::with_text("Fix it");
        assert!(!git.can_commit(), "nothing is staged");

        git.set_status(status(vec![file("a", GitChange::Modified, GitChange::None)]));
        assert!(git.can_commit());

        git.message = text_editor::Content::with_text("  \n ");
        assert!(!git.can_commit(), "a blank message is no message");

        git.message = text_editor::Content::with_text("Fix it");
        git.working = true;
        assert!(!git.can_commit(), "one change at a time");
    }

    #[test]
    fn a_rename_is_moved_under_both_names() {
        let mut renamed = file("new.nix", GitChange::Renamed, GitChange::None);
        renamed.old_path = "old.nix".to_owned();
        assert_eq!(renamed.paths(), ["new.nix", "old.nix"]);
        assert_eq!(file("a", GitChange::None, GitChange::Modified).paths(), ["a"]);
    }

    #[test]
    fn every_state_of_the_page_can_be_built() {
        let (mut app, _) = App::new();
        app.page = crate::Page::Git;
        let mut renamed = file("new.nix", GitChange::Renamed, GitChange::Modified);
        renamed.old_path = "dir/old.nix".to_owned();
        let lines = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@ fn\n keep\n-old\n+new\n\\ No newline at end of file\n";

        // Not connected: nothing loaded yet, then each way a load can end.
        let _ = crate::view::view(&app);
        for status in [
            Load::Loading,
            Load::Failed("boom".to_owned()),
            Load::Ready(GitStatusReply::default()),
            Load::Ready(status(Vec::new())),
            Load::Ready(GitStatusReply {
                is_repo: true,
                branch: "main".to_owned(),
                upstream: "origin/main".to_owned(),
                ahead: 2,
                behind: 1,
                files: vec![renamed.clone(), file("b/c.nix", GitChange::None, GitChange::Untracked)],
                ..Default::default()
            }),
        ] {
            app.git.status = status;
            let _ = crate::view::view(&app);
        }

        app.git.selected = Some(Selected { path: "new.nix".to_owned(), staged: true });
        for diff in [
            Load::Loading,
            Load::Failed("boom".to_owned()),
            Load::Ready(Diff::new(String::new(), false)),
            Load::Ready(Diff::new(lines.to_owned(), false)),
            Load::Ready(Diff::new(format!("@@ -1 +1 @@\n{}", "+x\n".repeat(MAX_DIFF_ROWS + 10)), true)),
        ] {
            app.git.diff = diff;
            let _ = crate::view::view(&app);
        }
        app.git.banner = Some(Banner { text: "done".to_owned(), error: false, action: None });
        let _ = crate::view::view(&app);
    }
}
