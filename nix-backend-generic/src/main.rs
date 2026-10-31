mod activity;
mod config;
mod destinations;
mod git;
mod nix_ops;
mod store;

pub mod proto {
    tonic::include_proto!("caelestia.nixbackend.v1");
}

use std::os::unix::fs::PermissionsExt;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use futures::Stream;
use tokio::net::UnixListener;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::{BroadcastStream, ReceiverStream, UnixListenerStream};
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status};

use proto::nix_backend_server::{NixBackend, NixBackendServer};
use proto::{
    CreateDestinationRequest, Destination, FlakeStatusReply, FlakeStatusRequest, GetPackageRequest, GitCommitReply,
    GitCommitRequest, GitDiffReply, GitDiffRequest, GitSetStagedRequest, GitStatusReply, GitStatusRequest,
    ListDestinationsReply, ListDestinationsRequest, ListInstalledReply, ListInstalledRequest, OperationEvent,
    OperationKind, Package, ProgressEvent, RebuildMode, RebuildRequest, SearchPackagesReply, SearchPackagesRequest,
    SetConfigPackageReply, SetConfigPackageRequest, UpdateFlakeRequest, WatchRequest,
};

type EventStream = Pin<Box<dyn Stream<Item = Result<ProgressEvent, Status>> + Send>>;
type OperationStream = Pin<Box<dyn Stream<Item = Result<OperationEvent, Status>> + Send>>;

/// Shared by every connection, so any number of frontends can be attached to
/// one backend: it enforces one running operation at a time and fans the
/// running operation's events out to every `Watch` subscriber.
struct Hub {
    running: Mutex<Option<OperationEvent>>,
    events: broadcast::Sender<OperationEvent>,
}

impl Hub {
    fn new() -> Arc<Self> {
        Arc::new(Self { running: Mutex::new(None), events: broadcast::channel(1024).0 })
    }

    /// Claims the single operation slot.
    fn begin(&self, kind: OperationKind, mode: RebuildMode) -> Result<(), Status> {
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if running.is_some() {
            return Err(Status::failed_precondition("another operation is already running"));
        }
        *running = Some(OperationEvent { kind: kind as i32, mode: mode as i32, event: None });
        Ok(())
    }

    fn publish(&self, kind: OperationKind, mode: RebuildMode, event: &ProgressEvent) {
        let tagged = OperationEvent { kind: kind as i32, mode: mode as i32, event: Some(event.clone()) };
        *self.running.lock().unwrap_or_else(|e| e.into_inner()) = Some(tagged.clone());
        // No subscribers is fine.
        let _ = self.events.send(tagged);
    }

    fn end(&self) {
        *self.running.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Subscribes first, then snapshots, so no event can fall in the gap.
    fn subscribe(&self) -> (broadcast::Receiver<OperationEvent>, Option<OperationEvent>) {
        let rx = self.events.subscribe();
        let snapshot = self.running.lock().unwrap_or_else(|e| e.into_inner()).clone().filter(|op| op.event.is_some());
        (rx, snapshot)
    }
}

/// Forwards an operation's events to the client that started it and to the
/// hub. The starter disconnecting must not abort the operation (a rebuild
/// half-way through activation is worse than one nobody is watching), so
/// send failures to it are ignored and the operation is always drained.
async fn pump(
    hub: Arc<Hub>,
    kind: OperationKind,
    mode: RebuildMode,
    mut rx: mpsc::Receiver<Result<ProgressEvent, Status>>,
    out: mpsc::Sender<Result<ProgressEvent, Status>>,
) {
    while let Some(item) = rx.recv().await {
        if let Ok(event) = &item {
            hub.publish(kind, mode, event);
        }
        let _ = out.send(item).await;
    }
    hub.end();
}

struct Service {
    hub: Arc<Hub>,
}

#[tonic::async_trait]
impl NixBackend for Service {
    type UpdateFlakeStream = EventStream;
    type RebuildStream = EventStream;
    type WatchStream = OperationStream;

    async fn update_flake(&self, request: Request<UpdateFlakeRequest>) -> Result<Response<Self::UpdateFlakeStream>, Status> {
        let target = config::resolve(request.into_inner().target.as_ref());
        self.hub.begin(OperationKind::UpdateFlake, RebuildMode::Unspecified)?;
        let (tx, rx) = mpsc::channel(64);
        let (out_tx, out_rx) = mpsc::channel(64);

        tokio::spawn(async move {
            nix_ops::update_flake(&target, tx).await;
        });
        tokio::spawn(pump(self.hub.clone(), OperationKind::UpdateFlake, RebuildMode::Unspecified, rx, out_tx));

        Ok(Response::new(Box::pin(ReceiverStream::new(out_rx))))
    }

    async fn rebuild(&self, request: Request<RebuildRequest>) -> Result<Response<Self::RebuildStream>, Status> {
        let req = request.into_inner();
        let target = config::resolve(req.target.as_ref());
        // An unknown value (a newer client talking to this backend) must never
        // fall back to a mode that activates the system.
        let mode = RebuildMode::try_from(req.mode)
            .map_err(|_| Status::invalid_argument(format!("unknown rebuild mode {}", req.mode)))?;
        self.hub.begin(OperationKind::Rebuild, mode)?;
        let (tx, rx) = mpsc::channel(64);
        let (out_tx, out_rx) = mpsc::channel(64);

        tokio::spawn(async move {
            nix_ops::rebuild(&target, mode, &req.nix_args, tx).await;
        });
        tokio::spawn(pump(self.hub.clone(), OperationKind::Rebuild, mode, rx, out_tx));

        Ok(Response::new(Box::pin(ReceiverStream::new(out_rx))))
    }

    async fn watch(&self, _request: Request<WatchRequest>) -> Result<Response<Self::WatchStream>, Status> {
        let (rx, snapshot) = self.hub.subscribe();
        // A subscriber that falls too far behind skips the oldest events it
        // missed and carries on with the newest ones (so it still sees the
        // terminal event); every event carries a full snapshot anyway.
        let live = BroadcastStream::new(rx).filter_map(|item| item.ok().map(Ok));
        let stream = tokio_stream::iter(snapshot.into_iter().map(Ok)).chain(live);
        Ok(Response::new(Box::pin(stream)))
    }

    async fn get_flake_status(&self, request: Request<FlakeStatusRequest>) -> Result<Response<FlakeStatusReply>, Status> {
        let target = config::resolve(request.into_inner().target.as_ref());
        Ok(Response::new(nix_ops::flake_status(&target).await))
    }

    async fn search_packages(
        &self,
        request: Request<SearchPackagesRequest>,
    ) -> Result<Response<SearchPackagesReply>, Status> {
        let req = request.into_inner();
        let target = config::resolve(req.target.as_ref());
        Ok(Response::new(store::search(&target, &req.query, req.limit).await?))
    }

    async fn get_package(&self, request: Request<GetPackageRequest>) -> Result<Response<Package>, Status> {
        let req = request.into_inner();
        let target = config::resolve(req.target.as_ref());
        Ok(Response::new(store::package(&target, &req.attr).await?))
    }

    async fn list_installed_packages(
        &self,
        request: Request<ListInstalledRequest>,
    ) -> Result<Response<ListInstalledReply>, Status> {
        let target = config::resolve(request.into_inner().target.as_ref());
        Ok(Response::new(store::installed(&target).await?))
    }

    async fn list_destinations(
        &self,
        request: Request<ListDestinationsRequest>,
    ) -> Result<Response<ListDestinationsReply>, Status> {
        let target = config::resolve(request.into_inner().target.as_ref());
        let destinations = blocking(move || destinations::list(&target.config_dir)).await?;
        Ok(Response::new(ListDestinationsReply { destinations }))
    }

    async fn create_destination(&self, request: Request<CreateDestinationRequest>) -> Result<Response<Destination>, Status> {
        let req = request.into_inner();
        let target = config::resolve(req.target.as_ref());
        Ok(Response::new(blocking(move || destinations::create(&target.config_dir, &req.name)).await?))
    }

    async fn set_config_package(
        &self,
        request: Request<SetConfigPackageRequest>,
    ) -> Result<Response<SetConfigPackageReply>, Status> {
        let req = request.into_inner();
        let target = config::resolve(req.target.as_ref());
        let reply =
            blocking(move || destinations::set_package(&target.config_dir, &req.file, &req.attr, req.present)).await?;
        Ok(Response::new(reply))
    }

    async fn git_status(&self, request: Request<GitStatusRequest>) -> Result<Response<GitStatusReply>, Status> {
        let target = config::resolve(request.into_inner().target.as_ref());
        Ok(Response::new(blocking(move || git::status(&target.config_dir)).await?))
    }

    async fn git_set_staged(&self, request: Request<GitSetStagedRequest>) -> Result<Response<GitStatusReply>, Status> {
        let req = request.into_inner();
        let target = config::resolve(req.target.as_ref());
        let reply = blocking(move || git::set_staged(&target.config_dir, &req.paths, req.staged)).await?;
        Ok(Response::new(reply))
    }

    async fn git_commit(&self, request: Request<GitCommitRequest>) -> Result<Response<GitCommitReply>, Status> {
        let req = request.into_inner();
        let target = config::resolve(req.target.as_ref());
        Ok(Response::new(blocking(move || git::commit(&target.config_dir, &req.message)).await?))
    }

    async fn git_diff(&self, request: Request<GitDiffRequest>) -> Result<Response<GitDiffReply>, Status> {
        let req = request.into_inner();
        let target = config::resolve(req.target.as_ref());
        let reply = blocking(move || git::diff(&target.config_dir, &req.path, &req.old_path, req.staged)).await?;
        Ok(Response::new(reply))
    }
}

/// Runs file-system work off the async threads.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, Status> + Send + 'static) -> Result<T, Status> {
    tokio::task::spawn_blocking(work).await.map_err(|e| Status::internal(format!("worker failed: {e}")))?
}

/// The daemon is usually spawned by the shell, whose systemd unit runs with a
/// minimal PATH that lacks the system/user profile dirs - so `nix`, `git` and
/// `pkexec` wouldn't resolve. Append the standard NixOS locations (after
/// whatever is already there, so an explicit PATH still wins).
fn ensure_system_path() {
    let mut dirs: Vec<std::path::PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();

    let mut extra = vec![
        std::path::PathBuf::from("/run/wrappers/bin"),
        std::path::PathBuf::from("/run/current-system/sw/bin"),
        std::path::PathBuf::from("/nix/var/nix/profiles/default/bin"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        extra.push(std::path::Path::new(&home).join(".nix-profile/bin"));
    }
    if let Some(user) = std::env::var_os("USER") {
        extra.push(std::path::Path::new("/etc/profiles/per-user").join(user).join("bin"));
    }

    for dir in extra {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }

    if let Ok(joined) = std::env::join_paths(dirs) {
        std::env::set_var("PATH", joined);
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    ensure_system_path();

    let socket_path = config::socket_path();
    // Leftover socket from a crashed previous run - a fresh bind fails with
    // AddrInUse otherwise.
    let _ = std::fs::remove_file(&socket_path);
    if let Some(parent) = std::path::Path::new(&socket_path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(&socket_path)?;
    // XDG_RUNTIME_DIR is already 0700 for the owning user, but the socket
    // itself defaults to whatever umask is active - pin it down explicitly
    // rather than rely on that.
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;

    tracing::info!("nix-backend-generic listening on {socket_path}");

    tonic::transport::Server::builder()
        .add_service(NixBackendServer::new(Service { hub: Hub::new() }))
        .serve_with_incoming(UnixListenerStream::new(listener))
        .await?;

    Ok(())
}
