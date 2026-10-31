//! Frontend-agnostic client for the Nix-Manager backend.
//!
//! Every frontend (iced, and whatever comes next) talks to the backend through
//! this crate, so connecting, request building and the presentation-neutral
//! helpers live in one place and a new frontend is only UI code.

// tonic::Status is large and every tonic call returns it.
#![allow(clippy::result_large_err)]

pub mod settings;

use std::path::{Path, PathBuf};

use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

pub use tonic::{Status, Streaming};

pub mod proto {
    tonic::include_proto!("caelestia.nixbackend.v1");
}

pub mod diff;

pub use proto::{
    BuildItem, BuildItemKind, BuildItemStatus, BuildStats, Destination, EventPhase, FlakeStatusReply, GitChange,
    GitCommitReply, GitDiffReply, GitFile, GitStatusReply, Maintainer, OperationEvent, OperationKind, Package,
    PackageSummary, ProgressEvent, RebuildMode, SearchPackagesReply, SetConfigPackageReply, SystemTarget,
};

use proto::nix_backend_client::NixBackendClient;
use proto::{
    CreateDestinationRequest, FlakeStatusRequest, GetPackageRequest, GitCommitRequest, GitDiffRequest,
    GitSetStagedRequest, GitStatusRequest, ListDestinationsRequest, ListInstalledRequest, RebuildRequest,
    SearchPackagesRequest, SetConfigPackageRequest, UpdateFlakeRequest, WatchRequest,
};

impl GitFile {
    /// What is staged for the next commit.
    pub fn staged_change(&self) -> GitChange {
        GitChange::try_from(self.staged).unwrap_or(GitChange::None)
    }

    /// What differs between the index and the working tree.
    pub fn unstaged_change(&self) -> GitChange {
        GitChange::try_from(self.unstaged).unwrap_or(GitChange::None)
    }

    pub fn is_staged(&self) -> bool {
        self.staged_change() != GitChange::None
    }

    pub fn is_unstaged(&self) -> bool {
        self.unstaged_change() != GitChange::None
    }

    /// The paths to hand to [`Client::git_set_staged`] to move this file. A
    /// rename has two: staging or unstaging only the new name would leave the
    /// old one behind as a deletion.
    pub fn paths(&self) -> Vec<String> {
        let mut paths = vec![self.path.clone()];
        paths.extend((!self.old_path.is_empty()).then(|| self.old_path.clone()));
        paths
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("backend socket {0} does not exist - is the backend running?")]
    NoSocket(PathBuf),
    #[error("could not connect to {path}: {source}")]
    Transport { path: PathBuf, source: tonic::transport::Error },
}

/// The unix socket the backend listens on. Mirrors the backend's own
/// resolution (`NIX_BACKEND_SOCKET`, then `$XDG_RUNTIME_DIR`) so a frontend
/// started from the same environment finds the same daemon.
pub fn socket_path() -> PathBuf {
    if let Ok(path) = std::env::var("NIX_BACKEND_SOCKET") {
        if !path.trim().is_empty() {
            return PathBuf::from(path);
        }
    }

    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
    Path::new(&runtime_dir).join("nix-backend-generic.sock")
}

#[derive(Clone, Debug)]
pub struct Client {
    inner: NixBackendClient<Channel>,
}

impl Client {
    pub async fn connect(socket: impl AsRef<Path>) -> Result<Self, ConnectError> {
        let path = socket.as_ref().to_owned();
        if !path.exists() {
            return Err(ConnectError::NoSocket(path));
        }

        // The URI is required by tonic but never used - the connector below
        // ignores it and dials the unix socket.
        let connect_path = path.clone();
        let channel = Endpoint::try_from("http://backend.invalid")
            .expect("static URI is valid")
            .connect_with_connector(service_fn(move |_: Uri| {
                let path = connect_path.clone();
                async move { tokio::net::UnixStream::connect(path).await }
            }))
            .await
            .map_err(|source| ConnectError::Transport { path, source })?;

        Ok(Self { inner: NixBackendClient::new(channel) })
    }

    pub async fn flake_status(&mut self, target: SystemTarget) -> Result<FlakeStatusReply, Status> {
        let reply = self.inner.get_flake_status(FlakeStatusRequest { target: Some(target) }).await?;
        Ok(reply.into_inner())
    }

    /// Searches the nixpkgs the target's flake has locked. `limit` of 0 means
    /// the backend's default. The first search against a new nixpkgs revision
    /// can take a while.
    pub async fn search_packages(
        &mut self,
        target: SystemTarget,
        query: String,
        limit: u32,
    ) -> Result<SearchPackagesReply, Status> {
        let request = SearchPackagesRequest { target: Some(target), query, limit };
        Ok(self.inner.search_packages(request).await?.into_inner())
    }

    /// Everything known about one package, by its nixpkgs attribute.
    pub async fn get_package(&mut self, target: SystemTarget, attr: String) -> Result<Package, Status> {
        let request = GetPackageRequest { target: Some(target), attr };
        Ok(self.inner.get_package(request).await?.into_inner())
    }

    /// The packages the target's NixOS configuration installs.
    pub async fn list_installed_packages(&mut self, target: SystemTarget) -> Result<Vec<Package>, Status> {
        let reply = self.inner.list_installed_packages(ListInstalledRequest { target: Some(target) }).await?;
        Ok(reply.into_inner().packages)
    }

    /// The files packages are added to: the default first, then the others.
    pub async fn list_destinations(&mut self, target: SystemTarget) -> Result<Vec<Destination>, Status> {
        let reply = self.inner.list_destinations(ListDestinationsRequest { target: Some(target) }).await?;
        Ok(reply.into_inner().destinations)
    }

    /// Creates `nix-manager-<name>.nix`, or returns it if it is already there.
    pub async fn create_destination(&mut self, target: SystemTarget, name: String) -> Result<Destination, Status> {
        let request = CreateDestinationRequest { target: Some(target), name };
        Ok(self.inner.create_destination(request).await?.into_inner())
    }

    /// Adds `attr` to, or (`present` false) removes it from, the destination
    /// `file`. Adding to the default destination creates it if need be.
    pub async fn set_config_package(
        &mut self,
        target: SystemTarget,
        file: String,
        attr: String,
        present: bool,
    ) -> Result<SetConfigPackageReply, Status> {
        let request = SetConfigPackageRequest { target: Some(target), file, attr, present };
        Ok(self.inner.set_config_package(request).await?.into_inner())
    }

    /// The git repository the configuration lives in: branch and changed files.
    /// `is_repo` is false when the directory isn't in one.
    pub async fn git_status(&mut self, target: SystemTarget) -> Result<GitStatusReply, Status> {
        Ok(self.inner.git_status(GitStatusRequest { target: Some(target) }).await?.into_inner())
    }

    /// Stages (`staged` true) or unstages `paths`, relative to the repository
    /// root; empty means every file. Returns the status afterwards.
    pub async fn git_set_staged(
        &mut self,
        target: SystemTarget,
        paths: Vec<String>,
        staged: bool,
    ) -> Result<GitStatusReply, Status> {
        let request = GitSetStagedRequest { target: Some(target), paths, staged };
        Ok(self.inner.git_set_staged(request).await?.into_inner())
    }

    /// Commits what is staged.
    pub async fn git_commit(&mut self, target: SystemTarget, message: String) -> Result<GitCommitReply, Status> {
        let request = GitCommitRequest { target: Some(target), message };
        Ok(self.inner.git_commit(request).await?.into_inner())
    }

    /// The unified diff of one file, staged or not. Parse it with [`diff::parse`].
    pub async fn git_diff(
        &mut self,
        target: SystemTarget,
        file: &GitFile,
        staged: bool,
    ) -> Result<GitDiffReply, Status> {
        let request =
            GitDiffRequest { target: Some(target), path: file.path.clone(), old_path: file.old_path.clone(), staged };
        Ok(self.inner.git_diff(request).await?.into_inner())
    }

    /// Every event of every operation, whoever started it. Frontends should
    /// drive their progress UI from this alone: it keeps several attached
    /// instances in sync, and it replays the running operation to late
    /// joiners.
    pub async fn watch(&mut self) -> Result<Streaming<OperationEvent>, Status> {
        Ok(self.inner.watch(WatchRequest {}).await?.into_inner())
    }

    /// Starts a flake update. Fails with `FAILED_PRECONDITION` if the backend
    /// is already busy. The returned stream mirrors what [`Client::watch`]
    /// yields for this operation and can simply be dropped - the operation
    /// keeps running.
    pub async fn update_flake(&mut self, target: SystemTarget) -> Result<Streaming<ProgressEvent>, Status> {
        let reply = self.inner.update_flake(UpdateFlakeRequest { target: Some(target) }).await?;
        Ok(reply.into_inner())
    }

    pub async fn rebuild(
        &mut self,
        target: SystemTarget,
        mode: RebuildMode,
        nix_args: Vec<String>,
        build_as_root: bool,
    ) -> Result<Streaming<ProgressEvent>, Status> {
        let reply = self
            .inner
            .rebuild(RebuildRequest { target: Some(target), mode: mode as i32, nix_args, build_as_root })
            .await?;
        Ok(reply.into_inner())
    }
}

/// Short human-readable name for a phase, for step indicators and log tags.
pub fn phase_label(phase: EventPhase) -> &'static str {
    match phase {
        EventPhase::PhaseUnspecified => "Waiting",
        EventPhase::Started => "Started",
        EventPhase::FlakeUpdate => "Update",
        EventPhase::Build => "Build",
        EventPhase::Diff => "Diff",
        EventPhase::Register => "Register",
        EventPhase::Activate => "Activate",
        EventPhase::Finished => "Finished",
        EventPhase::Failed => "Failed",
    }
}

/// `1536.0` -> `"1.5 KiB"`.
pub fn format_bytes(bytes: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes.max(0.0);
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Removes ANSI escape sequences (nix colours its messages even when piped).
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // CSI sequence: ESC [ params... final byte in @..~
        if chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_colour_codes() {
        assert_eq!(strip_ansi("\u{1b}[31;1merror:\u{1b}[0m boom"), "error: boom");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(format_bytes(512.0), "512 B");
        assert_eq!(format_bytes(1536.0), "1.5 KiB");
    }
}
