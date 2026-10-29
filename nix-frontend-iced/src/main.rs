mod fx;
mod store;
mod theme;
mod view;

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use iced::widget::text_editor;
use iced::{window, Subscription, Task};
use nix_backend_client::settings::Settings;
use nix_backend_client::{
    BuildItem, BuildStats, Client, Destination, EventPhase, FlakeStatusReply, OperationEvent, OperationKind, Package,
    ProgressEvent, RebuildMode, SearchPackagesReply, SetConfigPackageReply, SystemTarget,
};

use fx::{Fx, Slot};
use store::{Banner, BannerAction, Detail, Load, Snippet, Store, Tab};

const ICON_PNG: &[u8] = include_bytes!("../../assets/icons/nix-manager-256.png");

/// The logo as a window icon. Shown on X11; Wayland compositors pick the icon
/// from the desktop entry matching the app id instead.
fn window_icon() -> Option<window::Icon> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(ICON_PNG)).read_info().ok()?;
    let mut pixels = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut pixels).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    pixels.truncate(info.buffer_size());
    window::icon::from_rgba(pixels, info.width, info.height).ok()
}

fn main() -> iced::Result {
    iced::application(App::new, App::update, App::view)
        .title("Nix Manager")
        .subscription(App::subscription)
        .theme(|_: &App| theme::theme())
        .font(iced_fonts::LUCIDE_FONT_BYTES)
        .antialiasing(true)
        .window(window::Settings {
            size: iced::Size::new(1040.0, 800.0),
            min_size: Some(iced::Size::new(760.0, 560.0)),
            icon: window_icon(),
            platform_specific: window::settings::PlatformSpecific {
                application_id: "nix-manager".to_owned(),
                ..Default::default()
            },
            ..Default::default()
        })
        .run()
}

/// What the user can start. The index is also the slot in `Fx`'s hover
/// animators.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Update,
    Rebuild(RebuildMode),
}

impl Action {
    pub const ALL: [Action; fx::ACTIONS] = [
        Action::Update,
        Action::Rebuild(RebuildMode::BuildOnly),
        Action::Rebuild(RebuildMode::Switch),
        Action::Rebuild(RebuildMode::Boot),
        Action::Rebuild(RebuildMode::Test),
    ];
}

/// What a copy button puts on the clipboard.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CopyTarget {
    /// The status message (the full error text when the run failed).
    Message,
    /// The whole output log.
    Output,
    /// Where the open package is defined in nixpkgs.
    SourcePath,
    /// A snippet from the setup guide.
    Snippet(Snippet),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Page {
    Dashboard,
    Settings,
    Store,
}

pub enum Conn {
    Connecting,
    Online(Client),
    Offline(String),
}

pub enum Outcome {
    Running,
    Succeeded,
    Failed(String),
}

/// The operation currently shown in the activity panel. It is built purely
/// from the backend's `Watch` stream, so every frontend instance attached to
/// the backend shows the same thing no matter which one started it.
pub struct Run {
    pub kind: OperationKind,
    pub mode: RebuildMode,
    pub phase: EventPhase,
    pub outcome: Outcome,
    pub message: String,
    /// `None` until nix reports something derivable; the bar sweeps meanwhile.
    pub fraction: Option<f32>,
    pub stats: Option<BuildStats>,
    pub items: Vec<BuildItem>,
    /// The output, one entry per line. `App::log_view` is what is drawn.
    pub log: VecDeque<String>,
}

impl Run {
    fn new(kind: OperationKind, mode: RebuildMode) -> Self {
        Self {
            kind,
            mode,
            phase: EventPhase::Started,
            outcome: Outcome::Running,
            message: String::new(),
            fraction: None,
            stats: None,
            items: Vec::new(),
            log: VecDeque::new(),
        }
    }

    pub fn running(&self) -> bool {
        matches!(self.outcome, Outcome::Running)
    }

    /// The phases this kind of operation walks through, in order.
    pub fn steps(&self) -> &'static [EventPhase] {
        match (self.kind, self.mode) {
            (OperationKind::UpdateFlake, _) => &[EventPhase::FlakeUpdate],
            (_, RebuildMode::BuildOnly) => &[EventPhase::Build, EventPhase::Diff],
            (_, RebuildMode::Test) => &[EventPhase::Build, EventPhase::Diff, EventPhase::Activate],
            _ => &[EventPhase::Build, EventPhase::Diff, EventPhase::Register, EventPhase::Activate],
        }
    }
}

const MAX_LOG_LINES: usize = 2000;

pub struct App {
    pub conn: Conn,
    /// The applied target: what actions use and what is persisted.
    pub config_dir: String,
    pub host_name: String,
    pub page: Page,
    /// What the settings page inputs currently say; becomes the applied
    /// target on Apply.
    pub draft_config_dir: String,
    pub draft_host_name: String,
    /// What the backend falls back to when a field is empty, once it has said.
    pub default_config_dir: Option<String>,
    pub default_host_name: Option<String>,
    /// Only what the user typed - not values that came from the environment.
    saved: Settings,
    pub status: Option<Result<FlakeStatusReply, String>>,
    pub run: Option<Run>,
    pub notice: Option<String>,
    local_hostname: String,
    pub show_log: bool,
    /// Whether the downloads/builds list shows every running item instead of
    /// just the first few.
    pub show_all_items: bool,
    /// The output as the (read-only) editor that draws it, which is what makes
    /// the text wrap, selectable and copyable.
    pub log_view: text_editor::Content,
    /// Lines that arrived but aren't in `log_view` yet: it stands still while
    /// the pointer is over it or text is selected, so reading isn't disturbed.
    pub log_pending: Vec<String>,
    pub log_hovered: bool,
    pub copied: Option<CopyTarget>,
    pub store: Store,
    pub fx: Fx,
    last_frame: Option<Instant>,
}

#[derive(Debug, Clone)]
pub enum Message {
    Frame(Instant),
    Retry,
    Connected(Result<Client, String>),
    Watch(Result<OperationEvent, String>),
    WatchEnded,
    RefreshStatus,
    StatusLoaded(SystemTarget, Result<FlakeStatusReply, String>),
    OpenSettings,
    CloseSettings,
    DraftConfigDirChanged(String),
    DraftHostNameChanged(String),
    ApplySettings,
    RevertDraft,
    UseDefaults,
    Start(Action),
    StartRejected(String),
    Hover(usize, bool),
    ToggleLog,
    ToggleItems,
    LogAction(text_editor::Action),
    LogHover(bool),
    Copy(CopyTarget),
    DismissRun,
    DismissNotice,
    OpenStore,
    StoreBack,
    StoreTab(Tab),
    StoreQuery(String),
    StoreSearch,
    StoreSearched(u64, Result<SearchPackagesReply, String>),
    StoreFilter(String),
    InstalledLoaded(SystemTarget, Result<Vec<Package>, String>),
    ReloadInstalled,
    OpenResult(usize),
    OpenInstalled(usize),
    PackageLoaded(String, Result<Package, String>),
    OpenUrl(String),
    DestinationsLoaded(SystemTarget, Result<Vec<Destination>, String>),
    ReloadDestinations,
    SelectDestination(String),
    NewDestination,
    NewDestinationName(String),
    CreateDestination,
    DestinationCreated(SystemTarget, Result<Destination, String>),
    AddPackage(String),
    RemovePackage(String),
    PackageSet(SystemTarget, String, bool, Result<SetConfigPackageReply, String>),
    OpenGuide,
    DismissBanner,
    GoToDashboard,
}

fn status_text(status: nix_backend_client::Status) -> String {
    status.message().to_owned()
}

fn connect() -> Task<Message> {
    Task::perform(
        async { Client::connect(nix_backend_client::socket_path()).await.map_err(|e| e.to_string()) },
        Message::Connected,
    )
}

fn watch(mut client: Client) -> Task<Message> {
    Task::future(async move { client.watch().await.map_err(status_text) }).then(|result| match result {
        Ok(stream) => Task::stream(
            stream
                .map(|item| Message::Watch(item.map_err(status_text)))
                .chain(futures::stream::once(async { Message::WatchEnded })),
        ),
        Err(error) => Task::done(Message::Watch(Err(error))),
    })
}

impl App {
    fn new() -> (Self, Task<Message>) {
        let saved = Settings::load();
        // An explicit environment (e.g. `./scripts/run -c DIR`) beats what was saved.
        let from_env = |name: &str, saved: &str| match std::env::var(name) {
            Ok(value) if !value.trim().is_empty() => value,
            _ => saved.to_owned(),
        };
        let app = Self {
            conn: Conn::Connecting,
            config_dir: from_env("NIX_BACKEND_CONFIG_DIR", &saved.config_dir),
            host_name: from_env("NIX_BACKEND_HOST_NAME", &saved.host_name),
            page: Page::Dashboard,
            draft_config_dir: String::new(),
            draft_host_name: String::new(),
            default_config_dir: None,
            default_host_name: None,
            saved,
            status: None,
            run: None,
            notice: None,
            local_hostname: std::fs::read_to_string("/proc/sys/kernel/hostname")
                .map(|name| name.trim().to_owned())
                .unwrap_or_default(),
            show_log: true,
            show_all_items: false,
            log_view: text_editor::Content::new(),
            log_pending: Vec::new(),
            log_hovered: false,
            copied: None,
            store: Store::new(),
            fx: Fx::new(),
            last_frame: None,
        };
        let mut app = app;
        app.draft_config_dir = app.config_dir.clone();
        app.draft_host_name = app.host_name.clone();
        (app, connect())
    }

    pub fn target(&self) -> SystemTarget {
        SystemTarget { config_dir: self.config_dir.trim().to_owned(), host_name: self.host_name.trim().to_owned() }
    }

    /// The directory that will be used: what was typed, else the backend's default.
    pub fn effective_config_dir(&self) -> &str {
        match self.config_dir.trim() {
            "" => self.default_config_dir.as_deref().unwrap_or("/etc/nixos"),
            typed => typed,
        }
    }

    /// The host that will be used: what was typed, else the backend's default,
    /// else (before the backend has answered) this machine's hostname.
    pub fn effective_host_name(&self) -> &str {
        match self.host_name.trim() {
            "" => self.default_host_name.as_deref().unwrap_or(&self.local_hostname),
            typed => typed,
        }
    }

    /// Whether the settings page has edits that aren't applied yet.
    pub fn settings_dirty(&self) -> bool {
        self.draft_config_dir.trim() != self.config_dir.trim() || self.draft_host_name.trim() != self.host_name.trim()
    }

    fn revert_draft(&mut self) {
        self.draft_config_dir = self.config_dir.clone();
        self.draft_host_name = self.host_name.clone();
    }

    /// Switches page and plays the entrance of the new content.
    fn show(&mut self, page: Page) {
        self.page = page;
        self.fx.restart(Slot::Overview, 0.0);
        self.fx.restart(Slot::Actions, 0.08);
    }

    fn persist(&mut self) {
        if let Err(error) = self.saved.save() {
            self.notice = Some(format!("Could not save settings: {error}"));
        }
    }

    pub fn busy(&self) -> bool {
        self.run.as_ref().is_some_and(Run::running)
    }

    fn client(&self) -> Option<Client> {
        match &self.conn {
            Conn::Online(client) => Some(client.clone()),
            _ => None,
        }
    }

    fn load_status(&self) -> Task<Message> {
        let Some(mut client) = self.client() else {
            return Task::none();
        };
        let target = self.target();
        Task::perform(
            async move {
                let result = client.flake_status(target.clone()).await.map_err(status_text);
                (target, result)
            },
            |(target, result)| Message::StatusLoaded(target, result),
        )
    }

    fn go_offline(&mut self, reason: String) {
        self.conn = Conn::Offline(reason);
        if let Some(run) = self.run.as_mut().filter(|run| run.running()) {
            run.outcome = Outcome::Failed("Lost the connection to the backend".to_owned());
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        let task = self.handle(message);
        self.fx.set_looping(self.busy() || self.store.busy() || matches!(self.conn, Conn::Connecting));
        if !self.fx.animating() {
            // Frames stop arriving once nothing moves; don't let the gap count
            // as one huge step when they resume.
            self.last_frame = None;
        }
        task
    }

    fn handle(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Frame(now) => {
                let dt = self.last_frame.map_or(0.0, |last| now.duration_since(last).as_secs_f32());
                self.last_frame = Some(now);
                self.fx.tick(dt.min(0.05));
                Task::none()
            }
            Message::Retry => {
                if matches!(self.conn, Conn::Offline(_)) {
                    self.conn = Conn::Connecting;
                    return connect();
                }
                Task::none()
            }
            Message::Connected(Ok(client)) => {
                self.conn = Conn::Online(client.clone());
                let store = if self.page == Page::Store {
                    Task::batch([self.load_installed(), self.load_destinations()])
                } else {
                    Task::none()
                };
                Task::batch([self.load_status(), watch(client), store])
            }
            Message::Connected(Err(error)) => {
                self.go_offline(error);
                Task::none()
            }
            Message::WatchEnded => {
                if matches!(self.conn, Conn::Online(_)) {
                    self.go_offline("The backend closed the connection".to_owned());
                }
                Task::none()
            }
            Message::Watch(Err(error)) => {
                if matches!(self.conn, Conn::Online(_)) {
                    self.go_offline(error);
                }
                Task::none()
            }
            Message::Watch(Ok(op)) => self.apply(op),
            Message::RefreshStatus => {
                self.status = None;
                self.load_status()
            }
            Message::StatusLoaded(requested, status) => {
                // What "default" means is only known for fields the request
                // left empty; otherwise the reply just echoes what we sent.
                if let Ok(reply) = &status {
                    if requested.config_dir.is_empty() {
                        self.default_config_dir = Some(reply.resolved_config_dir.clone());
                    }
                    if requested.host_name.is_empty() {
                        self.default_host_name = Some(reply.resolved_host_name.clone());
                    }
                }
                self.status = Some(status);
                Task::none()
            }
            Message::OpenSettings => {
                self.revert_draft();
                self.show(Page::Settings);
                Task::none()
            }
            Message::CloseSettings => {
                self.show(Page::Dashboard);
                Task::none()
            }
            Message::DraftConfigDirChanged(value) => {
                self.draft_config_dir = value;
                Task::none()
            }
            Message::DraftHostNameChanged(value) => {
                self.draft_host_name = value;
                Task::none()
            }
            Message::RevertDraft => {
                self.revert_draft();
                Task::none()
            }
            Message::UseDefaults => {
                self.draft_config_dir.clear();
                self.draft_host_name.clear();
                Task::none()
            }
            Message::ApplySettings => {
                if !self.settings_dirty() {
                    return Task::none();
                }
                let (dir, host) = (self.draft_config_dir.trim().to_owned(), self.draft_host_name.trim().to_owned());
                // Only fields the user changed are persisted, so a value that
                // merely came from the environment doesn't get saved with them.
                if dir != self.config_dir.trim() {
                    self.saved.config_dir = dir.clone();
                }
                if host != self.host_name.trim() {
                    self.saved.host_name = host.clone();
                }
                self.config_dir = dir;
                self.host_name = host;
                self.notice = None;
                self.persist();
                // Forget what we knew about the old target.
                self.status = None;
                self.store.reset();
                self.load_status()
            }
            Message::Start(action) => {
                let Some(mut client) = self.client() else {
                    return Task::none();
                };
                self.notice = None;
                let target = self.target();
                // The progress comes back through `Watch`; the stream returned
                // here is only dropped. Its only use is the rejection when
                // another instance already has an operation running.
                Task::perform(
                    async move {
                        match action {
                            Action::Update => client.update_flake(target).await.map(drop),
                            Action::Rebuild(mode) => client.rebuild(target, mode).await.map(drop),
                        }
                        .map_err(status_text)
                    },
                    |result| match result {
                        Ok(()) => Message::DismissNotice,
                        Err(error) => Message::StartRejected(error),
                    },
                )
            }
            Message::StartRejected(error) => {
                self.notice = Some(error);
                Task::none()
            }
            Message::Hover(index, over) => {
                self.fx.set_hover(index, over);
                Task::none()
            }
            Message::ToggleLog => {
                self.show_log = !self.show_log;
                Task::none()
            }
            Message::ToggleItems => {
                self.show_all_items = !self.show_all_items;
                Task::none()
            }
            Message::LogAction(action) => {
                // Moving, selecting and scrolling are welcome; editing is not.
                if !action.is_edit() {
                    self.log_view.perform(action);
                }
                self.sync_log();
                Task::none()
            }
            Message::LogHover(hovered) => {
                self.log_hovered = hovered;
                self.sync_log();
                Task::none()
            }
            Message::Copy(target) => {
                let text = match target {
                    CopyTarget::SourcePath => self
                        .store
                        .detail
                        .as_ref()
                        .and_then(|detail| detail.package.ready())
                        .map(|package| package.source_path.clone())
                        .unwrap_or_default(),
                    CopyTarget::Snippet(kind) => {
                        store::snippet(kind, &self.store.destination, self.effective_host_name())
                    }
                    CopyTarget::Message | CopyTarget::Output => {
                        let Some(run) = &self.run else { return Task::none() };
                        match target {
                            CopyTarget::Message => match &run.outcome {
                                Outcome::Failed(message) => message.clone(),
                                _ => run.message.clone(),
                            },
                            _ => run.log.iter().map(String::as_str).collect::<Vec<_>>().join("\n"),
                        }
                    }
                };
                if text.is_empty() {
                    return Task::none();
                }
                self.copied = Some(target);
                self.fx.flash();
                iced::clipboard::write(text)
            }
            Message::DismissRun => {
                if !self.busy() {
                    self.run = None;
                    self.log_view = text_editor::Content::new();
                    self.log_pending.clear();
                }
                Task::none()
            }
            Message::DismissNotice => {
                self.notice = None;
                Task::none()
            }
            Message::OpenStore => {
                self.show(Page::Store);
                let installed =
                    if matches!(self.store.installed, Load::Idle) { self.load_installed() } else { Task::none() };
                Task::batch([installed, self.load_destinations(), iced::widget::operation::focus(store::SEARCH_ID)])
            }
            Message::StoreBack => {
                if self.store.guide {
                    self.store.guide = false;
                } else if self.store.detail.take().is_none() {
                    self.show(Page::Dashboard);
                }
                Task::none()
            }
            Message::StoreTab(tab) => {
                self.store.tab = tab;
                iced::widget::operation::focus(store::SEARCH_ID)
            }
            Message::StoreQuery(query) => {
                self.store.query = query;
                Task::none()
            }
            Message::StoreFilter(filter) => {
                self.store.filter = filter;
                Task::none()
            }
            Message::StoreSearch => {
                let query = self.store.query.trim().to_owned();
                let Some(mut client) = self.client().filter(|_| !query.is_empty()) else {
                    return Task::none();
                };
                self.store.search_seq += 1;
                let seq = self.store.search_seq;
                self.store.results = Load::Loading;
                self.store.searched = query.clone();
                let target = self.target();
                Task::perform(
                    async move { client.search_packages(target, query, 0).await.map_err(status_text) },
                    move |result| Message::StoreSearched(seq, result),
                )
            }
            Message::StoreSearched(seq, result) => {
                // Only the latest search counts.
                if seq == self.store.search_seq {
                    self.store.results = Load::from_result(result);
                }
                Task::none()
            }
            Message::ReloadInstalled => self.load_installed(),
            Message::InstalledLoaded(requested, result) => {
                // The target may have changed while the configuration was being read.
                if requested == self.target() {
                    self.store.set_installed(result);
                }
                Task::none()
            }
            Message::OpenResult(index) => {
                let Some(summary) = self.store.results.ready().and_then(|reply| reply.packages.get(index)) else {
                    return Task::none();
                };
                let attr = summary.attr.clone();
                let title = if summary.pname.is_empty() { attr.clone() } else { summary.pname.clone() };
                let Some(mut client) = self.client() else {
                    return Task::none();
                };
                self.store.detail = Some(Detail { title, attr: attr.clone(), package: Load::Loading });
                let target = self.target();
                Task::perform(
                    async move {
                        let result = client.get_package(target, attr.clone()).await.map_err(status_text);
                        (attr, result)
                    },
                    |(attr, result)| Message::PackageLoaded(attr, result),
                )
            }
            Message::OpenInstalled(index) => {
                if let Some(package) = self.store.installed.ready().and_then(|packages| packages.get(index)) {
                    self.store.detail = Some(Detail {
                        title: package.name.clone(),
                        attr: String::new(),
                        package: Load::Ready(package.clone()),
                    });
                }
                Task::none()
            }
            Message::PackageLoaded(attr, result) => {
                if let Some(detail) = self.store.detail.as_mut().filter(|detail| detail.attr == attr) {
                    detail.package = Load::from_result(result);
                }
                Task::none()
            }
            Message::OpenUrl(url) => {
                open_url(&url);
                Task::none()
            }
            Message::ReloadDestinations => self.load_destinations(),
            Message::DestinationsLoaded(requested, result) => {
                if requested == self.target() {
                    self.store.set_destinations(result);
                }
                Task::none()
            }
            Message::SelectDestination(file) => {
                self.store.destination = file;
                Task::none()
            }
            Message::NewDestination => {
                self.store.naming = match self.store.naming {
                    Some(_) => None,
                    None => Some(String::new()),
                };
                Task::none()
            }
            Message::NewDestinationName(name) => {
                if self.store.naming.is_some() {
                    self.store.naming = Some(name);
                }
                Task::none()
            }
            Message::CreateDestination => {
                let name = self.store.naming.clone().unwrap_or_default();
                let Some(mut client) = self.client().filter(|_| !name.trim().is_empty() && !self.store.changing) else {
                    return Task::none();
                };
                self.store.changing = true;
                let target = self.target();
                Task::perform(
                    async move {
                        let result = client.create_destination(target.clone(), name).await.map_err(status_text);
                        (target, result)
                    },
                    |(target, result)| Message::DestinationCreated(target, result),
                )
            }
            Message::DestinationCreated(requested, result) => {
                self.store.changing = false;
                if requested != self.target() {
                    return Task::none();
                }
                self.store.banner = Some(match result {
                    Ok(destination) => {
                        let text = if destination.imported {
                            format!("{} is ready to use.", destination.file)
                        } else {
                            format!(
                                "Created {}. Your configuration has to import it before the packages in it are installed.",
                                destination.file
                            )
                        };
                        let action = (!destination.imported).then_some(BannerAction::Guide);
                        self.store.destination = destination.file.clone();
                        self.store.naming = None;
                        self.store.put_destination(destination);
                        Banner { text, error: false, action }
                    }
                    Err(error) => Banner { text: error, error: true, action: None },
                });
                Task::none()
            }
            Message::AddPackage(attr) => {
                let file = self.store.destination.clone();
                self.set_config_package(file, attr, true)
            }
            Message::RemovePackage(attr) => {
                let Some(file) = self.store.configured(&attr).map(|destination| destination.file.clone()) else {
                    return Task::none();
                };
                self.set_config_package(file, attr, false)
            }
            Message::PackageSet(requested, attr, present, result) => {
                self.store.changing = false;
                if requested != self.target() {
                    return Task::none();
                }
                self.store.banner = Some(match result {
                    Ok(reply) => {
                        let destination = reply.destination.unwrap_or_default();
                        let file = destination.file.clone();
                        let staged = if reply.staged_in_git { " The new file was staged in git." } else { "" };
                        let banner = match (present, destination.imported) {
                            (true, true) => Banner {
                                text: format!("Added {attr} to {file}. Rebuild to install it.{staged}"),
                                error: false,
                                action: Some(BannerAction::Rebuild),
                            },
                            (true, false) => Banner {
                                text: format!(
                                    "Added {attr} to {file}, but your configuration doesn't import that file yet.{staged}"
                                ),
                                error: false,
                                action: Some(BannerAction::Guide),
                            },
                            (false, _) => Banner {
                                text: format!("Removed {attr} from {file}. Rebuild to apply it."),
                                error: false,
                                action: Some(BannerAction::Rebuild),
                            },
                        };
                        self.store.put_destination(destination);
                        banner
                    }
                    Err(error) => Banner { text: error, error: true, action: None },
                });
                Task::none()
            }
            Message::OpenGuide => {
                if self.page != Page::Store {
                    self.show(Page::Store);
                }
                self.store.guide = true;
                self.load_destinations()
            }
            Message::DismissBanner => {
                self.store.banner = None;
                Task::none()
            }
            Message::GoToDashboard => {
                self.store.detail = None;
                self.store.guide = false;
                self.show(Page::Dashboard);
                Task::none()
            }
        }
    }

    /// Brings the output view up to date, if the user isn't busy with it.
    fn sync_log(&mut self) {
        if let Some(run) = &self.run {
            sync_log(&mut self.log_view, &mut self.log_pending, self.log_hovered, &run.log);
        }
    }

    fn load_destinations(&mut self) -> Task<Message> {
        let Some(mut client) = self.client() else {
            return Task::none();
        };
        if matches!(self.store.destinations, Load::Idle) {
            self.store.destinations = Load::Loading;
        }
        let target = self.target();
        Task::perform(
            async move {
                let result = client.list_destinations(target.clone()).await.map_err(status_text);
                (target, result)
            },
            |(target, result)| Message::DestinationsLoaded(target, result),
        )
    }

    /// Adds `attr` to (or removes it from) the destination `file`.
    fn set_config_package(&mut self, file: String, attr: String, present: bool) -> Task<Message> {
        let Some(mut client) = self.client().filter(|_| !self.store.changing) else {
            return Task::none();
        };
        self.store.changing = true;
        self.store.banner = None;
        let target = self.target();
        Task::perform(
            async move {
                let result =
                    client.set_config_package(target.clone(), file, attr.clone(), present).await.map_err(status_text);
                (target, attr, present, result)
            },
            |(target, attr, present, result)| Message::PackageSet(target, attr, present, result),
        )
    }

    fn load_installed(&mut self) -> Task<Message> {
        let Some(mut client) = self.client() else {
            return Task::none();
        };
        self.store.installed = Load::Loading;
        let target = self.target();
        Task::perform(
            async move {
                let result = client.list_installed_packages(target.clone()).await.map_err(status_text);
                (target, result)
            },
            |(target, result)| Message::InstalledLoaded(target, result),
        )
    }

    /// Folds one backend event into the activity panel.
    fn apply(&mut self, op: OperationEvent) -> Task<Message> {
        let Some(event) = op.event.clone() else {
            return Task::none();
        };
        let kind = op.kind();
        let mode = op.mode();

        // The previous run is over (or there was none): this event belongs to
        // a new operation, started here or by another instance, or replayed
        // to us as a late joiner.
        if self.run.as_ref().is_none_or(|run| !run.running()) {
            self.run = Some(Run::new(kind, mode));
            self.log_view = text_editor::Content::new();
            self.log_pending.clear();
            self.fx.reset_progress();
            self.fx.restart(Slot::Activity, 0.0);
        }
        let run = self.run.as_mut().expect("run was just ensured");

        let phase = event.phase();
        run.phase = phase;
        let message = nix_backend_client::strip_ansi(&event.message);
        if !message.is_empty() {
            run.message = message.lines().next().unwrap_or_default().to_owned();
            for line in message.lines() {
                run.log.push_back(line.to_owned());
                self.log_pending.push(line.to_owned());
                if run.log.len() > MAX_LOG_LINES {
                    run.log.pop_front();
                }
            }
            sync_log(&mut self.log_view, &mut self.log_pending, self.log_hovered, &run.log);
        }
        absorb_progress(run, &event, &mut self.fx);

        match phase {
            EventPhase::Finished => {
                run.outcome = Outcome::Succeeded;
                run.items.clear();
                run.fraction = Some(1.0);
                self.fx.set_progress(1.0);
                // A finished update or rebuild changes what the status card shows,
                // and what the configuration installs.
                self.store.installed = Load::Idle;
                let installed = if self.page == Page::Store { self.load_installed() } else { Task::none() };
                return Task::batch([self.load_status(), installed]);
            }
            EventPhase::Failed => {
                run.outcome = Outcome::Failed(message);
                run.items.clear();
                return self.load_status();
            }
            _ => {}
        }
        Task::none()
    }

    fn view(&self) -> iced::Element<'_, Message> {
        view::view(self)
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = Vec::new();
        if self.fx.animating() {
            subscriptions.push(window::frames().map(Message::Frame));
        }
        if matches!(self.conn, Conn::Offline(_)) {
            subscriptions.push(iced::time::every(Duration::from_secs(2)).map(|_| Message::Retry));
        }
        Subscription::batch(subscriptions)
    }
}

/// Appends the lines in `pending` to the output view and follows them, unless
/// the pointer is over the view or text is selected in it: new lines would move
/// what is being read or selected, so they wait until the user is done.
fn sync_log(view: &mut text_editor::Content, pending: &mut Vec<String>, hovered: bool, log: &VecDeque<String>) {
    if pending.is_empty() || hovered || view.selection().is_some() {
        return;
    }
    use text_editor::{Action, Edit, Motion};

    // The view keeps growing past the log's own limit; start it afresh now and then.
    if view.line_count() + pending.len() > MAX_LOG_LINES + 500 {
        let lines: Vec<&str> = log.iter().map(String::as_str).collect();
        *view = text_editor::Content::with_text(&lines.join("\n"));
    } else {
        let mut text = pending.join("\n");
        if !view.is_empty() {
            text.insert(0, '\n');
        }
        view.perform(Action::Move(Motion::DocumentEnd));
        view.perform(Action::Edit(Edit::Paste(Arc::new(text))));
    }
    view.perform(Action::Move(Motion::DocumentEnd));
    pending.clear();
}

/// Hands a web link to the desktop's default browser. Package metadata is not
/// trusted to hold anything but web links.
fn open_url(url: &str) {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }
    if let Ok(mut child) = std::process::Command::new("xdg-open").arg(url).spawn() {
        // Reaped off-thread so it doesn't linger as a zombie.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// Copies the live build data out of an event. Plain log lines carry no
/// stats and must not wipe the last snapshot.
fn absorb_progress(run: &mut Run, event: &ProgressEvent, fx: &mut Fx) {
    if let Some(stats) = &event.stats {
        run.stats = Some(stats.clone());
        run.items = event.active_items.clone();
    }
    if event.fraction_done >= 0.0 {
        let fraction = event.fraction_done as f32;
        // Only ever move forward: nix's totals grow as it discovers more work.
        if run.fraction.is_none_or(|current| fraction > current) {
            run.fraction = Some(fraction);
            fx.set_progress(fraction);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(view: &text_editor::Content) -> Vec<String> {
        view.lines().map(|line| line.text.to_string()).collect()
    }

    #[test]
    fn output_lines_follow_in_order() {
        let mut view = text_editor::Content::new();
        let mut pending = vec!["one".to_owned(), "two".to_owned()];
        let log = VecDeque::new();

        sync_log(&mut view, &mut pending, false, &log);
        assert_eq!(lines(&view), ["one", "two"]);
        assert!(pending.is_empty());

        pending.push("three".to_owned());
        sync_log(&mut view, &mut pending, false, &log);
        assert_eq!(lines(&view), ["one", "two", "three"]);
    }

    #[test]
    fn output_waits_while_the_pointer_is_over_it() {
        let mut view = text_editor::Content::with_text("one");
        let mut pending = vec!["two".to_owned()];

        sync_log(&mut view, &mut pending, true, &VecDeque::new());
        assert_eq!(lines(&view), ["one"]);
        assert_eq!(pending, ["two"]);

        sync_log(&mut view, &mut pending, false, &VecDeque::new());
        assert_eq!(lines(&view), ["one", "two"]);
    }

    #[test]
    fn output_waits_while_text_is_selected() {
        let mut view = text_editor::Content::with_text("one");
        view.perform(text_editor::Action::SelectAll);
        let mut pending = vec!["two".to_owned()];

        sync_log(&mut view, &mut pending, false, &VecDeque::new());
        assert_eq!(view.selection().as_deref(), Some("one"), "the selection must survive");
        assert_eq!(pending, ["two"]);
    }

    #[test]
    fn a_runaway_view_is_rebuilt_from_the_log() {
        let log: VecDeque<String> = (0..MAX_LOG_LINES).map(|i| format!("line {i}")).collect();
        let mut view = text_editor::Content::with_text(&vec!["old"; MAX_LOG_LINES + 400].join("\n"));
        let mut pending = vec!["x".to_owned(); 200];

        sync_log(&mut view, &mut pending, false, &log);
        assert_eq!(view.line_count(), MAX_LOG_LINES);
        assert!(pending.is_empty());
    }
}
