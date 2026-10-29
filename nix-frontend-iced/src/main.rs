mod fx;
mod theme;
mod view;

use std::time::{Duration, Instant};

use futures::StreamExt;
use iced::{window, Subscription, Task};
use nix_backend_client::settings::Settings;
use nix_backend_client::{
    BuildItem, BuildStats, Client, EventPhase, FlakeStatusReply, OperationEvent, OperationKind, ProgressEvent,
    RebuildMode, SystemTarget,
};

use fx::{Fx, Slot};

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

pub struct LogLine {
    pub phase: EventPhase,
    pub text: String,
    pub is_error: bool,
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
    pub log: Vec<LogLine>,
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
            log: Vec::new(),
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
    pub config_dir: String,
    pub host_name: String,
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
    pub copied: Option<CopyTarget>,
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
    ConfigDirChanged(String),
    HostNameChanged(String),
    Start(Action),
    StartRejected(String),
    Hover(usize, bool),
    ToggleLog,
    Copy(CopyTarget),
    DismissRun,
    DismissNotice,
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
            copied: None,
            fx: Fx::new(),
            last_frame: None,
        };
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
        self.fx.set_looping(self.busy() || matches!(self.conn, Conn::Connecting));
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
                Task::batch([self.load_status(), watch(client)])
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
            Message::ConfigDirChanged(value) => {
                self.saved.config_dir = value.trim().to_owned();
                self.config_dir = value;
                self.persist();
                Task::none()
            }
            Message::HostNameChanged(value) => {
                self.saved.host_name = value.trim().to_owned();
                self.host_name = value;
                self.persist();
                Task::none()
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
            Message::Copy(target) => {
                let Some(run) = &self.run else { return Task::none() };
                let text = match target {
                    CopyTarget::Message => match &run.outcome {
                        Outcome::Failed(message) => message.clone(),
                        _ => run.message.clone(),
                    },
                    CopyTarget::Output => run.log.iter().map(|line| line.text.as_str()).collect::<Vec<_>>().join("\n"),
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
                }
                Task::none()
            }
            Message::DismissNotice => {
                self.notice = None;
                Task::none()
            }
        }
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
            self.fx.reset_progress();
            self.fx.restart(Slot::Activity, 0.0);
        }
        let run = self.run.as_mut().expect("run was just ensured");

        let phase = event.phase();
        run.phase = phase;
        let message = nix_backend_client::strip_ansi(&event.message);
        if !message.is_empty() {
            run.message = message.lines().next().unwrap_or_default().to_owned();
            run.log.push(LogLine { phase, text: message.clone(), is_error: event.is_error });
            if run.log.len() > MAX_LOG_LINES {
                run.log.drain(..run.log.len() - MAX_LOG_LINES);
            }
        }
        absorb_progress(run, &event, &mut self.fx);

        match phase {
            EventPhase::Finished => {
                run.outcome = Outcome::Succeeded;
                run.items.clear();
                run.fraction = Some(1.0);
                self.fx.set_progress(1.0);
                // A finished update or rebuild changes what the status card shows.
                return self.load_status();
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
