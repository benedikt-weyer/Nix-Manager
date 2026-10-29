use std::env;

use crate::proto::SystemTarget;

/// A fully resolved `SystemTarget`: never empty, so callers don't need to
/// keep threading the "what if it's unset" case through every nix
/// invocation.
pub struct ResolvedTarget {
    pub config_dir: String,
    pub host_name: String,
}

/// Resolves a request's `SystemTarget` against the daemon's own fallbacks:
/// the request wins when set, otherwise `NIX_BACKEND_CONFIG_DIR` /
/// `NIX_BACKEND_HOST_NAME`, otherwise `/etc/nixos` and the machine's
/// hostname. This mirrors the shell's Nexus settings (system config dir /
/// host name) which are sent as the request's `SystemTarget` when set.
pub fn resolve(target: Option<&SystemTarget>) -> ResolvedTarget {
    let requested_dir = target.map(|t| t.config_dir.trim()).unwrap_or("");
    let requested_host = target.map(|t| t.host_name.trim()).unwrap_or("");

    let config_dir = if !requested_dir.is_empty() {
        requested_dir.to_owned()
    } else if let Ok(dir) = env::var("NIX_BACKEND_CONFIG_DIR") {
        if !dir.trim().is_empty() {
            dir
        } else {
            default_config_dir()
        }
    } else {
        default_config_dir()
    };

    let host_name = if !requested_host.is_empty() {
        requested_host.to_owned()
    } else if let Ok(host) = env::var("NIX_BACKEND_HOST_NAME") {
        if !host.trim().is_empty() {
            host
        } else {
            default_host_name()
        }
    } else {
        default_host_name()
    };

    ResolvedTarget { config_dir, host_name }
}

fn default_config_dir() -> String {
    "/etc/nixos".to_owned()
}

fn default_host_name() -> String {
    hostname_from_system().unwrap_or_else(|| "default".to_owned())
}

fn hostname_from_system() -> Option<String> {
    let raw = std::fs::read_to_string("/proc/sys/kernel/hostname").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// The unix socket path the server listens on and the shell's C++ client
/// connects to. Overridable for tests/sandboxes; defaults under
/// `$XDG_RUNTIME_DIR` so it's per-user and cleaned up by the OS.
pub fn socket_path() -> String {
    if let Ok(path) = env::var("NIX_BACKEND_SOCKET") {
        if !path.trim().is_empty() {
            return path;
        }
    }

    let runtime_dir = env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
    format!("{runtime_dir}/nix-backend-generic.sock")
}
