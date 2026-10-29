//! Layout. Styling comes from `theme`, motion from `fx`.

use chrono::{DateTime, Local, Utc};
use iced::widget::{
    button, column, container, mouse_area, row, scrollable, space, stack, text, text::Wrapping, text_input, Text,
};
use iced::{font, mouse, Alignment, Color, Element, Font, Length, Padding};
use iced_fonts::lucide;
use nix_backend_client::{
    format_bytes, phase_label, BuildItem, BuildItemKind, BuildItemStatus, EventPhase, OperationKind, RebuildMode,
};

use crate::fx::{Fx, Slot, SLIDE};
use crate::theme::{self, ACCENT, ACCENT_ALT, DANGER, MUTED, SUCCESS, TEAL, TEXT, WARNING};
use crate::{store, Action, App, Conn, CopyTarget, Message, Outcome, Page, Run};

/// The output view: fixed row height and viewport height let it draw only the
/// rows that are visible.
const LOG_ROW_HEIGHT: f32 = 18.0;
const LOG_VIEW_HEIGHT: f32 = 220.0;
/// Extra rows built above and below the viewport so scrolling never shows a gap.
const LOG_OVERSCAN: usize = 4;

pub(crate) const BOLD: Font = Font { weight: font::Weight::Semibold, ..Font::DEFAULT };

pub fn view(app: &App) -> Element<'_, Message> {
    let fx = &app.fx;

    let mut sections = column![reveal(fx, Slot::Header, header(app))].spacing(SLIDE_GAP);

    match app.page {
        Page::Dashboard => {
            sections =
                sections.push(reveal(fx, Slot::Overview, overview(app))).push(reveal(fx, Slot::Actions, actions(app)));
            if let Some(notice) = &app.notice {
                sections = sections.push(notice_banner(notice));
            }
            sections = sections.push(match &app.run {
                Some(run) => reveal(fx, Slot::Activity, activity(app, run)),
                None => idle_panel(),
            });
        }
        Page::Settings => {
            if let Some(notice) = &app.notice {
                sections = sections.push(notice_banner(notice));
            }
            sections = sections.push(reveal(fx, Slot::Overview, settings(app)));
        }
        Page::Store => {
            sections = sections.push(reveal(fx, Slot::Overview, store::page(app)));
        }
    }

    container(scrollable(container(sections.max_width(1000)).padding(28).center_x(Length::Fill)))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(theme::page)
        .into()
}

/// Each section carries `SLIDE` px of bottom padding (see `reveal`), so the
/// visible gap between sections is this plus that.
const SLIDE_GAP: f32 = 6.0;

/// Fades and slides a section in. The fade is a page-coloured veil on top
/// (works for any content, icons and text included), and the slide moves the
/// content down inside constant-height padding so nothing below it jumps.
fn reveal<'a>(fx: &Fx, slot: Slot, content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    let entrance = fx.entrance(slot);
    let body =
        container(content).padding(Padding { top: entrance.offset, bottom: SLIDE - entrance.offset, ..Padding::ZERO });
    if entrance.opacity >= 0.999 {
        return body.into();
    }
    let veil = container(space::horizontal())
        .width(Length::Fill)
        .height(Length::Fill)
        .style(theme::solid(theme::with_alpha(theme::BG, 1.0 - entrance.opacity), 0.0));
    stack![body, veil].into()
}

// ---------------------------------------------------------------- header --

fn header(app: &App) -> Element<'_, Message> {
    let subtitle = match app.effective_host_name() {
        "" => app.effective_config_dir().to_owned(),
        host => format!("{host}  ·  {}", app.effective_config_dir()),
    };

    row![
        icon_tile(lucide::snowflake(), ACCENT, 22.0, 46.0, 0.6),
        column![text("Nix Manager").size(26).font(BOLD), text(subtitle).size(13).color(MUTED)].spacing(2),
        space::horizontal(),
        connection_pill(app),
        match app.page {
            Page::Dashboard => Element::from(row![refresh_button(app), store_button(), settings_button()].spacing(4)),
            Page::Settings => back_button(Message::CloseSettings),
            Page::Store => back_button(Message::StoreBack),
        },
    ]
    .spacing(14)
    .align_y(Alignment::Center)
    .into()
}

fn connection_pill(app: &App) -> Element<'_, Message> {
    let (label, tint) = match &app.conn {
        Conn::Online(_) => ("Connected", SUCCESS),
        Conn::Connecting => ("Connecting", WARNING),
        Conn::Offline(_) => ("Offline", DANGER),
    };
    let glow = match app.conn {
        Conn::Online(_) if !app.busy() => 0.6,
        Conn::Offline(_) => 0.0,
        _ => app.fx.pulse(),
    };

    let pill = container(
        row![
            container(space::horizontal()).width(8).height(8).style(theme::glowing(tint, 4.0, glow)),
            text(label).size(13).color(tint),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .padding([6, 12])
    .style(theme::chip(tint));

    match &app.conn {
        Conn::Offline(reason) => iced::widget::tooltip(
            pill,
            container(text(reason.clone()).size(12)).padding(8).style(theme::card),
            iced::widget::tooltip::Position::Bottom,
        )
        .into(),
        _ => pill.into(),
    }
}

fn settings_button<'a>() -> Element<'a, Message> {
    button(centered_icon(lucide::settings(), 16.0, MUTED))
        .padding(10)
        .style(ghost_button)
        .on_press(Message::OpenSettings)
        .into()
}

fn store_button<'a>() -> Element<'a, Message> {
    button(centered_icon(lucide::store(), 16.0, MUTED))
        .padding(10)
        .style(ghost_button)
        .on_press(Message::OpenStore)
        .into()
}

fn back_button<'a>(message: Message) -> Element<'a, Message> {
    button(row![lucide::arrow_left().size(14), text("Back").size(13)].spacing(8).align_y(Alignment::Center))
        .padding([8, 14])
        .style(ghost_button)
        .on_press(message)
        .into()
}

fn refresh_button(app: &App) -> Element<'_, Message> {
    let online = matches!(app.conn, Conn::Online(_));
    button(centered_icon(lucide::refresh_cw(), 16.0, MUTED))
        .padding(10)
        .style(ghost_button)
        .on_press_maybe(online.then_some(Message::RefreshStatus))
        .into()
}

pub(crate) fn ghost_button(_: &iced::Theme, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: Some(iced::Background::Color(if hovered { theme::RAISED } else { Color::TRANSPARENT })),
        text_color: TEXT,
        border: iced::Border {
            color: if hovered { theme::BORDER } else { Color::TRANSPARENT },
            width: 1.0,
            radius: 10.0.into(),
        },
        ..Default::default()
    }
}

// -------------------------------------------------------------- overview --

fn overview(app: &App) -> Element<'_, Message> {
    flake_card(app)
}

fn flake_card(app: &App) -> Element<'_, Message> {
    let body: Element<_> = match &app.status {
        None => column![text("Checking flake…").size(18).font(BOLD), text("Reading git history").size(13).color(MUTED)]
            .spacing(4)
            .into(),
        Some(Err(error)) => column![
            text("Status unavailable").size(18).font(BOLD).color(DANGER),
            text(error.clone()).size(13).color(MUTED)
        ]
        .spacing(4)
        .into(),
        Some(Ok(status)) if !status.has_git_history => column![
            text("No git history").size(18).font(BOLD),
            text("The config directory isn't a git repository").size(13).color(MUTED)
        ]
        .spacing(4)
        .into(),
        Some(Ok(status)) => {
            let when = DateTime::parse_from_rfc3339(&status.last_modified_iso8601).ok();
            let headline =
                when.map_or_else(|| "Updated".to_owned(), |t| format!("Updated {}", relative(t.with_timezone(&Utc))));
            let absolute = when.map(|t| t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string());
            let hash: String = status.last_commit_hash.chars().take(7).collect();

            column![
                text(headline).size(18).font(BOLD),
                clipped(text(status.last_commit_subject.clone()).size(13).color(MUTED)),
                row![
                    container(text(hash).size(12).font(Font::MONOSPACE).color(ACCENT))
                        .padding([3, 8])
                        .style(theme::chip(ACCENT)),
                    text(absolute.unwrap_or_default()).size(12).color(MUTED),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            ]
            .spacing(6)
            .width(Length::Fill)
            .into()
        }
    };

    container(
        row![icon_tile(lucide::git_commit_horizontal(), ACCENT, 20.0, 42.0, 0.0), body]
            .spacing(16)
            .align_y(Alignment::Start),
    )
    .padding(20)
    .width(Length::FillPortion(1))
    .height(Length::Fill)
    .style(theme::card)
    .into()
}

// -------------------------------------------------------------- settings --

fn settings(app: &App) -> Element<'_, Message> {
    let dirty = app.settings_dirty();
    let field =
        |icon: Text<'static>, label: &'static str, default: &str, value: &str, on_input: fn(String) -> Message| {
            column![
                row![icon.size(13).color(MUTED), text(label).size(13).color(MUTED)]
                    .spacing(6)
                    .align_y(Alignment::Center),
                text_input(&format!("Default: {default}"), value)
                    .on_input(on_input)
                    .on_submit(Message::ApplySettings)
                    .padding(12)
                    .size(14)
                    .style(theme::input),
                text(format!("Leave empty to use the default ({default}).")).size(12).color(MUTED),
            ]
            .spacing(6)
        };

    let apply = if dirty {
        button(row![lucide::check().size(14), text("Apply").size(14).font(BOLD)].spacing(8).align_y(Alignment::Center))
            .padding([9, 18])
            .style(theme::primary_button)
            .on_press(Message::ApplySettings)
    } else {
        button(
            row![lucide::check().size(14).color(SUCCESS), text("Applied").size(14).color(SUCCESS)]
                .spacing(8)
                .align_y(Alignment::Center),
        )
        .padding([9, 18])
        .style(ghost_button)
    };
    let ghost = |icon: Text<'static>, label: &'static str, message: Message, enabled: bool| {
        let color = if enabled { TEXT } else { theme::with_alpha(MUTED, 0.5) };
        button(
            row![icon.size(13).color(color), text(label).size(13).color(color)].spacing(8).align_y(Alignment::Center),
        )
        .padding([9, 14])
        .style(ghost_button)
        .on_press_maybe(enabled.then_some(message))
    };
    let has_values = !app.draft_config_dir.trim().is_empty() || !app.draft_host_name.trim().is_empty();

    container(
        column![
            row![
                icon_tile(lucide::settings(), ACCENT, 20.0, 42.0, 0.0),
                column![
                    text("Target").size(18).font(BOLD),
                    text("Which flake and host updates and rebuilds operate on. Saved on this machine.")
                        .size(13)
                        .color(MUTED),
                ]
                .spacing(2),
            ]
            .spacing(14)
            .align_y(Alignment::Center),
            field(
                lucide::folder(),
                "Config directory",
                app.default_config_dir.as_deref().unwrap_or("/etc/nixos"),
                &app.draft_config_dir,
                Message::DraftConfigDirChanged
            ),
            field(
                lucide::server(),
                "Host name",
                app.default_host_name.as_deref().unwrap_or(app.effective_host_name()),
                &app.draft_host_name,
                Message::DraftHostNameChanged
            ),
            row![
                apply,
                ghost(lucide::rotate_ccw(), "Revert", Message::RevertDraft, dirty),
                space::horizontal(),
                ghost(lucide::eraser(), "Use defaults", Message::UseDefaults, has_values),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        ]
        .spacing(20),
    )
    .padding(24)
    .width(Length::Fill)
    .style(theme::card)
    .into()
}

// --------------------------------------------------------------- actions --

struct ActionInfo {
    title: &'static str,
    detail: &'static str,
    tint: Color,
    icon: Text<'static>,
}

fn action_info(action: Action) -> ActionInfo {
    match action {
        Action::Update => ActionInfo {
            title: "Update flake",
            detail: "Fetch the latest inputs",
            tint: ACCENT,
            icon: lucide::refresh_cw(),
        },
        Action::Rebuild(RebuildMode::BuildOnly) => {
            ActionInfo { title: "Build", detail: "Build only, don't activate", tint: TEAL, icon: lucide::hammer() }
        }
        Action::Rebuild(RebuildMode::Boot) => {
            ActionInfo { title: "Boot", detail: "Activate on next boot", tint: WARNING, icon: lucide::power() }
        }
        Action::Rebuild(RebuildMode::Test) => ActionInfo {
            title: "Test",
            detail: "Activate, don't persist",
            tint: SUCCESS,
            icon: lucide::flask_conical(),
        },
        Action::Rebuild(_) => {
            ActionInfo { title: "Switch", detail: "Build and activate now", tint: ACCENT_ALT, icon: lucide::zap() }
        }
    }
}

fn actions(app: &App) -> Element<'_, Message> {
    let enabled = matches!(app.conn, Conn::Online(_)) && !app.busy();

    let tiles = Action::ALL.iter().enumerate().map(|(index, action)| {
        let info = action_info(*action);
        let glow = app.fx.hover(index);

        let card = container(
            column![
                icon_tile(info.icon, info.tint, 20.0, 42.0, if enabled { glow } else { 0.0 }),
                column![text(info.title).size(16).font(BOLD), text(info.detail).size(12).color(MUTED)].spacing(2),
            ]
            .spacing(14),
        )
        .padding(18)
        .width(Length::Fill)
        .style(theme::interactive_card(glow, enabled));

        let mut area = mouse_area(card).on_enter(Message::Hover(index, true)).on_exit(Message::Hover(index, false));
        if enabled {
            area = area.on_press(Message::Start(*action)).interaction(mouse::Interaction::Pointer);
        }
        Element::from(area)
    });

    column![text("Actions").size(13).color(MUTED).font(BOLD), row(tiles).spacing(16),].spacing(10).into()
}

// -------------------------------------------------------------- activity --

fn idle_panel<'a>() -> Element<'a, Message> {
    container(
        row![
            centered_icon(lucide::moon(), 18.0, MUTED),
            column![
                text("Nothing running").size(15).font(BOLD),
                text("Pick an action to see live progress here.").size(13).color(MUTED)
            ]
            .spacing(2),
        ]
        .spacing(14)
        .align_y(Alignment::Center),
    )
    .padding(20)
    .width(Length::Fill)
    .style(theme::card)
    .into()
}

fn notice_banner(notice: &str) -> Element<'_, Message> {
    container(
        row![
            lucide::triangle_alert().size(16).color(WARNING),
            text(notice.to_owned()).size(13).width(Length::Fill),
            button(lucide::x().size(14).color(MUTED)).padding(6).style(ghost_button).on_press(Message::DismissNotice),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .padding([10, 16])
    .width(Length::Fill)
    .style(theme::chip(WARNING))
    .into()
}

fn run_title(run: &Run) -> String {
    match (run.kind, run.mode) {
        (OperationKind::UpdateFlake, _) => "Updating flake inputs".to_owned(),
        (_, RebuildMode::BuildOnly) => "Building · not activated".to_owned(),
        (_, RebuildMode::Boot) => "Rebuilding · boot".to_owned(),
        (_, RebuildMode::Test) => "Rebuilding · test".to_owned(),
        _ => "Rebuilding · switch".to_owned(),
    }
}

fn activity<'a>(app: &'a App, run: &'a Run) -> Element<'a, Message> {
    let fx = &app.fx;
    let (tint, state_icon) = match &run.outcome {
        Outcome::Running => (ACCENT, lucide::loader()),
        Outcome::Succeeded => (SUCCESS, lucide::circle_check()),
        Outcome::Failed(_) => (DANGER, lucide::circle_x()),
    };

    let status: Element<_> = match &run.outcome {
        Outcome::Running => row((0..3).map(|i| {
            container(space::horizontal())
                .width(7)
                .height(7)
                .style(theme::solid(theme::with_alpha(ACCENT, fx.dot(i)), 3.5))
                .into()
        }))
        .spacing(5)
        .into(),
        Outcome::Succeeded => {
            container(text("Done").size(12).color(SUCCESS)).padding([4, 10]).style(theme::chip(SUCCESS)).into()
        }
        Outcome::Failed(_) => {
            container(text("Failed").size(12).color(DANGER)).padding([4, 10]).style(theme::chip(DANGER)).into()
        }
    };

    let strength = if run.running() { fx.pulse() } else { 0.6 };
    let head = row![
        icon_tile(state_icon, tint, 20.0, 42.0, strength),
        column![
            text(run_title(run)).size(17).font(BOLD),
            row![
                status_line(&run.message),
                if run.message.is_empty() {
                    Element::from(space::horizontal().width(0))
                } else {
                    copy_button(app, CopyTarget::Message, false)
                },
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        ]
        .spacing(2)
        .width(Length::Fill),
        status,
        if run.running() {
            Element::from(space::horizontal().width(0))
        } else {
            button(row![lucide::x().size(13), text("Dismiss").size(13)].spacing(6).align_y(Alignment::Center))
                .padding([6, 12])
                .style(ghost_button)
                .on_press(Message::DismissRun)
                .into()
        },
    ]
    .spacing(14)
    .align_y(Alignment::Center);

    let mut body = column![head, stepper(run, fx), progress(run, fx, tint)].spacing(18);

    if let Some(chips) = stat_chips(run) {
        body = body.push(chips);
    }
    if !run.items.is_empty() {
        body = body.push(items(app, run, fx));
    }
    body = body.push(log(app, run));

    container(body).padding(22).width(Length::Fill).style(theme::card).into()
}

fn stepper<'a>(run: &'a Run, fx: &Fx) -> Element<'a, Message> {
    let steps = run.steps();
    let current = steps.iter().position(|step| *step == run.phase);

    let mut cells = row![].align_y(Alignment::Center).spacing(10);
    for (index, step) in steps.iter().enumerate() {
        let (tint, icon, glow) = match (&run.outcome, current) {
            (Outcome::Succeeded, _) => (SUCCESS, lucide::check(), 0.0),
            (Outcome::Failed(_), Some(c)) if c == index => (DANGER, lucide::x(), 0.8),
            (_, Some(c)) if index < c => (SUCCESS, lucide::check(), 0.0),
            (Outcome::Running, Some(c)) if index == c => (ACCENT, step_icon(*step), fx.pulse()),
            // Before the first phase event arrives, the first step is next up.
            (Outcome::Running, None) if index == 0 => (ACCENT, step_icon(*step), fx.pulse()),
            _ => (MUTED, step_icon(*step), 0.0),
        };
        let reached = tint != MUTED;

        if index > 0 {
            cells = cells.push(
                container(space::horizontal())
                    .width(Length::Fill)
                    .height(2)
                    .style(theme::solid(if reached { theme::with_alpha(tint, 0.6) } else { theme::BORDER }, 1.0)),
            );
        }
        cells = cells.push(
            row![
                container(icon.size(14).color(tint).center()).width(30).height(30).center(30).style(
                    move |t: &iced::Theme| {
                        let mut style = theme::chip(tint)(t);
                        style.shadow = iced::Shadow {
                            color: theme::with_alpha(tint, 0.5 * glow),
                            offset: iced::Vector::ZERO,
                            blur_radius: 14.0 * glow,
                        };
                        style
                    }
                ),
                text(step_label(*step)).size(13).color(if reached { TEXT } else { MUTED }),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );
    }
    cells.into()
}

fn step_label(phase: EventPhase) -> &'static str {
    match phase {
        EventPhase::FlakeUpdate => "Update inputs",
        other => phase_label(other),
    }
}

fn step_icon(phase: EventPhase) -> Text<'static> {
    match phase {
        EventPhase::FlakeUpdate => lucide::refresh_cw(),
        EventPhase::Build => lucide::hammer(),
        EventPhase::Diff => lucide::git_compare(),
        EventPhase::Register => lucide::bookmark(),
        EventPhase::Activate => lucide::zap(),
        _ => lucide::circle(),
    }
}

/// A bar of the given `height` filled to `value` (0..1), or, when nothing is
/// measurable yet (`None`), with a highlight sweeping across it.
pub(crate) fn bar<'a>(
    value: Option<f32>,
    fx: &Fx,
    tint: Color,
    track_color: Color,
    height: f32,
) -> Element<'a, Message> {
    const SCALE: f32 = 1000.0;
    let portion = |v: f32| Length::FillPortion((v * SCALE).round().max(1.0) as u16);
    let radius = height / 2.0;

    let fill = |color: Color| container(space::horizontal()).height(height).style(theme::glowing(color, radius, 0.5));

    let track: Element<_> = match value {
        Some(value) => {
            let value = value.clamp(0.0, 1.0);
            row![fill(tint).width(portion(value)), space::horizontal().width(portion(1.0 - value))].into()
        }
        None => {
            const SEGMENT: f32 = 0.28;
            let lead = fx.sweep() * (1.0 - SEGMENT);
            row![
                space::horizontal().width(portion(lead)),
                fill(tint).width(portion(SEGMENT)),
                space::horizontal().width(portion(1.0 - SEGMENT - lead))
            ]
            .into()
        }
    };

    container(track).width(Length::Fill).height(height).style(theme::solid(track_color, radius)).into()
}

fn progress<'a>(run: &Run, fx: &Fx, tint: Color) -> Element<'a, Message> {
    let value = (run.fraction.is_some() || !run.running()).then(|| fx.progress());

    let label = match run.fraction {
        Some(_) => format!("{:.0}%", fx.progress() * 100.0),
        None if run.running() => "…".to_owned(),
        None => String::new(),
    };

    row![
        bar(value, fx, tint, theme::RAISED, 8.0),
        text(label).size(13).color(MUTED).font(Font::MONOSPACE).width(44).align_x(iced::alignment::Horizontal::Right),
    ]
    .spacing(14)
    .align_y(Alignment::Center)
    .into()
}

fn stat_chips<'a>(run: &Run) -> Option<Element<'a, Message>> {
    let stats = run.stats.as_ref()?;
    let chip = |icon: Text<'static>, label: String, tint: Color| {
        container(
            row![icon.size(13).color(tint), text(label).size(12).color(TEXT)].spacing(8).align_y(Alignment::Center),
        )
        .padding([5, 10])
        .style(theme::chip(tint))
    };

    let mut chips = row![].spacing(10);
    if stats.builds_expected > 0 {
        chips = chips.push(chip(
            lucide::hammer(),
            format!(
                "Builds {}/{}{}",
                stats.builds_done,
                stats.builds_expected,
                if stats.builds_running > 0 { format!(" · {} running", stats.builds_running) } else { String::new() }
            ),
            ACCENT,
        ));
    }
    if stats.copies_expected > 0 || stats.download_bytes_expected > 0 {
        chips = chips.push(chip(
            lucide::download(),
            format!(
                "Downloads {}/{} · {} / {}{}",
                stats.copies_done,
                stats.copies_expected,
                format_bytes(stats.download_bytes_done as f64),
                format_bytes(stats.download_bytes_expected as f64),
                if stats.download_bytes_per_sec > 0.0 {
                    format!(" · {}/s", format_bytes(stats.download_bytes_per_sec))
                } else {
                    String::new()
                }
            ),
            ACCENT_ALT,
        ));
    }
    if stats.builds_failed > 0 || stats.copies_failed > 0 {
        chips = chips.push(chip(
            lucide::triangle_alert(),
            format!("{} failed", stats.builds_failed + stats.copies_failed),
            DANGER,
        ));
    }
    Some(chips.wrap().into())
}

fn items<'a>(app: &'a App, run: &'a Run, fx: &Fx) -> Element<'a, Message> {
    const SHOWN: usize = 6;
    /// Past this many rows an expanded list scrolls instead of growing.
    const SCROLL_AFTER: usize = 12;

    let running: Vec<&BuildItem> =
        run.items.iter().filter(|item| item.status() == BuildItemStatus::BuildItemRunning).collect();
    let shown = if app.show_all_items { running.len() } else { SHOWN };

    let mut list = column![].spacing(10);
    for item in running.iter().take(shown) {
        let download = item.kind() == BuildItemKind::BuildItemDownload;
        let (icon, tint) = if download { (lucide::download(), ACCENT_ALT) } else { (lucide::hammer(), ACCENT) };
        let detail = if download && item.bytes_expected > 0.0 {
            format!(
                "{} / {}  ·  {}/s",
                format_bytes(item.bytes_done),
                format_bytes(item.bytes_expected),
                format_bytes(item.bytes_per_sec)
            )
        } else if download && item.bytes_done > 0.0 {
            format!("{}  ·  {}/s", format_bytes(item.bytes_done), format_bytes(item.bytes_per_sec))
        } else {
            String::new()
        };
        let mut entry = column![row![
            icon.size(13).color(tint),
            clipped(text(item.name.clone()).size(13).font(Font::MONOSPACE)),
            text(detail).size(12).color(MUTED),
        ]
        .spacing(10)
        .align_y(Alignment::Center)]
        .spacing(5);
        // Builds have nothing to measure; a download of unknown size sweeps.
        if download {
            let value = (item.bytes_expected > 0.0).then(|| (item.bytes_done / item.bytes_expected) as f32);
            entry = entry.push(bar(value, fx, tint, theme::BORDER, 4.0));
        }
        list = list.push(entry);
    }

    let list: Element<_> = if app.show_all_items && running.len() > SCROLL_AFTER {
        scrollable(list.padding([0, 8]).width(Length::Fill)).height(300).into()
    } else {
        list.into()
    };

    let mut body = column![list].spacing(10);
    if running.len() > SHOWN {
        let (chevron, label) = if app.show_all_items {
            (lucide::chevron_down(), "Show fewer".to_owned())
        } else {
            (lucide::chevron_right(), format!("Show all {}", running.len()))
        };
        body = body.push(
            button(row![chevron.size(13), text(label).size(12)].spacing(6).align_y(Alignment::Center))
                .padding([4, 8])
                .style(ghost_button)
                .on_press(Message::ToggleItems),
        );
    }

    container(body).padding([12, 14]).width(Length::Fill).style(theme::well).into()
}

fn log<'a>(app: &'a App, run: &'a Run) -> Element<'a, Message> {
    let toggle = button(
        row![
            if app.show_log { lucide::chevron_down() } else { lucide::chevron_right() }.size(14),
            lucide::terminal().size(14),
            text(format!("Output  ·  {} lines", run.log.len())).size(13),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .padding([6, 10])
    .style(ghost_button)
    .on_press(Message::ToggleLog);

    let header = row![
        toggle,
        space::horizontal(),
        if run.log.is_empty() {
            Element::from(space::horizontal().width(0))
        } else {
            copy_button(app, CopyTarget::Output, true)
        },
    ]
    .align_y(Alignment::Center);

    if !app.show_log {
        return header.into();
    }

    // Virtualised: every row is exactly `LOG_ROW_HEIGHT` tall, so which rows
    // are on screen follows from the scroll position alone. Only those (plus
    // a little overscan) are built and laid out; two spacers stand in for the
    // rest so the scrollbar still reflects the whole log.
    let total = run.log.len();
    let (first, last) = visible_rows(total, app.log_from_bottom);

    let mut rows = column![space::vertical().height(first as f32 * LOG_ROW_HEIGHT)];
    for line in run.log.iter().skip(first).take(last.saturating_sub(first)) {
        let color = if line.is_error { DANGER } else { TEXT };
        rows = rows.push(
            row![
                text(phase_label(line.phase))
                    .size(11)
                    .font(Font::MONOSPACE)
                    .color(theme::with_alpha(MUTED, 0.8))
                    .width(66),
                text(line.text.clone()).size(12).font(Font::MONOSPACE).color(color).wrapping(Wrapping::None),
            ]
            .spacing(10)
            .align_y(Alignment::Center)
            .height(LOG_ROW_HEIGHT),
        );
    }
    rows = rows.push(space::vertical().height(total.saturating_sub(last) as f32 * LOG_ROW_HEIGHT));

    column![
        header,
        container(
            scrollable(rows.padding([0, 4]).width(Length::Fill))
                .anchor_bottom()
                .on_scroll(|viewport| Message::LogScrolled(viewport.absolute_offset().y))
                .height(LOG_VIEW_HEIGHT),
        )
        .padding(8)
        .width(Length::Fill)
        .style(theme::well),
    ]
    .spacing(8)
    .into()
}

/// The range of log rows to build, given how many there are and how far the
/// view is scrolled up from its bottom (px): the visible ones plus overscan.
fn visible_rows(total: usize, from_bottom: f32) -> (usize, usize) {
    let content_height = total as f32 * LOG_ROW_HEIGHT;
    let top = (content_height - LOG_VIEW_HEIGHT - from_bottom).max(0.0);
    let first = ((top / LOG_ROW_HEIGHT) as usize).saturating_sub(LOG_OVERSCAN).min(total);
    let last = (((top + LOG_VIEW_HEIGHT) / LOG_ROW_HEIGHT).ceil() as usize + LOG_OVERSCAN).min(total);
    (first, last)
}

// --------------------------------------------------------------- helpers --

/// One line of text that is cut off at the edge of the space it is given, where
/// plain non-wrapping text would spill over whatever is next to it.
pub(crate) fn clipped<'a>(line: Text<'a>) -> Element<'a, Message> {
    container(line.wrapping(Wrapping::None)).width(Length::Fill).clip(true).into()
}

/// The status of a running operation. Nix messages can be far longer than the
/// window (store paths, flake references), so the line is cut off and hovering
/// it shows everything.
fn status_line(message: &str) -> Element<'_, Message> {
    let line = clipped(text(message.to_owned()).size(13).color(MUTED));
    if message.is_empty() {
        return line;
    }
    iced::widget::tooltip(
        line,
        container(text(message.to_owned()).size(12).wrapping(Wrapping::WordOrGlyph))
            .max_width(640)
            .padding(8)
            .style(theme::card),
        iced::widget::tooltip::Position::Bottom,
    )
    .into()
}

/// A ghost button that copies `target` and turns into a green "Copied" for a
/// moment (driven by the `Fx` flash) as confirmation.
pub(crate) fn copy_button<'a>(app: &App, target: CopyTarget, label: bool) -> Element<'a, Message> {
    let confirmed = app.copied == Some(target) && app.fx.copy_flash() > 0.0;
    let (icon, tint, caption) =
        if confirmed { (lucide::check(), SUCCESS, "Copied") } else { (lucide::copy(), MUTED, "Copy") };

    let mut content = row![icon.size(13).color(tint)].spacing(6).align_y(Alignment::Center);
    if label || confirmed {
        content = content.push(text(caption).size(13).color(tint));
    }
    button(content).padding([4, 8]).style(ghost_button).on_press(Message::Copy(target)).into()
}

pub(crate) fn centered_icon<'a>(icon: Text<'a>, size: f32, color: Color) -> Element<'a, Message> {
    icon.size(size).color(color).into()
}

/// An icon on a softly tinted rounded square.
pub(crate) fn icon_tile<'a>(
    icon: Text<'a>,
    tint: Color,
    icon_size: f32,
    side: f32,
    strength: f32,
) -> Element<'a, Message> {
    container(icon.size(icon_size).color(tint))
        .width(side)
        .height(side)
        .center(side)
        .style(theme::icon_tile(tint, strength))
        .into()
}

fn relative(then: DateTime<Utc>) -> String {
    let seconds = (Utc::now() - then).num_seconds().max(0);
    let (value, unit) = match seconds {
        0..=59 => return "just now".to_owned(),
        60..=3599 => (seconds / 60, "minute"),
        3600..=86_399 => (seconds / 3600, "hour"),
        86_400..=2_591_999 => (seconds / 86_400, "day"),
        2_592_000..=31_535_999 => (seconds / 2_592_000, "month"),
        _ => (seconds / 31_536_000, "year"),
    };
    format!("{value} {unit}{} ago", if value == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows that fit the viewport.
    const VISIBLE: usize = (LOG_VIEW_HEIGHT / LOG_ROW_HEIGHT) as usize + 1;

    #[test]
    fn short_logs_are_drawn_whole() {
        assert_eq!(visible_rows(0, 0.0), (0, 0));
        assert_eq!(visible_rows(5, 0.0), (0, 5));
    }

    #[test]
    fn following_the_bottom_draws_only_the_tail() {
        let (first, last) = visible_rows(2000, 0.0);
        assert_eq!(last, 2000);
        assert!(first >= 2000 - VISIBLE - LOG_OVERSCAN - 1, "first = {first}");
        assert!(last - first <= VISIBLE + 2 * LOG_OVERSCAN + 1);
    }

    #[test]
    fn scrolled_to_the_top_draws_only_the_head() {
        let from_bottom = 2000.0 * LOG_ROW_HEIGHT - LOG_VIEW_HEIGHT;
        let (first, last) = visible_rows(2000, from_bottom);
        assert_eq!(first, 0);
        assert!((VISIBLE..=VISIBLE + LOG_OVERSCAN + 1).contains(&last), "last = {last}");
    }

    #[test]
    fn mid_scroll_window_covers_the_viewport() {
        // 100 rows scrolled so row 50 is at the top of the viewport.
        let top_row = 50.0;
        let from_bottom = 100.0 * LOG_ROW_HEIGHT - LOG_VIEW_HEIGHT - top_row * LOG_ROW_HEIGHT;
        let (first, last) = visible_rows(100, from_bottom);
        assert!(first <= 50 && last >= 50 + VISIBLE - 1, "({first}, {last})");
        assert!(last - first <= VISIBLE + 2 * LOG_OVERSCAN + 1);
    }
}
