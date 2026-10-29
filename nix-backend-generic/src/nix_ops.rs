use std::process::Stdio;
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

/// Runs a command to completion, streaming every stdout/stderr line as a
/// `ProgressEvent` tagged with `phase` as it's produced (not buffered until
/// exit) so the UI's log view fills in live. Also feeds every line through a
/// fresh `ActivityTracker`, so each `ProgressEvent` carries a current
/// snapshot of what nix is actually building/downloading right now.
async fn run_streamed(program: &str, args: &[&str], phase: EventPhase, tx: &EventTx) -> anyhow::Result<()> {
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
    let out_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            forward_line(&tx_out, phase, &line, &tracker_out).await;
        }
    });

    let tx_err = tx.clone();
    let tracker_err = tracker.clone();
    let err_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            forward_line(&tx_err, phase, &line, &tracker_err).await;
        }
    });

    let status = child.wait().await?;
    let _ = out_task.await;
    let _ = err_task.await;

    if !status.success() {
        anyhow::bail!("{program} {} exited with {status}", args.join(" "));
    }
    Ok(())
}

fn mode_label(mode: RebuildMode) -> &'static str {
    match mode {
        RebuildMode::Boot => "boot",
        RebuildMode::Test => "test",
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

pub async fn rebuild(target: &ResolvedTarget, mode: RebuildMode, tx: EventTx) {
    let action = mode_label(mode);
    send(&tx, EventPhase::Started, format!("Rebuilding '{}' ({action})", target.host_name), false).await;

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

    if mode != RebuildMode::Test {
        send(&tx, EventPhase::Register, "Registering system generation (authorization required)", false).await;
        if let Err(e) = run_streamed(
            "pkexec",
            &["nix-env", "--profile", "/nix/var/nix/profiles/system", "--set", &resolved],
            EventPhase::Register,
            &tx,
        )
        .await
        {
            send(&tx, EventPhase::Failed, format!("Registering system generation failed: {e}"), true).await;
            let _ = tokio::fs::remove_file(&out_link).await;
            return;
        }
    }

    let switch_bin = format!("{resolved}/bin/switch-to-configuration");
    send(&tx, EventPhase::Activate, format!("Activating ({action}, authorization required)"), false).await;
    if let Err(e) = run_streamed("pkexec", &[&switch_bin, action], EventPhase::Activate, &tx).await {
        send(&tx, EventPhase::Failed, format!("Activation failed: {e}"), true).await;
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

    match found {
        Some((hash, date, subject)) => FlakeStatusReply {
            has_git_history: true,
            last_modified_iso8601: date,
            last_commit_subject: subject,
            last_commit_hash: hash,
        },
        None => FlakeStatusReply::default(),
    }
}
