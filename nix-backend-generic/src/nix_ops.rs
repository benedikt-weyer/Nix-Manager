use std::process::Stdio;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::Sender;
use tonic::Status;

use crate::activity::{self, ActivityTracker};
use crate::config::ResolvedTarget;
use crate::proto::{EventPhase, FlakeStatusReply, ProgressEvent, RebuildMode};

pub type EventTx = Sender<Result<ProgressEvent, Status>>;

async fn send(tx: &EventTx, phase: EventPhase, message: impl Into<String>, is_error: bool) {
    let _ = tx
        .send(Ok(ProgressEvent {
            phase: phase as i32,
            message: message.into(),
            is_error,
            fraction_done: -1.0,
            active_items: Vec::new(),
            stats: None,
        }))
        .await;
}

async fn forward_line(tx: &EventTx, phase: EventPhase, line: &str, tracker: &Mutex<ActivityTracker>) {
    let line = line.trim_end();
    if line.is_empty() {
        return;
    }

    let message = match line.strip_prefix("@nix ") {
        Some(json) => match serde_json::from_str::<serde_json::Value>(json) {
            Ok(value) => {
                let message = activity::extract_message(&value);
                tracker.lock().unwrap_or_else(|e| e.into_inner()).handle_json(&value);
                message
            }
            Err(_) => Some(json.to_owned()),
        },
        None => Some(line.to_owned()),
    };

    let (active_items, stats) = tracker.lock().unwrap_or_else(|e| e.into_inner()).snapshot();
    let fraction = activity::fraction_done(&stats);

    let _ = tx
        .send(Ok(ProgressEvent {
            phase: phase as i32,
            message: message.unwrap_or_default(),
            is_error: false,
            fraction_done: fraction,
            active_items,
            stats: Some(stats),
        }))
        .await;
}

/// A line a privileged helper script prints to move the stream on to the next
/// phase: `@@nix-manager-phase:ACTIVATE:Activating (boot)`. This lets one
/// authorized process (one polkit prompt) cover several UI phases.
const PHASE_MARKER: &str = "@@nix-manager-phase:";

/// Registers the built system as the current generation, then activates it,
/// all under a single `pkexec` so the user authorizes once. Arguments are
/// positional (never interpolated into the script): $1 nix-env, $2 profile,
/// $3 built system, $4 switch-to-configuration, $5 switch|boot.
const REGISTER_AND_ACTIVATE: &str = r#"set -e
"$1" --profile "$2" --set "$3"
echo "@@nix-manager-phase:ACTIVATE:Activating ($5)"
exec "$4" "$5"
"#;

const SYSTEM_PROFILE: &str = "/nix/var/nix/profiles/system";

fn current_phase(phase: &AtomicI32) -> EventPhase {
    EventPhase::try_from(phase.load(Ordering::Relaxed)).unwrap_or(EventPhase::PhaseUnspecified)
}

/// Handles one `PHASE_MARKER` payload (`PHASE:message`).
async fn switch_phase(tx: &EventTx, phase: &AtomicI32, payload: &str) {
    let (name, message) = payload.split_once(':').unwrap_or((payload, ""));
    if let Some(next) = EventPhase::from_str_name(name) {
        phase.store(next as i32, Ordering::Relaxed);
        send(tx, next, message, false).await;
    }
}

fn find_in_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|candidate| candidate.is_file()).map(|p| p.to_string_lossy().into_owned())
}

/// Runs a command to completion, streaming every stdout/stderr line as a
/// `ProgressEvent` tagged with `phase` as it's produced (not buffered until
/// exit) so the UI's log view fills in live. Also feeds every line through a
/// fresh `ActivityTracker`, so each `ProgressEvent` carries a current
/// snapshot of what nix is actually building/downloading right now.
async fn run_streamed(program: &str, args: &[&str], phase: EventPhase, tx: &EventTx) -> anyhow::Result<()> {
    run_phased(program, args, Arc::new(AtomicI32::new(phase as i32)), tx).await
}

/// Like [`run_streamed`], but the phase can change mid-run (see
/// [`PHASE_MARKER`]); `phase` holds the current one, so the caller can tell
/// afterwards which step a failure happened in.
async fn run_phased(program: &str, args: &[&str], phase: Arc<AtomicI32>, tx: &EventTx) -> anyhow::Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to start {program}: {e}"))?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    let tracker = Arc::new(Mutex::new(ActivityTracker::new()));

    let tx_out = tx.clone();
    let tracker_out = tracker.clone();
    let phase_out = phase.clone();
    let out_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(payload) = line.strip_prefix(PHASE_MARKER) {
                switch_phase(&tx_out, &phase_out, payload).await;
                continue;
            }
            forward_line(&tx_out, current_phase(&phase_out), &line, &tracker_out).await;
        }
    });

    let tx_err = tx.clone();
    let tracker_err = tracker.clone();
    let phase_err = phase.clone();
    let err_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            forward_line(&tx_err, current_phase(&phase_err), &line, &tracker_err).await;
        }
    });

    let status = child.wait().await?;
    let _ = out_task.await;
    let _ = err_task.await;

    if !status.success() {
        // Keep the message readable when an argument is a whole script.
        let mut command = format!("{program} {}", args.join(" "));
        if command.chars().count() > 160 {
            command = format!("{}…", command.chars().take(160).collect::<String>());
        }
        anyhow::bail!("{command} exited with {status}");
    }
    Ok(())
}

fn mode_label(mode: RebuildMode) -> &'static str {
    match mode {
        RebuildMode::Boot => "boot",
        RebuildMode::Test => "test",
        RebuildMode::BuildOnly => "build",
        RebuildMode::Switch | RebuildMode::Unspecified => "switch",
    }
}

fn out_link_path() -> String {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
    let dir = format!("{runtime_dir}/nix-backend-generic");
    let _ = std::fs::create_dir_all(&dir);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{dir}/result-{}-{nanos}", std::process::id())
}

pub async fn update_flake(target: &ResolvedTarget, tx: EventTx) {
    send(&tx, EventPhase::Started, format!("Updating flake inputs in {}", target.config_dir), false).await;

    let result = run_streamed(
        "nix",
        &["flake", "update", "--flake", &target.config_dir, "--log-format", "internal-json", "-v"],
        EventPhase::FlakeUpdate,
        &tx,
    )
    .await;

    match result {
        Ok(()) => send(&tx, EventPhase::Finished, "Flake inputs updated", false).await,
        Err(e) => send(&tx, EventPhase::Failed, format!("Flake update failed: {e}"), true).await,
    }
}

/// With `no_new_privs` set (inherited from whatever launched the daemon, e.g.
/// a sandboxed terminal or editor) the kernel ignores setuid bits, so the
/// pkexec wrapper can never elevate: it just prints "pkexec must be setuid
/// root". Detect that up front instead of after a long build.
fn no_new_privs() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .map(|status| status.lines().any(|line| line.strip_prefix("NoNewPrivs:").is_some_and(|v| v.trim() == "1")))
        .unwrap_or(false)
}

pub async fn rebuild(target: &ResolvedTarget, mode: RebuildMode, tx: EventTx) {
    let action = mode_label(mode);
    send(&tx, EventPhase::Started, format!("Rebuilding '{}' ({action})", target.host_name), false).await;

    if mode != RebuildMode::BuildOnly && no_new_privs() {
        send(
            &tx,
            EventPhase::Failed,
            "Cannot elevate: this backend runs with no_new_privs set (inherited from the process that started it), \
             so pkexec can't work. Start the backend from a regular session or the systemd user service - or use \
             the build-only mode, which needs no privileges.",
            true,
        )
        .await;
        return;
    }

    let out_link = out_link_path();
    let flake_attr = format!(
        "{}#nixosConfigurations.{}.config.system.build.toplevel",
        target.config_dir, target.host_name
    );

    if let Err(e) = run_streamed(
        "nix",
        &["build", &flake_attr, "--out-link", &out_link, "--log-format", "internal-json", "-v"],
        EventPhase::Build,
        &tx,
    )
    .await
    {
        send(&tx, EventPhase::Failed, format!("Build failed: {e}"), true).await;
        return;
    }

    if let Err(e) = run_streamed(
        "nix",
        &["store", "diff-closures", "/run/current-system", &out_link],
        EventPhase::Diff,
        &tx,
    )
    .await
    {
        // Not fatal - the previous generation may not exist yet, or the
        // running system may not be a nix profile (e.g. a first install).
        // Still worth telling the user, but keep going.
        send(&tx, EventPhase::Diff, format!("Could not diff against the running system: {e}"), false).await;
    }

    let resolved = match tokio::fs::canonicalize(&out_link).await {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => {
            send(&tx, EventPhase::Failed, format!("Could not resolve the built system: {e}"), true).await;
            let _ = tokio::fs::remove_file(&out_link).await;
            return;
        }
    };

    if mode == RebuildMode::BuildOnly {
        // Build only: no generation, no bootloader entry, no activation, and
        // so no privileged step at all.
        let _ = tokio::fs::remove_file(&out_link).await;
        send(&tx, EventPhase::Finished, format!("Build complete: {resolved}"), false).await;
        return;
    }

    let switch_bin = format!("{resolved}/bin/switch-to-configuration");
    let phase = Arc::new(AtomicI32::new(EventPhase::Register as i32));

    let outcome = if mode == RebuildMode::Test {
        // Nothing to register; activation alone is one authorization.
        phase.store(EventPhase::Activate as i32, Ordering::Relaxed);
        send(&tx, EventPhase::Activate, format!("Activating ({action}, authorization required)"), false).await;
        run_phased("pkexec", &[&switch_bin, action], phase.clone(), &tx).await
    } else {
        // Registering and activating share one pkexec, so a single
        // authorization covers both.
        let Some(nix_env) = find_in_path("nix-env") else {
            send(&tx, EventPhase::Failed, "Could not find nix-env in PATH", true).await;
            let _ = tokio::fs::remove_file(&out_link).await;
            return;
        };
        send(
            &tx,
            EventPhase::Register,
            format!("Registering system generation, then activating ({action}) - one authorization covers both"),
            false,
        )
        .await;
        run_phased(
            "pkexec",
            &["sh", "-c", REGISTER_AND_ACTIVATE, "sh", &nix_env, SYSTEM_PROFILE, &resolved, &switch_bin, action],
            phase.clone(),
            &tx,
        )
        .await
    };

    if let Err(e) = outcome {
        let step = if current_phase(&phase) == EventPhase::Register { "Registering system generation" } else { "Activation" };
        send(&tx, EventPhase::Failed, format!("{step} failed: {e}"), true).await;
        let _ = tokio::fs::remove_file(&out_link).await;
        return;
    }

    let _ = tokio::fs::remove_file(&out_link).await;
    send(&tx, EventPhase::Finished, format!("Rebuild ({action}) complete"), false).await;
}

async fn git_log_one(dir: &str, pathspec: Option<&str>) -> Option<(String, String, String)> {
    let mut args = vec!["-C", dir, "log", "-1", "--format=%H\u{1f}%cI\u{1f}%s"];
    if let Some(p) = pathspec {
        args.push("--");
        args.push(p);
    }

    let output = Command::new("git").args(&args).output().await.ok()?;
    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }

    let mut parts = text.splitn(3, '\u{1f}');
    let hash = parts.next()?.to_owned();
    let date = parts.next()?.to_owned();
    let subject = parts.next().unwrap_or_default().to_owned();
    Some((hash, date, subject))
}

/// Last-modified info for the dashboard's "last updated" label, from git
/// history rather than filesystem mtimes (which `nix flake update` doesn't
/// meaningfully change on its own, and which a fresh checkout resets
/// anyway). Prefers the last commit that touched `flake.lock`; falls back to
/// the repo's last commit if `flake.lock` isn't tracked.
pub async fn flake_status(target: &ResolvedTarget) -> FlakeStatusReply {
    let found = match git_log_one(&target.config_dir, Some("flake.lock")).await {
        Some(r) => Some(r),
        None => git_log_one(&target.config_dir, None).await,
    };

    let mut reply = match found {
        Some((hash, date, subject)) => FlakeStatusReply {
            has_git_history: true,
            last_modified_iso8601: date,
            last_commit_subject: subject,
            last_commit_hash: hash,
            ..Default::default()
        },
        None => FlakeStatusReply::default(),
    };
    reply.resolved_config_dir = target.config_dir.clone();
    reply.resolved_host_name = target.host_name.clone();
    reply
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command as StdCommand;

    fn fake(dir: &std::path::Path, name: &str, body: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn run_script(nix_env_body: &str) -> (bool, String) {
        let dir = std::env::temp_dir().join(format!("nix-ops-test-{}-{}", std::process::id(), nix_env_body.len()));
        std::fs::create_dir_all(&dir).unwrap();
        let nix_env = fake(&dir, "nix-env", nix_env_body);
        let switch = fake(&dir, "switch-to-configuration", r#"echo "switching: $1""#);
        let out = StdCommand::new("sh")
            .args(["-c", REGISTER_AND_ACTIVATE, "sh", &nix_env, "/profile", "/store/system", &switch, "boot"])
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned())
    }

    #[test]
    fn registers_then_marks_activation_then_activates() {
        let (ok, stdout) = run_script(r#"echo "registered $*""#);
        assert!(ok);
        let lines: Vec<_> = stdout.lines().collect();
        assert_eq!(lines[0], "registered --profile /profile --set /store/system");
        assert_eq!(lines[1], "@@nix-manager-phase:ACTIVATE:Activating (boot)");
        assert_eq!(lines[2], "switching: boot");
    }

    #[test]
    fn failed_registration_never_activates() {
        let (ok, stdout) = run_script("exit 3");
        assert!(!ok);
        assert!(!stdout.contains("ACTIVATE") && !stdout.contains("switching"));
    }

    #[tokio::test]
    async fn marker_moves_the_stream_from_register_to_activate() {
        let dir = std::env::temp_dir().join(format!("nix-ops-test-phase-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let nix_env = fake(&dir, "nix-env", r#"echo "registered""#);
        let switch = fake(&dir, "switch-to-configuration", r#"echo "switching: $1""#);

        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let phase = Arc::new(AtomicI32::new(EventPhase::Register as i32));
        let args = ["-c", REGISTER_AND_ACTIVATE, "sh", &nix_env, "/profile", "/store/system", &switch, "boot"];
        run_phased("sh", &args, phase.clone(), &tx).await.unwrap();
        drop(tx);
        let _ = std::fs::remove_dir_all(&dir);

        let mut events = Vec::new();
        while let Some(Ok(event)) = rx.recv().await {
            events.push((event.phase(), event.message));
        }
        assert_eq!(
            events,
            vec![
                (EventPhase::Register, "registered".to_owned()),
                (EventPhase::Activate, "Activating (boot)".to_owned()),
                (EventPhase::Activate, "switching: boot".to_owned()),
            ]
        );
        assert_eq!(current_phase(&phase), EventPhase::Activate);
    }

    #[test]
    fn marker_names_a_known_phase() {
        assert_eq!(EventPhase::from_str_name("ACTIVATE"), Some(EventPhase::Activate));
    }
}
