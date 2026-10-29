mod activity;
mod config;
mod nix_ops;

pub mod proto {
    tonic::include_proto!("caelestia.nixbackend.v1");
}

use std::os::unix::fs::PermissionsExt;
use std::pin::Pin;

use futures::Stream;
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, UnixListenerStream};
use tonic::{Request, Response, Status};

use proto::nix_backend_server::{NixBackend, NixBackendServer};
use proto::{FlakeStatusReply, FlakeStatusRequest, ProgressEvent, RebuildMode, RebuildRequest, UpdateFlakeRequest};

type EventStream = Pin<Box<dyn Stream<Item = Result<ProgressEvent, Status>> + Send>>;

#[derive(Default)]
struct Service;

#[tonic::async_trait]
impl NixBackend for Service {
    type UpdateFlakeStream = EventStream;
    type RebuildStream = EventStream;

    async fn update_flake(&self, request: Request<UpdateFlakeRequest>) -> Result<Response<Self::UpdateFlakeStream>, Status> {
        let target = config::resolve(request.into_inner().target.as_ref());
        let (tx, rx) = mpsc::channel(64);

        tokio::spawn(async move {
            nix_ops::update_flake(&target, tx).await;
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn rebuild(&self, request: Request<RebuildRequest>) -> Result<Response<Self::RebuildStream>, Status> {
        let req = request.into_inner();
        let target = config::resolve(req.target.as_ref());
        let mode = RebuildMode::try_from(req.mode).unwrap_or(RebuildMode::Switch);
        let (tx, rx) = mpsc::channel(64);

        tokio::spawn(async move {
            nix_ops::rebuild(&target, mode, tx).await;
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn get_flake_status(&self, request: Request<FlakeStatusRequest>) -> Result<Response<FlakeStatusReply>, Status> {
        let target = config::resolve(request.into_inner().target.as_ref());
        Ok(Response::new(nix_ops::flake_status(&target).await))
    }
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
        .add_service(NixBackendServer::new(Service))
        .serve_with_incoming(UnixListenerStream::new(listener))
        .await?;

    Ok(())
}
