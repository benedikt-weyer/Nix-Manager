//! The store page: browse and search the packages the flake's nixpkgs offers,
//! see what is installed, read about any one of them, and add them to the
//! configuration.

use std::collections::HashSet;

use iced::widget::{button, column, container, pick_list, row, space, text, text::Wrapping, text_input, Text};
use iced::{Alignment, Background, Border, Color, Element, Font, Length};
use iced_fonts::lucide;
use nix_backend_client::{Destination, Maintainer, Package, SearchPackagesReply};

use crate::theme::{self, ACCENT, ACCENT_ALT, DANGER, MUTED, SUCCESS, TEXT, WARNING};
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

/// The file packages go to unless another is picked.
pub const DEFAULT_DESTINATION: &str = "nix-manager-config.nix";

/// Text the setup guide offers to copy.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Snippet {
    /// The import line, for `configuration.nix`.
    Imports,
    /// The same line, for a flake's `modules` list.
    Modules,
    GitAdd,
}

pub fn snippet(kind: Snippet, file: &str, host: &str) -> String {
    match kind {
        Snippet::Imports => format!(
            "{{ config, pkgs, ... }}:\n{{\n  imports = [\n    ./hardware-configuration.nix\n    ./{file}   # <- add this line\n  ];\n\n  # ... the rest of your configuration\n}}"
        ),
        Snippet::Modules => format!(
            "nixosConfigurations.{host} = nixpkgs.lib.nixosSystem {{\n  modules = [\n    ./configuration.nix\n    ./{file}   # <- add this line\n  ];\n}};"
        ),
        Snippet::GitAdd => format!("git add {file}"),
    }
}

/// What a change to the configuration led to, shown above the page.
pub struct Banner {
    pub text: String,
    pub error: bool,
    pub action: Option<BannerAction>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BannerAction {
    /// To the dashboard, to rebuild.
    Rebuild,
    /// To the setup guide.
    Guide,
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
    pub destinations: Load<Vec<Destination>>,
    /// The file name packages are added to.
    pub destination: String,
    /// The name being typed for a new destination, while that form is open.
    pub naming: Option<String>,
    /// Whether the setup guide is showing.
    pub guide: bool,
    pub banner: Option<Banner>,
    /// A change to the configuration is on its way.
    pub changing: bool,
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
            destinations: Load::Idle,
            destination: DEFAULT_DESTINATION.to_owned(),
            naming: None,
            guide: false,
            banner: None,
            changing: false,
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

    pub fn set_destinations(&mut self, result: Result<Vec<Destination>, String>) {
        if let Ok(destinations) = &result {
            if !destinations.iter().any(|d| d.file == self.destination) {
                self.destination = DEFAULT_DESTINATION.to_owned();
            }
        }
        self.destinations = Load::from_result(result);
    }

    /// Takes in a destination the backend just changed or created.
    pub fn put_destination(&mut self, destination: Destination) {
        let mut all = match std::mem::replace(&mut self.destinations, Load::Idle) {
            Load::Ready(all) => all,
            _ => Vec::new(),
        };
        match all.iter_mut().find(|d| d.file == destination.file) {
            Some(existing) => *existing = destination,
            None => {
                all.push(destination);
                all.sort_by(|a, b| b.is_default.cmp(&a.is_default).then_with(|| a.file.cmp(&b.file)));
            }
        }
        self.destinations = Load::Ready(all);
    }

    fn files(&self) -> Vec<String> {
        match self.destinations.ready() {
            Some(all) if !all.is_empty() => all.iter().map(|d| d.file.clone()).collect(),
            _ => vec![DEFAULT_DESTINATION.to_owned()],
        }
    }

    /// The destination packages currently go to, if the backend has listed it.
    pub fn selected(&self) -> Option<&Destination> {
        self.destinations.ready()?.iter().find(|d| d.file == self.destination)
    }

    /// The destination that lists `attr`, if any.
    pub fn configured(&self, attr: &str) -> Option<&Destination> {
        self.destinations.ready()?.iter().find(|d| d.packages.iter().any(|p| p == attr))
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
    let body = if app.store.guide {
        guide_page(app)
    } else {
        match &app.store.detail {
            Some(detail) => detail_page(app, detail),
            None => list_page(app),
        }
    };
    match &app.store.banner {
        Some(banner) => column![banner_view(banner), body].spacing(16).into(),
        None => body,
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
    let tabs = row![
        tabs,
        space::horizontal(),
        button(row![lucide::book_open().size(13), text("Setup guide").size(13)].spacing(8).align_y(Alignment::Center))
            .padding([8, 14])
            .style(ghost_button)
            .on_press(Message::OpenGuide),
    ]
    .align_y(Alignment::Center);

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

    let mut page = column![tabs, search].spacing(16);
    if online && store.tab == Tab::Browse {
        page = page.push(destination_bar(app));
    }
    page.push(body).into()
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
                    add_action(app, &package.attr),
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
                    space::horizontal().width(0).into(),
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
    if !detail.attr.is_empty() {
        sections = sections.push(config_card(app, &detail.attr));
    }
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
    action: Element<'a, Message>,
    on_press: Message,
) -> Element<'a, Message> {
    let mut name = row![text(title).size(15).font(BOLD)].spacing(8).align_y(Alignment::Center);
    if !version.is_empty() {
        name = name.push(text(version).size(12).font(Font::MONOSPACE).color(MUTED));
    }
    if installed {
        name = name.push(installed_chip());
    }

    row![
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
        .on_press(on_press),
        action,
    ]
    .spacing(6)
    .padding([0, 6])
    .align_y(Alignment::Center)
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

// ------------------------------------------------------ adding to the config --

/// What a search result offers: add it, or note that it is in the config.
fn add_action<'a>(app: &App, attr: &str) -> Element<'a, Message> {
    let store = &app.store;
    if store.configured(attr).is_some() {
        return container(
            row![lucide::check().size(12).color(SUCCESS), text("Added").size(12).color(SUCCESS)]
                .spacing(5)
                .align_y(Alignment::Center),
        )
        .padding([5, 10])
        .style(theme::chip(SUCCESS))
        .into();
    }
    let can_add = matches!(app.conn, Conn::Online(_)) && !store.changing;
    button(row![lucide::plus().size(13), text("Add").size(13)].spacing(6).align_y(Alignment::Center))
        .padding([6, 12])
        .style(ghost_button)
        .on_press_maybe(can_add.then(|| Message::AddPackage(attr.to_owned())))
        .into()
}

fn destination_picker<'a>(store: &Store) -> Element<'a, Message> {
    let files = store.files();
    let selected = files.iter().find(|file| **file == store.destination).cloned();
    pick_list(files, selected, Message::SelectDestination).text_size(13).padding([6, 10]).into()
}

/// Where packages will be added, and the state of that file.
fn destination_bar(app: &App) -> Element<'_, Message> {
    let store = &app.store;

    let status: Element<_> = match store.selected() {
        Some(d) if d.exists && d.imported => status_chip(lucide::circle_check(), "imported", SUCCESS),
        Some(d) if d.exists => status_chip(lucide::triangle_alert(), "not imported yet", WARNING),
        Some(_) => status_chip(lucide::file_plus(), "created on first add", MUTED),
        None => space::horizontal().width(0).into(),
    };

    let mut bar = column![row![
        lucide::file_plus().size(14).color(MUTED),
        text("Add to").size(13).color(MUTED),
        destination_picker(store),
        button(row![lucide::plus().size(12), text("New file").size(12)].spacing(6).align_y(Alignment::Center))
            .padding([5, 10])
            .style(ghost_button)
            .on_press(Message::NewDestination),
        space::horizontal(),
        status,
    ]
    .spacing(10)
    .align_y(Alignment::Center)]
    .spacing(10);

    if let Some(name) = &store.naming {
        let can_create = !name.trim().is_empty() && !store.changing;
        bar = bar.push(
            row![
                text_input("A name, e.g. gaming", name)
                    .on_input(Message::NewDestinationName)
                    .on_submit_maybe(can_create.then_some(Message::CreateDestination))
                    .padding(8)
                    .size(13)
                    .style(theme::input),
                text(format!("→ nix-manager-{}.nix", slug(name))).size(12).font(Font::MONOSPACE).color(MUTED),
                button(text("Create").size(13))
                    .padding([7, 14])
                    .style(theme::primary_button)
                    .on_press_maybe(can_create.then_some(Message::CreateDestination)),
                button(text("Cancel").size(13)).padding([7, 12]).style(ghost_button).on_press(Message::NewDestination),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );
    }

    if store.selected().is_some_and(|d| d.exists && !d.imported) {
        bar = bar.push(
            row![
                lucide::triangle_alert().size(14).color(WARNING),
                text(format!(
                    "{} isn't imported by your configuration yet, so packages added to it won't be installed.",
                    store.destination
                ))
                .size(13)
                .width(Length::Fill),
                button(text("Show me how").size(13)).padding([5, 12]).style(ghost_button).on_press(Message::OpenGuide),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );
    }

    container(bar).padding([12, 16]).width(Length::Fill).style(theme::card).into()
}

/// What `nix-manager-<this>.nix` will be called for what is typed.
fn slug(name: &str) -> String {
    let name = name.trim().to_lowercase();
    let name = name.strip_suffix(".nix").unwrap_or(&name);
    let name = name.strip_prefix("nix-manager-").unwrap_or(name);
    if name.is_empty() {
        "…".to_owned()
    } else {
        name.to_owned()
    }
}

fn status_chip<'a>(icon: Text<'a>, label: &'static str, tint: Color) -> Element<'a, Message> {
    container(row![icon.size(12).color(tint), text(label).size(12).color(tint)].spacing(6).align_y(Alignment::Center))
        .padding([4, 10])
        .style(theme::chip(tint))
        .into()
}

/// On a package's page: put it in the configuration, or take it out.
fn config_card<'a>(app: &'a App, attr: &'a str) -> Element<'a, Message> {
    let store = &app.store;
    let can_change = matches!(app.conn, Conn::Online(_)) && !store.changing;

    let body: Element<_> = match store.configured(attr) {
        Some(destination) => row![
            column![
                row![lucide::circle_check().size(15).color(SUCCESS), text("In your configuration").size(15).font(BOLD)]
                    .spacing(8)
                    .align_y(Alignment::Center),
                text(format!("Listed in {}. Rebuild to install it.", destination.file)).size(13).color(MUTED),
            ]
            .spacing(2)
            .width(Length::Fill),
            button(
                row![lucide::trash().size(13), text(format!("Remove from {}", destination.file)).size(13)]
                    .spacing(8)
                    .align_y(Alignment::Center)
            )
            .padding([8, 14])
            .style(ghost_button)
            .on_press_maybe(can_change.then(|| Message::RemovePackage(attr.to_owned()))),
        ]
        .spacing(16)
        .align_y(Alignment::Center)
        .into(),
        None => {
            let target = store.selected().map_or_else(|| store.destination.clone(), |d| d.path.clone());
            row![
                column![
                    text("Install it").size(15).font(BOLD),
                    text(format!("Writes pkgs.{attr} into {target}. Rebuild afterwards to install it."))
                        .size(13)
                        .color(MUTED),
                ]
                .spacing(2)
                .width(Length::Fill),
                destination_picker(store),
                button(
                    row![lucide::plus().size(14), text("Add to config").size(14)].spacing(8).align_y(Alignment::Center)
                )
                .padding([9, 16])
                .style(theme::primary_button)
                .on_press_maybe(can_change.then(|| Message::AddPackage(attr.to_owned()))),
            ]
            .spacing(14)
            .align_y(Alignment::Center)
            .into()
        }
    };
    card_with(body, 20)
}

fn banner_view(banner: &Banner) -> Element<'_, Message> {
    let (icon, tint) = if banner.error { (lucide::circle_x(), DANGER) } else { (lucide::circle_check(), SUCCESS) };
    let mut content = row![icon.size(16).color(tint), text(banner.text.clone()).size(13).width(Length::Fill)]
        .spacing(12)
        .align_y(Alignment::Center);
    match banner.action {
        Some(BannerAction::Rebuild) => {
            content = content.push(
                button(text("Go to dashboard").size(13))
                    .padding([5, 12])
                    .style(ghost_button)
                    .on_press(Message::GoToDashboard),
            );
        }
        Some(BannerAction::Guide) => {
            content = content.push(
                button(text("Show me how").size(13)).padding([5, 12]).style(ghost_button).on_press(Message::OpenGuide),
            );
        }
        None => {}
    }
    content = content.push(
        button(lucide::x().size(14).color(MUTED)).padding(6).style(ghost_button).on_press(Message::DismissBanner),
    );
    container(content).padding([10, 16]).width(Length::Fill).style(theme::chip(tint)).into()
}

// ------------------------------------------------------------------ guide --

fn guide_page(app: &App) -> Element<'_, Message> {
    let store = &app.store;
    let file = store.destination.as_str();
    let host = app.effective_host_name();

    let status: Element<_> = match store.selected() {
        Some(d) if d.exists && d.imported => status_chip(lucide::circle_check(), "imported", SUCCESS),
        Some(d) if d.exists => status_chip(lucide::triangle_alert(), "not imported yet", WARNING),
        _ => status_chip(lucide::file_plus(), "not created yet", MUTED),
    };

    let head = row![
        icon_tile(lucide::book_open(), ACCENT, 22.0, 46.0, 0.4),
        column![
            text("Use your packages file").size(22).font(BOLD),
            text(format!("Make {file} part of your NixOS configuration.")).size(14).color(MUTED),
        ]
        .spacing(2)
        .width(Length::Fill),
        status,
        button(row![lucide::refresh_cw().size(13), text("Check again").size(13)].spacing(8).align_y(Alignment::Center))
            .padding([8, 14])
            .style(ghost_button)
            .on_press(Message::ReloadDestinations),
    ]
    .spacing(16)
    .align_y(Alignment::Center);

    let intro = text(format!(
        "Packages you add in the store are written to {file} in your configuration directory. NixOS only reads \
         files that something imports, so the file has to be hooked in once. After that, adding a package is just \
         a click and a rebuild."
    ))
    .size(14);

    let imports = step(
        1,
        "Import the file",
        column![
            text("Add it to the imports of your configuration. Use whichever matches how your setup is organised.")
                .size(14),
            caption("In configuration.nix — the usual place"),
            code(app, Snippet::Imports, file, host),
            text(format!("If configuration.nix sits in a subfolder, adjust the path, e.g. ../{file}."))
                .size(13)
                .color(MUTED),
            caption("In flake.nix — if your flake lists its modules itself"),
            code(app, Snippet::Modules, file, host),
        ]
        .spacing(10)
        .into(),
    );

    let git = step(
        2,
        "Let git see the file",
        column![
            text(
                "A flake only reads files that git tracks. Nix Manager stages the file for you when it creates it \
                 inside a git repository. If you created or moved it yourself, run this in your configuration \
                 directory:"
            )
            .size(14),
            code(app, Snippet::GitAdd, file, host),
            text("Commit it together with the rest of your configuration when you are happy with it.")
                .size(13)
                .color(MUTED),
        ]
        .spacing(10)
        .into(),
    );

    let rebuild = step(
        3,
        "Rebuild",
        text(
            "Back on the dashboard, run Build to check that everything evaluates, then Switch to install. \
             Afterwards the packages show up under Installed. Adding or removing more packages always needs \
             another rebuild.",
        )
        .size(14)
        .into(),
    );

    let notes = step(
        4,
        "Good to know",
        column![
            bullet(format!(
                "Only the lines between the `nix-manager:begin` and `nix-manager:end` comments in {file} are managed. \
                 Anything else you write in the file is left alone."
            )),
            bullet(
                "Use New file on the Browse tab to keep packages apart, for example gaming or work. Each file \
                 needs its own import line, exactly as above."
                    .to_owned()
            ),
            bullet("Packages go into environment.systemPackages, so they are installed for every user.".to_owned()),
            bullet(
                "Nix Manager must be able to write to the configuration directory. If it can't, make the directory \
                 yours (sudo chown -R $USER <directory>) or point Nix Manager at a copy you own in Settings."
                    .to_owned()
            ),
        ]
        .spacing(8)
        .into(),
    );

    column![
        card_with(column![head, intro].spacing(16).into(), 24),
        card_with(imports, 24),
        card_with(git, 24),
        card_with(rebuild, 24),
        card_with(notes, 24),
    ]
    .spacing(16)
    .into()
}

fn step<'a>(number: u8, title: &'static str, body: Element<'a, Message>) -> Element<'a, Message> {
    row![
        container(text(number.to_string()).size(13).font(BOLD).color(ACCENT))
            .width(28)
            .height(28)
            .center(28)
            .style(theme::chip(ACCENT)),
        column![text(title).size(16).font(BOLD), body].spacing(10).width(Length::Fill),
    ]
    .spacing(16)
    .into()
}

fn caption<'a>(label: &'static str) -> Element<'a, Message> {
    text(label).size(13).color(MUTED).font(BOLD).into()
}

fn bullet<'a>(content: String) -> Element<'a, Message> {
    row![text("•").size(14).color(MUTED), text(content).size(14).width(Length::Fill)].spacing(10).into()
}

/// A snippet to read and to copy.
fn code<'a>(app: &App, kind: Snippet, file: &str, host: &str) -> Element<'a, Message> {
    container(
        row![
            text(snippet(kind, file, host)).size(13).font(Font::MONOSPACE).width(Length::Fill),
            copy_button(app, CopyTarget::Snippet(kind), false),
        ]
        .spacing(10)
        .align_y(Alignment::Start),
    )
    .padding([12, 14])
    .width(Length::Fill)
    .style(theme::well)
    .into()
}
