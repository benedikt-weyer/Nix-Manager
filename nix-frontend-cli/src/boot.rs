//! `nix-manager boot`: the bootloader, its partition, and what is on it.

use std::io::{self, BufRead, IsTerminal, Write};
use std::process::ExitCode;

use chrono::{DateTime, Local, TimeZone};
use clap::Subcommand;
use nix_backend_client::{format_bytes, BootFileState, BootInfo, BootScan, Client};

use crate::style;

#[derive(Subcommand)]
pub enum BootCommand {
    /// The bootloader, how full its partition is, and the system generations [default]
    Status,
    /// List what is on the partition and which files the boot menu no longer uses (asks for authorization)
    Scan,
    /// Delete the files the boot menu no longer uses, to make room (asks for authorization)
    Clean {
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Delete old generations and rewrite the boot menu, keeping the newest ones (asks for authorization)
    Prune {
        /// How many generations to keep; the current and the booted one always stay
        #[arg(short, long, default_value_t = 5)]
        keep: usize,
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Run the bootloader installer again, which rewrites the boot menu (asks for authorization)
    Reinstall,
}

pub async fn run(client: &mut Client, command: Option<BootCommand>) -> Result<ExitCode, String> {
    let tty = io::stdout().is_terminal();
    match command.unwrap_or(BootCommand::Status) {
        BootCommand::Status => {
            let info = client.boot_info().await.map_err(message)?;
            print_info(&info, tty);
        }
        BootCommand::Scan => {
            let scan = client.scan_boot().await.map_err(message)?;
            print_scan(&scan, tty);
        }
        BootCommand::Clean { yes } => {
            let scan = client.scan_boot().await.map_err(message)?;
            let junk: Vec<_> = scan.junk().collect();
            if junk.is_empty() {
                println!("Nothing to clean: every kernel and initrd on the partition is used by the boot menu.");
                return Ok(ExitCode::SUCCESS);
            }
            for file in &junk {
                println!("  {:>10}  {}", format_bytes(file.size as f64), file.path);
            }
            let question = format!("Delete {} files ({})?", junk.len(), format_bytes(scan.junk_bytes() as f64));
            if !yes && !confirm(&question) {
                return Ok(ExitCode::FAILURE);
            }
            let paths = junk.iter().map(|file| file.path.clone()).collect();
            let reply = client.maintain_boot(paths, Vec::new(), false).await.map_err(message)?;
            print!("{}", reply.output);
            println!("{}", style("✔ Boot partition cleaned", "1;32", tty));
        }
        BootCommand::Prune { keep, yes } => {
            let info = client.boot_info().await.map_err(message)?;
            let doomed = info.prunable(keep.max(1));
            if doomed.is_empty() {
                println!("Nothing to prune: there are no more than {keep} generations besides the protected ones.");
                return Ok(ExitCode::SUCCESS);
            }
            let list: Vec<String> = doomed.iter().map(u64::to_string).collect();
            println!("Generations to delete: {}", list.join(", "));
            if !yes && !confirm(&format!("Delete {} generations and rewrite the boot menu?", doomed.len())) {
                return Ok(ExitCode::FAILURE);
            }
            let reply = client.maintain_boot(Vec::new(), doomed, true).await.map_err(message)?;
            print!("{}", reply.output);
            println!("{}", style("✔ Generations pruned, boot menu rewritten", "1;32", tty));
        }
        BootCommand::Reinstall => {
            let reply = client.maintain_boot(Vec::new(), Vec::new(), true).await.map_err(message)?;
            print!("{}", reply.output);
            println!("{}", style("✔ Bootloader reinstalled", "1;32", tty));
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn message(status: nix_backend_client::Status) -> String {
    status.message().to_owned()
}

fn confirm(question: &str) -> bool {
    if !io::stdin().is_terminal() {
        eprintln!("{question} Not a terminal: pass --yes to go ahead.");
        return false;
    }
    print!("{question} [y/N] ");
    let _ = io::stdout().flush();
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer).is_ok() && matches!(answer.trim(), "y" | "Y" | "yes")
}

fn when(unix: i64) -> String {
    Local
        .timestamp_opt(unix, 0)
        .single()
        .map(|time: DateTime<Local>| time.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

fn print_info(info: &BootInfo, tty: bool) {
    println!("{} {}", style("Bootloader", "1", tty), info.loader().label());
    if !info.installer.is_empty() {
        println!("Installer   {}", info.installer);
    }
    let kind = if info.is_mount {
        format!("{} on {}", info.fs_type, info.device)
    } else {
        "not a separate partition".to_owned()
    };
    println!("Partition   {} ({kind})", info.mount_point);
    if info.has_usage() {
        let percent = info.used_fraction() * 100.0;
        let code = if percent >= 95.0 {
            "1;31"
        } else if percent >= 80.0 {
            "1;33"
        } else {
            "1;32"
        };
        println!(
            "Usage       {} of {} used, {} free ({})",
            format_bytes(info.used_bytes as f64),
            format_bytes(info.total_bytes as f64),
            format_bytes(info.free_bytes as f64),
            style(&format!("{percent:.0}%"), code, tty)
        );
    }
    if info.generation_limit > 0 {
        println!("Menu limit  the current generation and {} older ones", info.generation_limit);
    }
    println!("\n{}", style("Generations (newest first)", "1", tty));
    for generation in &info.generations {
        let mut marks = Vec::new();
        if generation.current {
            marks.push("current");
        }
        if generation.booted {
            marks.push("booted");
        }
        println!(
            "  {:>5}  {}  {:>9}  {}{}",
            generation.number,
            when(generation.created_unix),
            format_bytes(generation.boot_bytes as f64),
            generation.label,
            if marks.is_empty() { String::new() } else { format!("  [{}]", marks.join(", ")) },
        );
    }
}

fn print_scan(scan: &BootScan, tty: bool) {
    println!("{}", style("Boot menu entries", "1", tty));
    if scan.entries.is_empty() {
        println!("  none found - the boot menu could not be read, so no file is called unused");
    }
    for entry in &scan.entries {
        let generation = if entry.generation > 0 { format!("generation {}", entry.generation) } else { String::new() };
        println!("  {}{}  {generation}  {}", entry.id, if entry.is_default { " (default)" } else { "" }, entry.title);
    }
    println!("\n{}", style("Files", "1", tty));
    for file in &scan.files {
        let (tag, code) = match file.file_state() {
            BootFileState::InUse => ("in use ", "32"),
            BootFileState::Stale => ("unused", "1;33"),
            BootFileState::Partial => ("partial", "1;31"),
            BootFileState::Other => ("       ", "2"),
        };
        println!("  {}  {:>10}  {}", style(tag, code, tty), format_bytes(file.size as f64), file.path);
    }
    let junk = scan.junk().count();
    if junk > 0 {
        println!(
            "\n{junk} files ({}) are unused and can be deleted: `nix-manager boot clean`",
            format_bytes(scan.junk_bytes() as f64)
        );
    }
}
