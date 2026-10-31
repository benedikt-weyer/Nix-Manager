use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use chrono::{DateTime, Utc};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum, ValueHint};
use clap_complete::Shell;
use nix_backend_client::{
    format_bytes, phase_label, strip_ansi, Client, EventPhase, OperationKind, ProgressEvent, RebuildMode, Streaming,
    SystemTarget,
};

#[derive(Parser)]
#[command(name = "nix-manager", version, about = "Update and rebuild a NixOS flake through the Nix-Manager backend")]
struct Cli {
    /// Backend socket [default: $NIX_BACKEND_SOCKET, then $XDG_RUNTIME_DIR/nix-backend-generic.sock]
    #[arg(short, long, global = true, value_hint = ValueHint::FilePath)]
    socket: Option<PathBuf>,

    /// Flake directory to operate on [default: the backend's own, usually /etc/nixos]
    #[arg(short = 'c', long, global = true, env = "NIX_BACKEND_CONFIG_DIR", value_hint = ValueHint::DirPath)]
    config_dir: Option<String>,

    /// nixosConfiguration to build [default: the backend's own, usually this machine's hostname]
    #[arg(short = 'H', long, global = true, env = "NIX_BACKEND_HOST_NAME", value_hint = ValueHint::Hostname)]
    host: Option<String>,

    /// Only show phase changes and the result, not nix's output
    #[arg(short, long, global = true)]
    quiet: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show when the flake was last changed
    Status,
    /// Run `nix flake update`
    Update,
    /// Build the system configuration and activate it
    Rebuild {
        /// How to activate: switch now, on next boot, test (not persistent), or just build
        #[arg(value_enum, default_value = "switch")]
        mode: Mode,
        /// Extra arguments for `nix build`, after `--` [default: the nix options saved in the GUI's settings]
        #[arg(last = true)]
        nix_args: Vec<String>,
        /// Build as yourself instead of as root; the nix daemon then ignores download settings like download-attempts
        #[arg(long)]
        no_root: bool,
    },
    /// Follow the running (or next) operation, e.g. one started from a GUI; exits when it ends
    Watch,
    /// Print a shell completion script, e.g. `source <(nix-manager completions zsh)`
    Completions { shell: Shell },
}

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    /// Build, register as the current generation and activate now
    Switch,
    /// Build and register; activate on next boot
    Boot,
    /// Build and activate now, without registering a generation
    Test,
    /// Build only: no generation, no bootloader entry, no activation
    Build,
}

impl From<Mode> for RebuildMode {
    fn from(mode: Mode) -> Self {
        match mode {
            Mode::Switch => RebuildMode::Switch,
            Mode::Boot => RebuildMode::Boot,
            Mode::Test => RebuildMode::Test,
            Mode::Build => RebuildMode::BuildOnly,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Command::Completions { shell } = cli.command {
        clap_complete::generate(shell, &mut Cli::command(), "nix-manager", &mut io::stdout());
        return ExitCode::SUCCESS;
    }

    match run(cli).await {
        Ok(code) => code,
        Err(message) => {
            eprintln!("{} {message}", style("error:", "1;31", io::stderr().is_terminal()));
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode, String> {
    let socket = cli.socket.unwrap_or_else(nix_backend_client::socket_path);
    let mut client = Client::connect(&socket).await.map_err(|e| e.to_string())?;
    let target = SystemTarget {
        config_dir: cli.config_dir.unwrap_or_default().trim().to_owned(),
        host_name: cli.host.unwrap_or_default().trim().to_owned(),
    };
    let mut printer = Printer::new(cli.quiet);

    match cli.command {
        Command::Status => {
            let status = client.flake_status(target).await.map_err(|s| s.message().to_owned())?;
            if !status.has_git_history {
                println!("No git history in the config directory");
                return Ok(ExitCode::SUCCESS);
            }
            let when = DateTime::parse_from_rfc3339(&status.last_modified_iso8601).ok();
            let ago = when.map(|t| format!(" ({})", relative(t.with_timezone(&Utc)))).unwrap_or_default();
            println!("Last changed  {}{ago}", status.last_modified_iso8601);
            println!(
                "Commit        {} {}",
                status.last_commit_hash.chars().take(7).collect::<String>(),
                status.last_commit_subject
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Update => {
            let stream = client.update_flake(target).await.map_err(|s| s.message().to_owned())?;
            follow(stream, &mut printer).await
        }
        Command::Rebuild { mode, nix_args, no_root } => {
            let settings = nix_backend_client::settings::Settings::load();
            let nix_args = if nix_args.is_empty() { nix_backend_client::settings::nix_args(&settings.nix_options) } else { nix_args };
            let build_as_root = settings.build_as_root() && !no_root;
            let stream = client.rebuild(target, mode.into(), nix_args, build_as_root).await.map_err(|s| s.message().to_owned())?;
            follow(stream, &mut printer).await
        }
        Command::Watch => {
            let mut stream = client.watch().await.map_err(|s| s.message().to_owned())?;
            eprintln!("Waiting for an operation... (Ctrl-C to stop watching)");
            let mut announced = false;
            loop {
                let next = tokio::select! {
                    next = stream.message() => next.map_err(|s| s.message().to_owned())?,
                    _ = tokio::signal::ctrl_c() => return Ok(ExitCode::SUCCESS),
                };
                let Some(op) = next else { return Err("the backend closed the connection".to_owned()) };
                let Some(event) = op.event.as_ref() else { continue };
                if !announced {
                    announced = true;
                    let what = match op.kind() {
                        OperationKind::UpdateFlake => "Updating flake inputs".to_owned(),
                        _ => format!("Rebuilding ({:?})", op.mode()).to_lowercase(),
                    };
                    println!("{}", style(&format!("==> {what}"), "1;34", printer.tty));
                }
                if let Some(code) = printer.event(event) {
                    return Ok(code);
                }
            }
        }
        Command::Completions { .. } => unreachable!("handled before connecting"),
    }
}

/// Prints events until the operation ends. Ctrl-C only stops following: the
/// operation itself keeps running in the backend.
async fn follow(mut stream: Streaming<ProgressEvent>, printer: &mut Printer) -> Result<ExitCode, String> {
    loop {
        let next = tokio::select! {
            next = stream.message() => next.map_err(|s| s.message().to_owned())?,
            _ = tokio::signal::ctrl_c() => {
                printer.clear_status();
                eprintln!("\nStopped following. The operation keeps running in the backend; `nix-manager watch` re-attaches.");
                return Ok(ExitCode::from(130));
            }
        };
        let Some(event) = next else {
            return Err("the backend closed the stream before the operation finished".to_owned());
        };
        if let Some(code) = printer.event(&event) {
            return Ok(code);
        }
    }
}

struct Printer {
    tty: bool,
    quiet: bool,
    status_shown: bool,
    phase: Option<EventPhase>,
}

impl Printer {
    fn new(quiet: bool) -> Self {
        Self { tty: io::stdout().is_terminal(), quiet, status_shown: false, phase: None }
    }

    /// Renders one event; returns the exit code once the operation is over.
    fn event(&mut self, event: &ProgressEvent) -> Option<ExitCode> {
        self.clear_status();
        let phase = event.phase();
        let message = strip_ansi(&event.message);

        if !matches!(phase, EventPhase::Started | EventPhase::Finished | EventPhase::Failed)
            && self.phase != Some(phase)
        {
            println!("{}", style(&format!("==> {}", phase_label(phase)), "1;34", self.tty));
        }
        self.phase = Some(phase);

        match phase {
            EventPhase::Started => println!("{}", style(&message, "1", self.tty)),
            EventPhase::Finished => {
                println!("{}", style(&format!("✔ {message}"), "1;32", self.tty));
                return Some(ExitCode::SUCCESS);
            }
            EventPhase::Failed => {
                eprintln!("{}", style(&format!("✘ {message}"), "1;31", io::stderr().is_terminal()));
                return Some(ExitCode::FAILURE);
            }
            _ if message.is_empty() => {}
            _ if event.is_error => eprintln!("  {}", style(&message, "31", io::stderr().is_terminal())),
            _ if !self.quiet => println!("  {message}"),
            _ => {}
        }

        self.show_status(event);
        None
    }

    /// A single self-redrawing line on stderr, only when it's a terminal.
    fn show_status(&mut self, event: &ProgressEvent) {
        let Some(stats) = event.stats.as_ref().filter(|_| io::stderr().is_terminal()) else { return };

        let mut parts = Vec::new();
        if stats.builds_expected > 0 {
            parts.push(format!("builds {}/{}", stats.builds_done, stats.builds_expected));
        }
        if stats.copies_expected > 0 {
            parts.push(format!(
                "downloads {}/{} ({} / {}{})",
                stats.copies_done,
                stats.copies_expected,
                format_bytes(stats.download_bytes_done as f64),
                format_bytes(stats.download_bytes_expected as f64),
                if stats.download_bytes_per_sec > 0.0 {
                    format!(", {}/s", format_bytes(stats.download_bytes_per_sec))
                } else {
                    String::new()
                }
            ));
        }
        if event.fraction_done >= 0.0 {
            parts.push(format!("{:.0}%", event.fraction_done * 100.0));
        }
        if parts.is_empty() {
            return;
        }
        eprint!("\r\x1b[2K\x1b[2m{}\x1b[0m", parts.join("  ·  "));
        let _ = io::stderr().flush();
        self.status_shown = true;
    }

    fn clear_status(&mut self) {
        if self.status_shown {
            eprint!("\r\x1b[2K");
            self.status_shown = false;
        }
    }
}

fn style(text: &str, code: &str, enabled: bool) -> String {
    if enabled {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

fn relative(then: DateTime<Utc>) -> String {
    let seconds = (Utc::now() - then).num_seconds().max(0);
    let (value, unit) = match seconds {
        0..=59 => return "just now".to_owned(),
        60..=3599 => (seconds / 60, "minute"),
        3600..=86_399 => (seconds / 3600, "hour"),
        86_400..=2_591_999 => (seconds / 86_400, "day"),
        2_592_000..=31_535_999 => (seconds / 2_592_000, "month"),
        _ => (seconds / 31_536_000, "year"),
    };
    format!("{value} {unit}{} ago", if value == 1 { "" } else { "s" })
}
