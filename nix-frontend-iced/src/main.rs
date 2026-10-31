mod ai;
mod fx;
mod git;
mod inbox;
mod store;
mod theme;
mod view;

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use iced::widget::text_editor;
use iced::{window, Subscription, Task};
use nix_backend_client::settings::{self, Settings};
use nix_backend_client::{
    BuildItem, BuildStats, Client, Destination, EventPhase, FlakeStatusReply, GitCommitReply, GitDiffReply,
    GitStatusReply, OperationKind, Package, RebuildMode, SearchPackagesReply, SetConfigPackageReply, SystemTarget,
};

use ai::Ai;
use fx::{Fx, Slot};
use git::{Diff, Git, Selected};
use inbox::{End, Inbox, Segment};
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
    /// The diff of the open file on the git page.
    Diff,
    /// The agent's answer, by its position in the conversation.
    AiAnswer(usize),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Page {
    Dashboard,
    Settings,
    Store,
    Git,
    Ai,
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
    /// Backend events waiting to be shown; see `inbox`.
    inbox: Arc<Inbox>,
    /// The applied target: what actions use and what is persisted.
    pub config_dir: String,
    pub host_name: String,
    /// The command that opens the configuration; empty means the default.
    pub editor: String,
    /// The command that starts the AI agent; empty means the default.
    pub agent: String,
    pub page: Page,
    /// What the settings page inputs currently say; becomes the applied
    /// target on Apply.
    pub draft_config_dir: String,
    pub draft_host_name: String,
    pub draft_editor: String,
    pub draft_agent: String,
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
    pub git: Git,
    pub ai: Ai,
    pub fx: Fx,
    last_frame: Option<Instant>,
}

#[derive(Debug, Clone)]
pub enum Message {
    Frame(Instant),
    Retry,
    Connected(Result<Client, String>),
    /// The backend sent something; it waits in `App::inbox` for the next frame.
    Inbound,
    WatchFailed(String),
    WatchEnded,
    RefreshStatus,
    StatusLoaded(SystemTarget, Result<FlakeStatusReply, String>),
    OpenSettings,
    CloseSettings,
    DraftConfigDirChanged(String),
    DraftHostNameChanged(String),
    DraftEditorChanged(String),
    DraftAgentChanged(String),
    OpenConfig,
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
    OpenGit,
    GitBack,
    GitReload,
    GitLoaded(SystemTarget, Result<GitStatusReply, String>),
    GitSelect(Selected),
    GitDiffLoaded(u64, Result<GitDiffReply, String>),
    /// The paths to stage (or, with `false`, unstage); none means every file.
    GitStage(Vec<String>, bool),
    GitStaged(SystemTarget, Result<GitStatusReply, String>),
    GitMessage(text_editor::Action),
    GitCommit,
    GitCommitted(SystemTarget, Result<GitCommitReply, String>),
    GitDismissBanner,
    WindowFocused,
    OpenAi,
    AiBack,
    AiRestart,
    /// The agent sent something; it waits in `Ai::mailbox`. Tagged with the
    /// session it belongs to.
    AiWake(u64),
    AiInput(text_editor::Action),
    AiSend,
    AiAsk(String),
    AiCancel,
    AiAnswer(u64, Option<String>),
    AiMode(String),
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

/// Follows the backend's events. Each goes into `inbox` rather than becoming a
/// message of its own; the UI is only woken when the inbox goes from empty to
/// not empty, and reads it on its next frame.
fn watch(mut client: Client, inbox: Arc<Inbox>) -> Task<Message> {
    Task::future(async move { client.watch().await.map_err(status_text) }).then(move |result| match result {
        Ok(stream) => {
            let inbox = inbox.clone();
            Task::stream(
                stream
                    .filter_map(move |item| {
                        futures::future::ready(match item {
                            Ok(op) => inbox.push(op).then_some(Message::Inbound),
                            Err(error) => Some(Message::WatchFailed(status_text(error))),
                        })
                    })
                    .chain(futures::stream::once(async { Message::WatchEnded })),
            )
        }
        Err(error) => Task::done(Message::WatchFailed(error)),
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
            inbox: Arc::default(),
            config_dir: from_env("NIX_BACKEND_CONFIG_DIR", &saved.config_dir),
            host_name: from_env("NIX_BACKEND_HOST_NAME", &saved.host_name),
            editor: saved.editor.clone(),
            agent: saved.agent.clone(),
            page: Page::Dashboard,
            draft_config_dir: String::new(),
            draft_host_name: String::new(),
            draft_editor: String::new(),
            draft_agent: String::new(),
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
            git: Git::new(),
            ai: Ai::new(),
            fx: Fx::new(),
            last_frame: None,
        };
        let mut app = app;
        app.draft_config_dir = app.config_dir.clone();
        app.draft_host_name = app.host_name.clone();
        app.draft_editor = app.editor.clone();
        app.draft_agent = app.agent.clone();
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

    /// The editor command that will be used: what was typed, else the default.
    pub fn effective_editor(&self) -> &str {
        match self.editor.trim() {
            "" => settings::DEFAULT_EDITOR,
            typed => typed,
        }
    }

    /// The agent command that will be used: what was typed, else the default.
    pub fn effective_agent(&self) -> &str {
        match self.agent.trim() {
            "" => settings::DEFAULT_AGENT,
            typed => typed,
        }
    }

    /// Whether the settings page has edits that aren't applied yet.
    pub fn settings_dirty(&self) -> bool {
        self.draft_config_dir.trim() != self.config_dir.trim()
            || self.draft_host_name.trim() != self.host_name.trim()
            || self.draft_editor.trim() != self.editor.trim()
            || self.draft_agent.trim() != self.agent.trim()
    }

    fn revert_draft(&mut self) {
        self.draft_config_dir = self.config_dir.clone();
        self.draft_host_name = self.host_name.clone();
        self.draft_editor = self.editor.clone();
        self.draft_agent = self.agent.clone();
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
        self.fx.set_looping(
            self.busy()
                || self.store.busy()
                || self.git.busy()
                || self.ai.working()
                || matches!(self.conn, Conn::Connecting),
        );
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
                self.drain_inbox()
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
                Task::batch([self.load_status(), watch(client, self.inbox.clone()), store])
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
            Message::WatchFailed(error) => {
                if matches!(self.conn, Conn::Online(_)) {
                    self.go_offline(error);
                }
                Task::none()
            }
            // While frames are running the next one reads the inbox; the wake-up
            // matters when nothing is moving and so no frame is coming.
            Message::Inbound => {
                if self.fx.animating() {
                    Task::none()
                } else {
                    self.drain_inbox()
                }
            }
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
            Message::DraftEditorChanged(value) => {
                self.draft_editor = value;
                Task::none()
            }
            Message::DraftAgentChanged(value) => {
                self.draft_agent = value;
                Task::none()
            }
            Message::OpenConfig => {
                self.notice = open_in_editor(self.effective_editor(), self.effective_config_dir()).err();
                Task::none()
            }
            Message::RevertDraft => {
                self.revert_draft();
                Task::none()
            }
            Message::UseDefaults => {
                self.draft_config_dir.clear();
                self.draft_host_name.clear();
                self.draft_editor.clear();
                self.draft_agent.clear();
                Task::none()
            }
            Message::ApplySettings => {
                if !self.settings_dirty() {
                    return Task::none();
                }
                let (dir, host, editor, agent) = (
                    self.draft_config_dir.trim().to_owned(),
                    self.draft_host_name.trim().to_owned(),
                    self.draft_editor.trim().to_owned(),
                    self.draft_agent.trim().to_owned(),
                );
                let agent_changed = agent != self.agent.trim();
                let target_changed = dir != self.config_dir.trim() || host != self.host_name.trim();
                // Only fields the user changed are persisted, so a value that
                // merely came from the environment doesn't get saved with them.
                if dir != self.config_dir.trim() {
                    self.saved.config_dir = dir.clone();
                }
                if host != self.host_name.trim() {
                    self.saved.host_name = host.clone();
                }
                self.saved.editor = editor.clone();
                self.saved.agent = agent.clone();
                self.config_dir = dir;
                self.host_name = host;
                self.editor = editor;
                self.agent = agent;
                // A conversation belongs to the agent and the configuration it started with.
                if target_changed || agent_changed {
                    self.ai.stop();
                }
                self.notice = None;
                self.persist();
                if !target_changed {
                    return Task::none();
                }
                // Forget what we knew about the old target.
                self.status = None;
                self.store.reset();
                self.git.reset();
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
                    CopyTarget::Diff => self.git.diff.ready().map(|diff| diff.raw.clone()).unwrap_or_default(),
                    CopyTarget::AiAnswer(index) => self.ai.answer_text(index).unwrap_or_default().to_owned(),
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
            Message::OpenGit => {
                self.show(Page::Git);
                self.load_git()
            }
            Message::GitBack => {
                self.show(Page::Dashboard);
                Task::none()
            }
            Message::GitReload => self.load_git(),
            // Coming back to the window is when files edited elsewhere need to show up.
            Message::WindowFocused => {
                if self.page == Page::Git && !self.git.working {
                    self.load_git()
                } else {
                    Task::none()
                }
            }
            Message::GitLoaded(requested, result) => {
                if requested != self.target() {
                    return Task::none();
                }
                self.git.refreshing = false;
                match result {
                    Ok(status) => {
                        self.git.set_status(status);
                        self.reload_diff()
                    }
                    // Keep showing what we had if there is something.
                    Err(error) if self.git.status.ready().is_some() => {
                        self.git.banner = Some(Banner { text: error, error: true, action: None });
                        Task::none()
                    }
                    Err(error) => {
                        self.git.status = Load::Failed(error);
                        Task::none()
                    }
                }
            }
            Message::GitSelect(selected) => {
                self.git.selected = Some(selected);
                self.git.diff = Load::Loading;
                self.reload_diff()
            }
            Message::GitDiffLoaded(seq, result) => {
                // Only the latest request counts.
                if seq == self.git.diff_seq && self.git.selected.is_some() {
                    self.git.diff = Load::from_result(result.map(|reply| Diff::new(reply.text, reply.truncated)));
                }
                Task::none()
            }
            Message::GitStage(paths, staged) => {
                let Some(mut client) = self.client().filter(|_| !self.git.working) else {
                    return Task::none();
                };
                self.git.working = true;
                self.git.banner = None;
                let target = self.target();
                Task::perform(
                    async move {
                        let result = client.git_set_staged(target.clone(), paths, staged).await.map_err(status_text);
                        (target, result)
                    },
                    |(target, result)| Message::GitStaged(target, result),
                )
            }
            Message::GitStaged(requested, result) => {
                self.git.working = false;
                if requested != self.target() {
                    return Task::none();
                }
                match result {
                    Ok(status) => {
                        self.git.set_status(status);
                        self.reload_diff()
                    }
                    Err(error) => {
                        self.git.banner = Some(Banner { text: error, error: true, action: None });
                        Task::none()
                    }
                }
            }
            Message::GitMessage(action) => {
                self.git.message.perform(action);
                Task::none()
            }
            Message::GitCommit => {
                let Some(mut client) = self.client().filter(|_| self.git.can_commit()) else {
                    return Task::none();
                };
                self.git.working = true;
                self.git.banner = None;
                let target = self.target();
                let message = self.git.message.text();
                Task::perform(
                    async move {
                        let result = client.git_commit(target.clone(), message).await.map_err(status_text);
                        (target, result)
                    },
                    |(target, result)| Message::GitCommitted(target, result),
                )
            }
            Message::GitCommitted(requested, result) => {
                self.git.working = false;
                if requested != self.target() {
                    return Task::none();
                }
                match result {
                    Ok(commit) => {
                        self.git.message = text_editor::Content::new();
                        self.git.banner = Some(Banner {
                            text: format!("Committed {} · {}", commit.hash, commit.subject),
                            error: false,
                            action: None,
                        });
                        // The dashboard shows the latest commit.
                        Task::batch([self.load_git(), self.load_status()])
                    }
                    Err(error) => {
                        self.git.banner = Some(Banner { text: error, error: true, action: None });
                        Task::none()
                    }
                }
            }
            Message::GitDismissBanner => {
                self.git.banner = None;
                Task::none()
            }
            Message::OpenAi => {
                self.show(Page::Ai);
                if matches!(self.ai.phase, ai::Phase::Idle) {
                    self.start_ai()
                } else {
                    Task::none()
                }
            }
            Message::AiBack => {
                self.show(Page::Dashboard);
                Task::none()
            }
            Message::AiRestart => self.start_ai(),
            Message::AiWake(session) => {
                if session == self.ai.session {
                    for event in self.ai.mailbox.take() {
                        self.ai.apply(event);
                    }
                }
                Task::none()
            }
            Message::AiInput(action) => {
                self.ai.input.perform(action);
                Task::none()
            }
            Message::AiSend => {
                self.ai.ask(None);
                Task::none()
            }
            Message::AiAsk(question) => {
                self.ai.ask(Some(question));
                Task::none()
            }
            Message::AiCancel => {
                self.ai.cancel();
                Task::none()
            }
            Message::AiAnswer(permission, option) => {
                self.ai.answer(permission, option);
                Task::none()
            }
            Message::AiMode(mode) => {
                self.ai.set_mode(mode);
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

    /// Starts the agent, ending any conversation there was. What it sends
    /// waits in the mailbox, and the UI is woken when that goes from empty to not.
    fn start_ai(&mut self) -> Task<Message> {
        let config = match ai::config(self) {
            Ok(config) => config,
            Err(reason) => {
                self.ai.stop();
                self.ai.fail(reason);
                return Task::none();
            }
        };
        let (handle, events) = nix_agent_client::start(config);
        let (session, mailbox) = self.ai.begin(handle);
        Task::stream(
            events.filter_map(move |event| {
                futures::future::ready(mailbox.push(event).then_some(Message::AiWake(session)))
            }),
        )
    }

    fn load_git(&mut self) -> Task<Message> {
        let Some(mut client) = self.client() else {
            return Task::none();
        };
        // What is shown stays while it is read again.
        if self.git.status.ready().is_some() {
            self.git.refreshing = true;
        } else {
            self.git.status = Load::Loading;
        }
        let target = self.target();
        Task::perform(
            async move {
                let result = client.git_status(target.clone()).await.map_err(status_text);
                (target, result)
            },
            |(target, result)| Message::GitLoaded(target, result),
        )
    }

    /// Fetches the diff of the selected file.
    fn reload_diff(&mut self) -> Task<Message> {
        let (Some(mut client), Some(selected)) = (self.client(), self.git.selected.clone()) else {
            return Task::none();
        };
        let Some(file) = self.git.selected_file().cloned() else {
            return Task::none();
        };
        self.git.diff_seq += 1;
        let seq = self.git.diff_seq;
        if self.git.diff.ready().is_none() {
            self.git.diff = Load::Loading;
        }
        let target = self.target();
        Task::perform(
            async move { client.git_diff(target, &file, selected.staged).await.map_err(status_text) },
            move |result| Message::GitDiffLoaded(seq, result),
        )
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

    /// Shows what the backend sent since the last frame: one update however
    /// much arrived, and for the state only the newest of it.
    fn drain_inbox(&mut self) -> Task<Message> {
        let segments = self.inbox.take();
        if segments.is_empty() {
            return Task::none();
        }
        let tasks: Vec<Task<Message>> = segments.into_iter().map(|segment| self.apply_segment(segment)).collect();
        self.sync_log();
        Task::batch(tasks)
    }

    /// Folds what arrived for one operation into the activity panel.
    fn apply_segment(&mut self, segment: Segment) -> Task<Message> {
        // The previous run is over (or there was none): this belongs to a new
        // operation, started here or by another instance, or replayed to us as
        // a late joiner.
        if self.run.as_ref().is_none_or(|run| !run.running()) {
            self.run = Some(Run::new(segment.kind, segment.mode));
            self.log_view = text_editor::Content::new();
            self.log_pending.clear();
            self.fx.reset_progress();
            self.fx.restart(Slot::Activity, 0.0);
        }
        let run = self.run.as_mut().expect("run was just ensured");

        run.phase = segment.phase;
        if let Some(status) = segment.status {
            run.message = status;
        }
        let skipped = (segment.skipped > 0).then(|| {
            format!("… {} lines were skipped because they arrived faster than they could be shown", segment.skipped)
        });
        for line in skipped.into_iter().chain(segment.lines) {
            run.log.push_back(line.clone());
            self.log_pending.push(line);
            if run.log.len() > MAX_LOG_LINES {
                run.log.pop_front();
            }
        }
        if let Some((stats, items)) = segment.snapshot {
            run.stats = Some(stats);
            run.items = items;
        }
        if let Some(fraction) = segment.fraction {
            let fraction = fraction as f32;
            // Only ever move forward: nix's totals grow as it discovers more work.
            if run.fraction.is_none_or(|current| fraction > current) {
                run.fraction = Some(fraction);
                self.fx.set_progress(fraction);
            }
        }

        match segment.end {
            Some(End::Finished) => {
                run.outcome = Outcome::Succeeded;
                run.items.clear();
                run.fraction = Some(1.0);
                self.fx.set_progress(1.0);
                // A finished update or rebuild changes what the status card shows,
                // and what the configuration installs.
                self.store.installed = Load::Idle;
                let installed = if self.page == Page::Store { self.load_installed() } else { Task::none() };
                // An update rewrites flake.lock.
                let git = if self.page == Page::Git { self.load_git() } else { Task::none() };
                Task::batch([self.load_status(), installed, git])
            }
            Some(End::Failed(message)) => {
                run.outcome = Outcome::Failed(message);
                run.items.clear();
                self.load_status()
            }
            None => Task::none(),
        }
    }

    fn view(&self) -> iced::Element<'_, Message> {
        view::view(self)
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = Vec::new();
        // Frames also run while events wait, so the next one shows them.
        if self.fx.animating() || self.inbox.has_pending() {
            subscriptions.push(window::frames().map(Message::Frame));
        }
        if self.page == Page::Git {
            subscriptions.push(iced::event::listen_with(|event, _, _| match event {
                iced::Event::Window(window::Event::Focused) => Some(Message::WindowFocused),
                _ => None,
            }));
        }
        if matches!(self.conn, Conn::Offline(_)) {
            subscriptions.push(iced::time::every(Duration::from_secs(2)).map(|_| Message::Retry));
        }
        Subscription::batch(subscriptions)
    }
}

/// Starts the editor on the configuration directory and leaves it running on
/// its own. `editor` is a command line such as `zed` or `code --new-window`.
fn open_in_editor(editor: &str, config_dir: &str) -> Result<(), String> {
    let dir = config_dir.strip_prefix("path:").unwrap_or(config_dir);
    if !std::path::Path::new(dir).is_dir() {
        return Err(format!("{dir} is not a directory on this machine, so there is nothing to open."));
    }
    let mut words = settings::split_command(editor).into_iter();
    let Some(program) = words.next() else {
        return Err("No editor is set. Enter one in Settings.".to_owned());
    };
    let mut child = std::process::Command::new(&program)
        .args(words)
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| format!("Could not start '{program}': {error}. Set the editor to use in Settings."))?;
    // Reaped off-thread so it doesn't linger as a zombie when it exits.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
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

    #[test]
    fn the_editor_gets_its_arguments_and_the_directory() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("nix-manager-editor-{}", std::process::id()));
        let config = root.join("my config");
        std::fs::create_dir_all(&config).unwrap();
        let record = root.join("record");
        let editor = root.join("fake editor");
        std::fs::write(&editor, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", record.display())).unwrap();
        std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();

        let command = format!("'{}' --new-window", editor.display());
        open_in_editor(&command, config.to_str().unwrap()).unwrap();

        // It runs on its own, so wait for it to have written its arguments.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !record.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let args = std::fs::read_to_string(&record).unwrap();
        assert_eq!(args.lines().collect::<Vec<_>>(), ["--new-window", config.to_str().unwrap()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn opening_the_config_explains_what_went_wrong() {
        let dir = std::env::temp_dir();
        let dir = dir.to_str().unwrap();
        let missing = open_in_editor("definitely-not-an-editor-xyz", dir).unwrap_err();
        assert!(missing.contains("definitely-not-an-editor-xyz") && missing.contains("Settings"), "{missing}");
        assert!(open_in_editor("  ", dir).unwrap_err().contains("No editor"));
        assert!(open_in_editor("zed", "/no/such/dir").unwrap_err().contains("not a directory"));
    }

    fn backend_event(phase: EventPhase, message: &str, builds_done: Option<i64>) -> nix_backend_client::OperationEvent {
        use nix_backend_client::proto::ProgressEvent;
        nix_backend_client::OperationEvent {
            kind: OperationKind::Rebuild as i32,
            mode: RebuildMode::BuildOnly as i32,
            event: Some(ProgressEvent {
                phase: phase as i32,
                message: message.to_owned(),
                is_error: false,
                fraction_done: builds_done.map_or(-1.0, |done| done as f64 / 1000.0),
                active_items: Vec::new(),
                stats: builds_done.map(|builds_done| BuildStats { builds_done, ..Default::default() }),
            }),
        }
    }

    #[test]
    fn a_burst_is_shown_as_its_newest_state_and_all_its_output() {
        let (mut app, _) = App::new();
        for i in 1..=1000 {
            app.inbox.push(backend_event(EventPhase::Build, &format!("line {i}"), Some(i)));
        }

        // One read, however much arrived.
        let _ = app.drain_inbox();
        let run = app.run.as_ref().expect("a run was started");
        assert_eq!(run.log.len(), 1000, "no output line is lost");
        assert_eq!(run.log.front().map(String::as_str), Some("line 1"));
        assert_eq!(run.message, "line 1000", "the status is the newest");
        assert_eq!(run.stats.as_ref().unwrap().builds_done, 1000, "so is the snapshot");
        assert_eq!(run.fraction, Some(1.0));
        assert!(run.running());
        assert!(!app.inbox.has_pending());
    }

    #[test]
    fn a_finish_in_the_same_burst_is_not_lost() {
        let (mut app, _) = App::new();
        app.inbox.push(backend_event(EventPhase::Build, "working", Some(5)));
        app.inbox.push(backend_event(EventPhase::Finished, "Build complete", None));

        let _ = app.drain_inbox();
        let run = app.run.as_ref().unwrap();
        assert!(matches!(run.outcome, Outcome::Succeeded));
        assert!(run.items.is_empty());
    }

    #[test]
    fn a_failure_in_the_same_burst_carries_its_text() {
        let (mut app, _) = App::new();
        app.inbox.push(backend_event(EventPhase::Build, "working", None));
        app.inbox.push(backend_event(EventPhase::Failed, "Build failed: boom", None));

        let _ = app.drain_inbox();
        assert!(matches!(&app.run.as_ref().unwrap().outcome, Outcome::Failed(text) if text == "Build failed: boom"));
    }

    #[test]
    fn the_next_operation_starts_a_fresh_run() {
        let (mut app, _) = App::new();
        app.inbox.push(backend_event(EventPhase::Finished, "done", None));
        let _ = app.drain_inbox();
        assert!(!app.run.as_ref().unwrap().running());

        app.inbox.push(backend_event(EventPhase::Build, "again", Some(1)));
        let _ = app.drain_inbox();
        let run = app.run.as_ref().unwrap();
        assert!(run.running());
        assert_eq!(run.log.len(), 1);
    }
}
