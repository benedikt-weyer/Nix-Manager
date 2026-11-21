//! The boot page: which bootloader the system installs, how full its partition
//! is, what is on it and which of that the boot menu still uses, and pruning
//! old generations. It speaks for r-boot, systemd-boot and GRUB alike: the
//! backend turns each of them into the same entries, files and generations.

use std::collections::HashSet;

use chrono::{Local, TimeZone};
use iced::widget::text::Wrapping;
use iced::widget::{button, column, container, row, space, text, tooltip};
use iced::{Alignment, Background, Border, Color, Element, Length};
use iced_fonts::lucide;
use nix_backend_client::{format_bytes, BootFile, BootFileState, BootGeneration, BootInfo, BootScan, Bootloader};

use crate::store::{card_with, failure, notice, working, Banner, Load};
use crate::theme::{self, ACCENT, DANGER, MUTED, SUCCESS, TEAL, TEXT, WARNING};
use crate::view::{bar, centered_icon, clipped, ghost_button, icon_tile, BOLD};
use crate::{App, Conn, Message};

/// How many files the list shows before it says how many more there are.
const MAX_FILE_ROWS: usize = 60;

/// Fuller than this, a rebuild is likely to fail copying a new kernel.
const TIGHT: f32 = 0.9;

/// What the user has asked for and still has to confirm.
#[derive(Clone, Debug, PartialEq)]
pub enum Ask {
    /// Delete every unused file on the partition.
    Clean,
    /// Delete the generations outside the newest `keep`.
    Prune,
    /// Delete one generation.
    Generation(u64),
}

pub struct Boot {
    pub info: Load<BootInfo>,
    /// Idle until the user asks: it needs authorization.
    pub scan: Load<BootScan>,
    /// The info is being fetched again; what is shown stays until it arrives.
    pub refreshing: bool,
    /// Files or generations are being deleted.
    pub working: bool,
    pub banner: Option<Banner>,
    /// What the last maintenance printed.
    pub output: Option<String>,
    /// How many of the newest generations pruning keeps.
    pub keep: usize,
    keep_touched: bool,
    pub ask: Option<Ask>,
}

impl Boot {
    pub fn new() -> Self {
        Self {
            info: Load::Idle,
            scan: Load::Idle,
            refreshing: false,
            working: false,
            banner: None,
            output: None,
            keep: 5,
            keep_touched: false,
            ask: None,
        }
    }

    pub fn busy(&self) -> bool {
        self.info.loading() || self.scan.loading() || self.working
    }

    /// Takes in new info, and unless the user picked a number, suggests keeping
    /// as many generations as the boot menu shows anyway.
    pub fn set_info(&mut self, info: BootInfo) {
        if !self.keep_touched {
            self.keep = info.menu_size().unwrap_or(5).clamp(1, info.generations.len().max(1));
        }
        self.info = Load::Ready(info);
        self.refreshing = false;
    }

    pub fn set_keep(&mut self, keep: usize) {
        let most = self.info.ready().map_or(1, |info| info.generations.len().max(1));
        self.keep = keep.clamp(1, most);
        self.keep_touched = true;
        // What was asked for no longer matches the number on screen.
        if self.ask == Some(Ask::Prune) {
            self.ask = None;
        }
    }

    /// The generations pruning would delete.
    pub fn prunable(&self) -> Vec<u64> {
        self.info.ready().map(|info| info.prunable(self.keep)).unwrap_or_default()
    }

    /// The generations that have an entry in the boot menu, once the partition
    /// has been scanned.
    fn in_menu(&self) -> Option<HashSet<u64>> {
        self.scan.ready().map(|scan| scan.entries.iter().map(|entry| entry.generation).collect())
    }
}

// ------------------------------------------------------------------ views --

pub fn page(app: &App) -> Element<'_, Message> {
    let boot = &app.boot;
    let body: Element<_> = if !matches!(app.conn, Conn::Online(_)) {
        notice(
            lucide::unplug(),
            MUTED,
            "Backend offline",
            "The boot partition is read through the backend. It reconnects automatically.",
        )
    } else {
        match &boot.info {
            Load::Idle | Load::Loading => {
                working(&app.fx, "Reading the bootloader…".to_owned(), "Looking at the partition and the generations.")
            }
            Load::Failed(error) => column![
                failure("Could not read the boot setup", error),
                button(
                    row![lucide::rotate_ccw().size(13), text("Try again").size(13)]
                        .spacing(8)
                        .align_y(Alignment::Center)
                )
                .padding([6, 12])
                .style(ghost_button)
                .on_press(Message::BootReload),
            ]
            .spacing(10)
            .into(),
            Load::Ready(info) => overview(app, info),
        }
    };

    let mut page = column![].spacing(16);
    if let Some(banner) = &boot.banner {
        page = page.push(banner_view(banner));
    }
    if let Some(output) = boot.output.as_ref().filter(|output| !output.trim().is_empty()) {
        page = page.push(output_view(output));
    }
    page.push(body).into()
}

fn overview<'a>(app: &'a App, info: &'a BootInfo) -> Element<'a, Message> {
    let boot = &app.boot;
    let mut page = column![partition_card(app, info)].spacing(16);
    if boot.working {
        page = page.push(working(
            &app.fx,
            "Updating the boot partition…".to_owned(),
            "Waiting for authorization, then deleting and rewriting the boot menu.",
        ));
    }
    page.push(menu_card(app, info)).push(generations_card(app, info)).into()
}

// -------------------------------------------------------------- partition --

fn partition_card<'a>(app: &'a App, info: &'a BootInfo) -> Element<'a, Message> {
    let loader = info.loader();
    let location = if info.is_mount {
        format!("{}  ·  {} on {}", info.mount_point, info.fs_type, info.device)
    } else {
        format!("{}  ·  not a separate partition", info.mount_point)
    };

    let head = row![
        icon_tile(lucide::hard_drive(), ACCENT, 20.0, 42.0, 0.0),
        column![text(loader.label()).size(18).font(BOLD), clipped(text(location).size(13).color(MUTED))]
            .spacing(2)
            .width(Length::Fill),
        button(
            row![
                lucide::refresh_cw().size(13),
                text(if app.boot.refreshing { "Refreshing" } else { "Refresh" }).size(13)
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .padding([8, 14])
        .style(ghost_button)
        .on_press_maybe((!app.boot.refreshing).then_some(Message::BootReload)),
    ]
    .spacing(14)
    .align_y(Alignment::Center);

    let mut body = column![head].spacing(14);

    if info.has_usage() {
        let fraction = info.used_fraction();
        let tint = if fraction >= TIGHT {
            DANGER
        } else if fraction >= 0.75 {
            WARNING
        } else {
            SUCCESS
        };
        body = body.push(
            column![
                row![
                    text(format!(
                        "{} used of {}",
                        format_bytes(info.used_bytes as f64),
                        format_bytes(info.total_bytes as f64)
                    ))
                    .size(14)
                    .font(BOLD),
                    space::horizontal(),
                    text(format!("{} free  ·  {:.0}%", format_bytes(info.free_bytes as f64), fraction * 100.0))
                        .size(13)
                        .color(tint),
                ]
                .align_y(Alignment::Center),
                bar(Some(fraction), &app.fx, tint, theme::RAISED, 8.0),
            ]
            .spacing(8),
        );
        if fraction >= TIGHT {
            body = body.push(
                container(
                    row![
                        lucide::triangle_alert().size(15).color(DANGER),
                        text(
                            "The partition is nearly full. A rebuild copies the new kernel and initrd before it \
                             removes old ones, so it can fail with “No space left on device”. Free some space below."
                        )
                        .size(13)
                        .width(Length::Fill),
                    ]
                    .spacing(12)
                    .align_y(Alignment::Center),
                )
                .padding([10, 14])
                .width(Length::Fill)
                .style(theme::chip(DANGER)),
            );
        }
    }

    let mut facts = row![].spacing(8).align_y(Alignment::Center);
    if let Some(size) = info.menu_size() {
        facts = facts.push(chip(
            lucide::list_checks(),
            format!("menu keeps {size} generations"),
            TEAL,
            "The bootloader's configurationLimit: the current generation plus this many minus one",
        ));
    }
    if loader == Bootloader::Unknown {
        facts = facts.push(chip(
            lucide::info(),
            "bootloader not recognised".to_owned(),
            WARNING,
            "Only r-boot, systemd-boot and GRUB are understood. Generations can still be listed.",
        ));
    }
    if !info.listable {
        facts = facts.push(chip(
            lucide::shield_alert(),
            "needs authorization to read".to_owned(),
            MUTED,
            "Only root can read the partition, so scanning it asks for authorization",
        ));
    }
    body = body.push(facts);

    card_with(body.into(), 20)
}

// -------------------------------------------------------------- boot menu --

fn menu_card<'a>(app: &'a App, info: &'a BootInfo) -> Element<'a, Message> {
    let boot = &app.boot;
    let scanning = boot.scan.loading();
    let scan_label = match &boot.scan {
        Load::Ready(_) => "Scan again",
        _ => "Scan partition",
    };
    let scan_button =
        button(row![lucide::scan_search().size(13), text(scan_label).size(13)].spacing(8).align_y(Alignment::Center))
            .padding([8, 14])
            .style(if matches!(boot.scan, Load::Idle) { theme::primary_button } else { ghost_button })
            .on_press_maybe((!scanning && !boot.working).then_some(Message::BootScan));

    let header = row![
        lucide::list_tree().size(15).color(ACCENT),
        text("Boot menu and files").size(15).font(BOLD),
        space::horizontal(),
        scan_button,
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    let content: Element<_> = match &boot.scan {
        Load::Idle => text(if info.listable {
            "Scan the partition to see which menu entries exist and which kernels and initrds nothing uses any more."
        } else {
            "The partition can only be read by root. Scanning asks for authorization once, and lists the menu \
             entries and which kernels and initrds nothing uses any more."
        })
        .size(13)
        .color(MUTED)
        .into(),
        Load::Loading => working(&app.fx, "Reading the partition…".to_owned(), "Waiting for authorization if asked."),
        Load::Failed(error) => failure("Could not read the partition", error),
        Load::Ready(scan) => scanned(app, scan),
    };

    card_with(column![header, content].spacing(14).into(), 20)
}

fn scanned<'a>(app: &'a App, scan: &'a BootScan) -> Element<'a, Message> {
    let mut list = column![].spacing(14);

    // The entries the loader would show.
    let mut entries = column![].spacing(2);
    for entry in &scan.entries {
        let mut label = row![clipped(text(entry.id.clone()).size(14))].spacing(8).align_y(Alignment::Center);
        if entry.generation > 0 {
            label = label.push(text(format!("generation {}", entry.generation)).size(12).color(MUTED));
        }
        if entry.is_default {
            label = label.push(small_chip("default", ACCENT));
        }
        entries = entries.push(container(label).padding([5, 8]));
    }
    if scan.entries.is_empty() {
        entries = entries.push(
            container(
                text(
                    "No menu entries were found, so the boot menu could not be read. Nothing is called unused \
                     until it can be.",
                )
                .size(13)
                .color(WARNING),
            )
            .padding([5, 8]),
        );
    }
    list = list.push(
        column![text(format!("Menu entries · {}", scan.entries.len())).size(13).font(BOLD).color(MUTED), entries]
            .spacing(4),
    );

    // What is on the partition.
    let mut files = column![].spacing(2);
    for file in scan.files.iter().take(MAX_FILE_ROWS) {
        files = files.push(file_row(file));
    }
    if scan.files.len() > MAX_FILE_ROWS {
        files = files.push(
            container(text(format!("and {} more", scan.files.len() - MAX_FILE_ROWS)).size(12).color(MUTED))
                .padding([5, 8]),
        );
    }
    list = list.push(
        column![text(format!("Files · {}", scan.files.len())).size(13).font(BOLD).color(MUTED), files].spacing(4),
    );

    // Making room.
    let junk = scan.junk().count();
    let footer: Element<_> = if junk == 0 {
        row![
            lucide::circle_check().size(15).color(SUCCESS),
            text("Every kernel and initrd on the partition is used by the boot menu.").size(13).color(MUTED),
        ]
        .spacing(10)
        .align_y(Alignment::Center)
        .into()
    } else if app.boot.ask == Some(Ask::Clean) {
        confirm_bar(
            format!(
                "Delete {junk} unused file{} ({})? They are not part of any menu entry.",
                plural(junk),
                format_bytes(scan.junk_bytes() as f64)
            ),
            "Delete files",
        )
    } else {
        row![
            lucide::eraser().size(15).color(WARNING),
            text(format!(
                "{junk} file{} ({}) are not used by any menu entry",
                plural(junk),
                format_bytes(scan.junk_bytes() as f64)
            ))
            .size(13)
            .width(Length::Fill),
            button(
                row![lucide::trash().size(13), text("Delete unused files").size(13)]
                    .spacing(8)
                    .align_y(Alignment::Center)
            )
            .padding([7, 12])
            .style(theme::primary_button)
            .on_press_maybe((!app.boot.working).then_some(Message::BootAsk(Ask::Clean))),
        ]
        .spacing(10)
        .align_y(Alignment::Center)
        .into()
    };
    list.push(footer).into()
}

fn file_row(file: &BootFile) -> Element<'_, Message> {
    let (label, tint) = match file.file_state() {
        BootFileState::InUse => ("in use", SUCCESS),
        BootFileState::Stale => ("unused", WARNING),
        BootFileState::Partial => ("partial", DANGER),
        BootFileState::Other => ("", MUTED),
    };
    let tag: Element<_> = if label.is_empty() {
        space::horizontal().width(64).into()
    } else {
        container(small_chip(label, tint)).width(64).into()
    };
    let dim = if matches!(file.file_state(), BootFileState::Other) { MUTED } else { TEXT };
    container(
        row![
            tag,
            clipped(text(file.path.clone()).size(13).color(dim).font(iced::Font::MONOSPACE)),
            text(format_bytes(file.size as f64)).size(12).color(MUTED),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .padding([4, 8])
    .into()
}

// ------------------------------------------------------------ generations --

fn generations_card<'a>(app: &'a App, info: &'a BootInfo) -> Element<'a, Message> {
    let boot = &app.boot;
    let in_menu = boot.in_menu();

    let header = row![
        lucide::history().size(15).color(ACCENT),
        text(format!("Generations · {}", info.generations.len())).size(15).font(BOLD),
        space::horizontal(),
        tooltip(
            button(
                row![lucide::power().size(13), text("Reinstall bootloader").size(13)]
                    .spacing(8)
                    .align_y(Alignment::Center)
            )
            .padding([8, 14])
            .style(ghost_button)
            .on_press_maybe((!boot.working).then_some(Message::BootReinstall)),
            container(
                text("Runs the installer again, which rewrites the boot menu from the generations that exist").size(12)
            )
            .padding(8)
            .style(theme::card),
            tooltip::Position::Left,
        ),
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    let mut list = column![].spacing(2);
    for generation in &info.generations {
        list = list.push(generation_row(app, generation, in_menu.as_ref()));
    }
    if info.generations.is_empty() {
        list = list.push(container(text("No system generations were found.").size(13).color(MUTED)).padding([6, 8]));
    }

    let doomed = boot.prunable().len();
    let stepper = row![
        text("Keep the newest").size(13).color(MUTED),
        button(centered_icon(lucide::minus(), 13.0, MUTED))
            .padding(6)
            .style(ghost_button)
            .on_press_maybe((boot.keep > 1).then_some(Message::BootKeep(boot.keep - 1))),
        text(boot.keep.to_string()).size(15).font(BOLD),
        button(centered_icon(lucide::plus(), 13.0, MUTED))
            .padding(6)
            .style(ghost_button)
            .on_press_maybe((boot.keep < info.generations.len()).then_some(Message::BootKeep(boot.keep + 1))),
        text("generations").size(13).color(MUTED),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let footer: Element<_> = if boot.ask == Some(Ask::Prune) {
        confirm_bar(
            format!(
                "Delete {doomed} older generation{} and rewrite the boot menu? Their store paths stay until the next \
                 garbage collection.",
                plural(doomed)
            ),
            "Delete generations",
        )
    } else {
        row![
            stepper,
            space::horizontal(),
            button(
                row![
                    lucide::trash().size(13),
                    text(if doomed == 0 {
                        "Nothing to prune".to_owned()
                    } else {
                        format!("Prune {doomed} older generation{}", plural(doomed))
                    })
                    .size(13)
                ]
                .spacing(8)
                .align_y(Alignment::Center)
            )
            .padding([8, 14])
            .style(theme::primary_button)
            .on_press_maybe((doomed > 0 && !boot.working).then_some(Message::BootAsk(Ask::Prune))),
        ]
        .align_y(Alignment::Center)
        .into()
    };

    card_with(
        column![
            header,
            text(
                "Each generation keeps its own kernel and initrd on the partition. Deleting one removes its menu \
                 entry; the current and the booted generation always stay."
            )
            .size(12)
            .color(MUTED),
            list,
            footer
        ]
        .spacing(12)
        .into(),
        20,
    )
}

fn generation_row<'a>(
    app: &'a App,
    generation: &'a BootGeneration,
    in_menu: Option<&HashSet<u64>>,
) -> Element<'a, Message> {
    let boot = &app.boot;
    let created = Local
        .timestamp_opt(generation.created_unix, 0)
        .single()
        .map(|time| time.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default();

    let mut chips = row![].spacing(6).align_y(Alignment::Center);
    if generation.current {
        chips = chips.push(small_chip("current", ACCENT));
    }
    if generation.booted {
        chips = chips.push(small_chip("booted", SUCCESS));
    }
    if let Some(menu) = in_menu {
        if menu.contains(&generation.number) {
            chips = chips.push(small_chip("in menu", TEAL));
        } else {
            chips = chips.push(small_chip("not in menu", MUTED));
        }
    }

    let label = column![
        row![text(generation.number.to_string()).size(14).font(BOLD), clipped(text(generation.label.clone()).size(13))]
            .spacing(10)
            .align_y(Alignment::Center),
        text(format!("{created}  ·  {}", format_bytes(generation.boot_bytes as f64))).size(11).color(MUTED),
    ]
    .spacing(1)
    .width(Length::Fill);

    let asked = boot.ask == Some(Ask::Generation(generation.number));
    let remove: Element<_> = if generation.is_protected() {
        space::horizontal().width(34).into()
    } else {
        tooltip(
            button(centered_icon(lucide::trash(), 13.0, if asked { DANGER } else { MUTED }))
                .padding(7)
                .style(ghost_button)
                .on_press_maybe((!boot.working).then(|| Message::BootAsk(Ask::Generation(generation.number)))),
            container(text("Delete this generation").size(12)).padding(6).style(theme::card),
            tooltip::Position::Left,
        )
        .into()
    };

    let line = container(row![label, chips, remove].spacing(10).align_y(Alignment::Center)).padding([5, 8]);
    if asked {
        column![
            line,
            confirm_bar(
                format!("Delete generation {} and rewrite the boot menu?", generation.number),
                "Delete generation"
            )
        ]
        .spacing(4)
        .into()
    } else {
        line.into()
    }
}

// ---------------------------------------------------------------- pieces --

fn confirm_bar<'a>(question: String, action: &'static str) -> Element<'a, Message> {
    container(
        row![
            lucide::triangle_alert().size(15).color(WARNING),
            text(question).size(13).width(Length::Fill),
            button(text("Cancel").size(13)).padding([6, 12]).style(ghost_button).on_press(Message::BootCancelAsk),
            button(text(action).size(13)).padding([6, 12]).style(danger_button).on_press(Message::BootConfirm),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .padding([10, 14])
    .width(Length::Fill)
    .style(theme::chip(WARNING))
    .into()
}

fn danger_button(_: &iced::Theme, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: Some(Background::Color(if hovered { theme::mix(DANGER, Color::WHITE, 0.12) } else { DANGER })),
        text_color: theme::BG,
        border: Border { radius: 10.0.into(), ..Default::default() },
        ..Default::default()
    }
}

fn chip<'a>(icon: iced::widget::Text<'a>, label: String, tint: Color, hint: &'static str) -> Element<'a, Message> {
    tooltip(
        container(
            row![icon.size(12).color(tint), text(label).size(12).color(tint)].spacing(6).align_y(Alignment::Center),
        )
        .padding([4, 10])
        .style(theme::chip(tint)),
        container(text(hint).size(12).wrapping(Wrapping::WordOrGlyph)).max_width(360).padding(8).style(theme::card),
        tooltip::Position::Bottom,
    )
    .into()
}

fn small_chip<'a>(label: &'static str, tint: Color) -> Element<'a, Message> {
    container(text(label).size(11).color(tint)).padding([2, 8]).style(theme::chip(tint)).into()
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
                .on_press(Message::BootDismissBanner),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .padding([10, 16])
    .width(Length::Fill)
    .style(theme::chip(tint))
    .into()
}

/// What the privileged steps printed, which is where a failure explains itself.
fn output_view(output: &str) -> Element<'_, Message> {
    container(
        text(output.trim().to_owned())
            .size(12)
            .font(iced::Font::MONOSPACE)
            .color(MUTED)
            .wrapping(Wrapping::WordOrGlyph),
    )
    .padding(12)
    .width(Length::Fill)
    .style(theme::well)
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

    fn generation(number: u64, current: bool, booted: bool) -> BootGeneration {
        BootGeneration { number, current, booted, ..Default::default() }
    }

    fn info(limit: u32, generations: Vec<BootGeneration>) -> BootInfo {
        BootInfo { generation_limit: limit, generations, ..Default::default() }
    }

    #[test]
    fn suggests_the_size_of_the_boot_menu() {
        let mut boot = Boot::new();
        boot.set_info(info(6, (1..=20).rev().map(|n| generation(n, n == 20, false)).collect()));
        assert_eq!(boot.keep, 7);
        // Never more than there are.
        boot.keep_touched = false;
        boot.set_info(info(6, vec![generation(2, true, false), generation(1, false, false)]));
        assert_eq!(boot.keep, 2);
    }

    #[test]
    fn a_chosen_number_survives_a_refresh() {
        let mut boot = Boot::new();
        boot.set_info(info(6, (1..=20).rev().map(|n| generation(n, n == 20, false)).collect()));
        boot.set_keep(3);
        boot.set_info(info(6, (1..=21).rev().map(|n| generation(n, n == 21, false)).collect()));
        assert_eq!(boot.keep, 3);
        assert_eq!(boot.prunable().len(), 18);
    }

    #[test]
    fn changing_the_number_withdraws_a_pending_prune() {
        let mut boot = Boot::new();
        boot.set_info(info(6, (1..=10).rev().map(|n| generation(n, n == 10, false)).collect()));
        boot.ask = Some(Ask::Prune);
        boot.set_keep(4);
        assert_eq!(boot.ask, None);
        boot.ask = Some(Ask::Generation(3));
        boot.set_keep(5);
        assert_eq!(boot.ask, Some(Ask::Generation(3)));
    }

    #[test]
    fn the_current_and_booted_generation_are_never_pruned() {
        let mut boot = Boot::new();
        // The booted one is old, outside the newest two.
        let generations = vec![
            generation(5, true, false),
            generation(4, false, false),
            generation(3, false, false),
            generation(2, false, true),
            generation(1, false, false),
        ];
        boot.set_info(info(0, generations));
        boot.set_keep(2);
        assert_eq!(boot.prunable(), vec![3, 1]);
    }
}
