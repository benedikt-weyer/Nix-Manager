//! The store page: browse and search the packages the flake's nixpkgs offers,
//! see what is installed, and read about any one of them.

use std::collections::HashSet;

use iced::widget::{button, column, container, row, space, text, text::Wrapping, text_input, Text};
use iced::{Alignment, Background, Border, Color, Element, Font, Length};
use iced_fonts::lucide;
use nix_backend_client::{Maintainer, Package, SearchPackagesReply};

use crate::theme::{self, ACCENT, ACCENT_ALT, DANGER, MUTED, SUCCESS, TEXT};
use crate::view::{bar, centered_icon, copy_button, ghost_button, icon_tile, BOLD};
use crate::{App, Conn, CopyTarget, Message};

/// The id of the search box, so opening the page can put the cursor in it.
pub const SEARCH_ID: &str = "store-search";

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tab {
    Browse,
    Installed,
}

/// Something fetched from the backend.
pub enum Load<T> {
    /// Not asked for (yet).
    Idle,
    Loading,
    Ready(T),
    Failed(String),
}

impl<T> Load<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Load::Ready(value) => Some(value),
            _ => None,
        }
    }

    pub fn loading(&self) -> bool {
        matches!(self, Load::Loading)
    }

    pub fn from_result(result: Result<T, String>) -> Self {
        match result {
            Ok(value) => Load::Ready(value),
            Err(error) => Load::Failed(error),
        }
    }
}

/// The package whose page is open.
pub struct Detail {
    /// What to call it while its details are still on their way.
    pub title: String,
    /// Its nixpkgs attribute; empty for an installed package, whose details
    /// came with the list.
    pub attr: String,
    pub package: Load<Package>,
}

pub struct Store {
    pub tab: Tab,
    /// What the search box says; searched for on Enter.
    pub query: String,
    /// What the shown results are for.
    pub searched: String,
    pub results: Load<SearchPackagesReply>,
    /// Counts searches, so that the answer to an old one is recognised and dropped.
    pub search_seq: u64,
    pub installed: Load<Vec<Package>>,
    installed_names: HashSet<String>,
    /// What the filter box on the installed tab says.
    pub filter: String,
    pub detail: Option<Detail>,
}

impl Store {
    pub fn new() -> Self {
        Self {
            tab: Tab::Browse,
            query: String::new(),
            searched: String::new(),
            results: Load::Idle,
            search_seq: 0,
            installed: Load::Idle,
            installed_names: HashSet::new(),
            filter: String::new(),
            detail: None,
        }
    }

    /// Forgets everything that came from the previous target, keeping what the
    /// user typed.
    pub fn reset(&mut self) {
        *self = Self { tab: self.tab, query: std::mem::take(&mut self.query), ..Self::new() };
    }

    pub fn set_installed(&mut self, result: Result<Vec<Package>, String>) {
        self.installed_names = match &result {
            Ok(packages) => packages.iter().map(|package| package.pname.clone()).filter(|n| !n.is_empty()).collect(),
            Err(_) => HashSet::new(),
        };
        self.installed = Load::from_result(result);
    }

    pub fn is_installed(&self, pname: &str) -> bool {
        self.installed_names.contains(pname)
    }

    /// Whether anything on this page is waiting for the backend.
    pub fn busy(&self) -> bool {
        self.results.loading()
            || self.installed.loading()
            || self.detail.as_ref().is_some_and(|detail| detail.package.loading())
    }

    /// The installed packages the filter lets through, with their position in
    /// the full list.
    fn filtered(&self) -> Vec<(usize, &Package)> {
        let needle = self.filter.trim().to_lowercase();
        let words: Vec<&str> = needle.split_whitespace().collect();
        self.installed
            .ready()
            .into_iter()
            .flatten()
            .enumerate()
            .filter(|(_, package)| {
                let haystack = format!("{} {} {}", package.name, package.pname, package.description).to_lowercase();
                words.iter().all(|word| haystack.contains(word))
            })
            .collect()
    }
}

// ------------------------------------------------------------------ views --

pub fn page(app: &App) -> Element<'_, Message> {
    match &app.store.detail {
        Some(detail) => detail_page(app, detail),
        None => list_page(app),
    }
}

fn list_page(app: &App) -> Element<'_, Message> {
    let store = &app.store;
    let installed_count = store.installed.ready().map(Vec::len);

    let tabs = row![
        tab_button("Browse".to_owned(), lucide::compass(), store.tab == Tab::Browse, Tab::Browse),
        tab_button(
            match installed_count {
                Some(count) => format!("Installed · {count}"),
                None => "Installed".to_owned(),
            },
            lucide::circle_check(),
            store.tab == Tab::Installed,
            Tab::Installed,
        ),
    ]
    .spacing(6);

    let online = matches!(app.conn, Conn::Online(_));
    let search: Element<_> = match store.tab {
        Tab::Browse => {
            let can_search = online && !store.query.trim().is_empty() && !store.results.loading();
            row![
                text_input("Search packages, e.g. firefox, ripgrep, \"video editor\"", &store.query)
                    .id(SEARCH_ID)
                    .on_input(Message::StoreQuery)
                    .on_submit_maybe(can_search.then_some(Message::StoreSearch))
                    .padding(12)
                    .size(14)
                    .style(theme::input),
                button(row![lucide::search().size(14), text("Search").size(14)].spacing(8).align_y(Alignment::Center))
                    .padding([12, 18])
                    .style(theme::primary_button)
                    .on_press_maybe(can_search.then_some(Message::StoreSearch)),
            ]
            .spacing(10)
            .into()
        }
        Tab::Installed => text_input("Filter installed packages", &store.filter)
            .id(SEARCH_ID)
            .on_input(Message::StoreFilter)
            .padding(12)
            .size(14)
            .style(theme::input)
            .into(),
    };

    let body = if !online {
        notice(
            lucide::unplug(),
            MUTED,
            "Backend offline",
            "The store needs the backend to search nixpkgs. It reconnects automatically.",
        )
    } else {
        match store.tab {
            Tab::Browse => browse_body(app),
            Tab::Installed => installed_body(app),
        }
    };

    column![tabs, search, body].spacing(16).into()
}

fn browse_body(app: &App) -> Element<'_, Message> {
    let store = &app.store;
    match &store.results {
        Load::Idle => notice(
            lucide::package_search(),
            ACCENT,
            "Find a program",
            "Search the nixpkgs your flake has locked. Everything listed here is what a rebuild would install.",
        ),
        Load::Loading => working(
            &app.fx,
            format!("Searching for \"{}\"…", store.searched),
            "The first search after nixpkgs changes evaluates all of it and can take a minute. Later ones are fast.",
        ),
        Load::Failed(error) => failure("Search failed", error),
        Load::Ready(reply) if reply.packages.is_empty() => notice(
            lucide::search_x(),
            MUTED,
            "No matches",
            &format!("Nothing in {} matches \"{}\".", reply.source, store.searched),
        ),
        Load::Ready(reply) => {
            let shown = reply.packages.len();
            let total = reply.total_matches as usize;
            let summary = if total > shown {
                format!("Top {shown} of {total} matches in {} — refine the search to narrow it down", reply.source)
            } else {
                format!("{total} match{} in {}", if total == 1 { "" } else { "es" }, reply.source)
            };

            let mut list = column![].spacing(2);
            for (index, package) in reply.packages.iter().enumerate() {
                let title = if package.attr.contains('.') || package.pname.is_empty() {
                    package.attr.clone()
                } else {
                    package.pname.clone()
                };
                list = list.push(entry(
                    title,
                    package.version.clone(),
                    package.description.clone(),
                    store.is_installed(&package.pname),
                    Message::OpenResult(index),
                ));
            }
            column![text(summary).size(13).color(MUTED), card(list.into())].spacing(10).into()
        }
    }
}

fn installed_body(app: &App) -> Element<'_, Message> {
    let store = &app.store;
    match &store.installed {
        Load::Idle | Load::Loading => working(
            &app.fx,
            "Reading your configuration…".to_owned(),
            "Evaluating the configuration to see which packages it installs.",
        ),
        Load::Failed(error) => column![
            failure("Could not list installed packages", error),
            button(
                row![lucide::rotate_ccw().size(13), text("Try again").size(13)].spacing(8).align_y(Alignment::Center)
            )
            .padding([6, 12])
            .style(ghost_button)
            .on_press(Message::ReloadInstalled),
        ]
        .spacing(10)
        .into(),
        Load::Ready(packages) if packages.is_empty() => {
            notice(lucide::package_open(), MUTED, "Nothing installed", "This configuration installs no packages.")
        }
        Load::Ready(packages) => {
            let shown = store.filtered();
            if shown.is_empty() {
                return notice(lucide::search_x(), MUTED, "No matches", "None of the installed packages match.");
            }
            let mut list = column![].spacing(2);
            for (index, package) in &shown {
                let title = if package.pname.is_empty() { package.name.clone() } else { package.pname.clone() };
                list = list.push(entry(
                    title,
                    package.version.clone(),
                    package.description.clone(),
                    false,
                    Message::OpenInstalled(*index),
                ));
            }
            let summary = if shown.len() == packages.len() {
                format!("{} packages installed by this configuration", packages.len())
            } else {
                format!("{} of {} installed packages", shown.len(), packages.len())
            };
            column![text(summary).size(13).color(MUTED), card(list.into())].spacing(10).into()
        }
    }
}

fn detail_page<'a>(app: &'a App, detail: &'a Detail) -> Element<'a, Message> {
    let store = &app.store;
    let package = detail.package.ready();

    let title = package
        .map(|p| if p.pname.is_empty() { p.name.clone() } else { p.pname.clone() })
        .unwrap_or_else(|| detail.title.clone());
    let installed = detail.attr.is_empty() || package.is_some_and(|p| store.is_installed(&p.pname));

    let mut heading = row![text(title).size(24).font(BOLD)].spacing(10).align_y(Alignment::Center);
    if let Some(version) = package.map(|p| p.version.as_str()).filter(|v| !v.is_empty()) {
        heading = heading.push(
            container(text(version.to_owned()).size(12).font(Font::MONOSPACE).color(ACCENT))
                .padding([3, 8])
                .style(theme::chip(ACCENT)),
        );
    }
    if installed {
        heading = heading.push(installed_chip());
    }

    let mut top = column![heading].spacing(6);
    if let Some(description) = package.map(|p| p.description.as_str()).filter(|d| !d.is_empty()) {
        top = top.push(text(description.to_owned()).size(14).color(MUTED));
    }

    let mut head = column![row![
        icon_tile(lucide::package(), ACCENT_ALT, 22.0, 46.0, 0.4),
        container(top).width(Length::Fill),
        links(package),
    ]
    .spacing(16)
    .align_y(Alignment::Center)]
    .spacing(18);

    match &detail.package {
        Load::Ready(package) => {
            head = head.push(facts(app, detail, package));
        }
        Load::Failed(error) => {
            head = head.push(text(error.clone()).size(13).color(DANGER));
        }
        Load::Idle | Load::Loading => {
            head = head.push(bar(None, &app.fx, ACCENT_ALT, theme::RAISED, 4.0));
        }
    }

    let mut sections = column![card_with(head.into(), 24)].spacing(16);
    if let Some(about) = package.map(|p| p.long_description.trim()).filter(|d| !d.is_empty()) {
        sections = sections.push(card_with(
            column![text("About").size(13).color(MUTED).font(BOLD), text(about.to_owned()).size(14)].spacing(10).into(),
            24,
        ));
    }
    sections.into()
}

/// "Homepage" and "Source on GitHub", for whichever the package has.
fn links<'a>(package: Option<&Package>) -> Element<'a, Message> {
    let link = |icon: Text<'static>, label: &'static str, url: &str| -> Element<'a, Message> {
        button(row![icon.size(13), text(label).size(13)].spacing(8).align_y(Alignment::Center))
            .padding([8, 14])
            .style(ghost_button)
            .on_press(Message::OpenUrl(url.to_owned()))
            .into()
    };
    let mut buttons = row![].spacing(8);
    if let Some(package) = package {
        if package.homepage.starts_with("http") {
            buttons = buttons.push(link(lucide::globe(), "Homepage", &package.homepage));
        }
        if !package.source_url.is_empty() {
            buttons = buttons.push(link(lucide::github(), "Source", &package.source_url));
        }
    }
    buttons.into()
}

fn facts<'a>(app: &'a App, detail: &'a Detail, package: &'a Package) -> Element<'a, Message> {
    let mut facts = column![].spacing(14);

    if !detail.attr.is_empty() {
        facts = facts.push(fact(lucide::at_sign(), "Attribute", mono(format!("pkgs.{}", detail.attr))));
    }
    if !package.main_program.is_empty() {
        facts = facts.push(fact(lucide::terminal(), "Program", mono(package.main_program.clone())));
    }
    if !package.licenses.is_empty() {
        facts = facts.push(fact(lucide::scale(), "License", text(package.licenses.join(", ")).size(14).into()));
    }
    if !package.source_path.is_empty() {
        let mut location = row![mono(package.source_path.clone())].spacing(8).align_y(Alignment::Center);
        if package.source_line > 0 {
            location = location.push(
                container(text(format!("line {}", package.source_line)).size(11).color(MUTED))
                    .padding([2, 8])
                    .style(theme::chip(MUTED)),
            );
        }
        location = location.push(copy_button(app, CopyTarget::SourcePath, false));
        facts = facts.push(fact(lucide::file_code(), "nixpkgs path", location.into()));
    }
    facts = facts.push(fact(lucide::users(), "Maintainers", maintainers(&package.maintainers)));

    container(facts).padding([16, 0]).width(Length::Fill).into()
}

fn maintainers<'a>(maintainers: &[Maintainer]) -> Element<'a, Message> {
    if maintainers.is_empty() {
        return text("None listed").size(14).color(MUTED).into();
    }
    row(maintainers.iter().map(|maintainer| {
        let login: String =
            maintainer.github.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')).collect();
        let label = match (maintainer.name.is_empty(), login.is_empty()) {
            (false, false) => format!("{}  @{login}", maintainer.name),
            (false, true) => maintainer.name.clone(),
            _ => format!("@{login}"),
        };
        let chip = container(
            row![lucide::user().size(12).color(ACCENT), text(label).size(13)].spacing(8).align_y(Alignment::Center),
        )
        .padding([5, 10])
        .style(theme::chip(ACCENT));
        if login.is_empty() {
            chip.into()
        } else {
            button(chip)
                .padding(0)
                .style(|_, _| button::Style::default())
                .on_press(Message::OpenUrl(format!("https://github.com/{login}")))
                .into()
        }
    }))
    .spacing(8)
    .wrap()
    .vertical_spacing(8)
    .into()
}

fn fact<'a>(icon: Text<'a>, label: &'static str, value: Element<'a, Message>) -> Element<'a, Message> {
    row![
        row![icon.size(14).color(MUTED), text(label).size(13).color(MUTED)]
            .spacing(8)
            .align_y(Alignment::Center)
            .width(140),
        container(value).width(Length::Fill),
    ]
    .spacing(12)
    .align_y(Alignment::Center)
    .into()
}

fn mono<'a>(value: String) -> Element<'a, Message> {
    text(value).size(13).font(Font::MONOSPACE).into()
}

fn installed_chip<'a>() -> Element<'a, Message> {
    container(
        row![lucide::check().size(11).color(SUCCESS), text("Installed").size(11).color(SUCCESS)]
            .spacing(5)
            .align_y(Alignment::Center),
    )
    .padding([3, 8])
    .style(theme::chip(SUCCESS))
    .into()
}

/// One package in a list: click to open its page.
fn entry<'a>(
    title: String,
    version: String,
    description: String,
    installed: bool,
    on_press: Message,
) -> Element<'a, Message> {
    let mut name = row![text(title).size(15).font(BOLD)].spacing(8).align_y(Alignment::Center);
    if !version.is_empty() {
        name = name.push(text(version).size(12).font(Font::MONOSPACE).color(MUTED));
    }
    if installed {
        name = name.push(installed_chip());
    }

    button(
        row![
            icon_tile(lucide::package(), ACCENT_ALT, 16.0, 36.0, 0.0),
            column![name, text(description).size(13).color(MUTED).wrapping(Wrapping::None)]
                .spacing(2)
                .width(Length::Fill),
            lucide::chevron_right().size(14).color(MUTED),
        ]
        .spacing(14)
        .align_y(Alignment::Center),
    )
    .padding([10, 12])
    .width(Length::Fill)
    .style(entry_style)
    .on_press(on_press)
    .into()
}

fn entry_style(_: &iced::Theme, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: Some(Background::Color(if hovered { theme::RAISED } else { Color::TRANSPARENT })),
        text_color: TEXT,
        border: Border {
            color: if hovered { theme::BORDER } else { Color::TRANSPARENT },
            width: 1.0,
            radius: 12.0.into(),
        },
        ..Default::default()
    }
}

fn tab_button<'a>(label: String, icon: Text<'a>, active: bool, tab: Tab) -> Element<'a, Message> {
    button(row![icon.size(14), text(label).size(14)].spacing(8).align_y(Alignment::Center))
        .padding([8, 14])
        .style(move |_, status| {
            let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
            button::Style {
                background: Some(Background::Color(if active {
                    theme::with_alpha(ACCENT, 0.16)
                } else if hovered {
                    theme::RAISED
                } else {
                    Color::TRANSPARENT
                })),
                text_color: if active { ACCENT } else { TEXT },
                border: Border {
                    color: if active { theme::with_alpha(ACCENT, 0.4) } else { Color::TRANSPARENT },
                    width: 1.0,
                    radius: 10.0.into(),
                },
                ..Default::default()
            }
        })
        .on_press(Message::StoreTab(tab))
        .into()
}

fn card<'a>(content: Element<'a, Message>) -> Element<'a, Message> {
    card_with(content, 8)
}

fn card_with<'a>(content: Element<'a, Message>, padding: u16) -> Element<'a, Message> {
    container(content).padding(padding).width(Length::Fill).style(theme::card).into()
}

/// A centred message for when there is nothing to list.
fn notice<'a>(icon: Text<'a>, tint: Color, title: &str, detail: &str) -> Element<'a, Message> {
    card_with(
        row![
            centered_icon(icon, 22.0, tint),
            column![text(title.to_owned()).size(15).font(BOLD), text(detail.to_owned()).size(13).color(MUTED)]
                .spacing(2),
        ]
        .spacing(16)
        .align_y(Alignment::Center)
        .into(),
        22,
    )
}

fn failure<'a>(title: &str, error: &str) -> Element<'a, Message> {
    card_with(
        row![
            lucide::circle_x().size(22).color(DANGER),
            column![
                text(title.to_owned()).size(15).font(BOLD).color(DANGER),
                text(error.to_owned()).size(13).color(MUTED)
            ]
            .spacing(2)
            .width(Length::Fill),
        ]
        .spacing(16)
        .align_y(Alignment::Center)
        .into(),
        22,
    )
}

/// Something is on its way: say what, with a sweeping bar.
fn working<'a>(fx: &crate::fx::Fx, title: String, detail: &str) -> Element<'a, Message> {
    card_with(
        column![
            text(title).size(15).font(BOLD),
            text(detail.to_owned()).size(13).color(MUTED),
            space::vertical().height(4),
            bar(None, fx, ACCENT_ALT, theme::RAISED, 4.0),
        ]
        .spacing(4)
        .into(),
        22,
    )
}
