//! The bootloader and the boot partition: which loader the running system
//! installs, how full its partition is, which system generations exist, which
//! of them have a menu entry, and which files on the partition nothing refers
//! to any more.
//!
//! Three loaders are understood: r-boot, systemd-boot and GRUB. What they have
//! in common is that every rebuild runs an installer script that copies kernels
//! and initrds onto the partition and rewrites the menu from the generations in
//! `/nix/var/nix/profiles`. So removing a generation and running the installer
//! again is the one way to prune the menu for all three, and it is what
//! [`maintain`] does.
//!
//! Reading the partition and changing it need root (it is mounted `0077`), and
//! go through a single `pkexec` each, like activating a rebuild does. Everything
//! else here works as the user.

// tonic::Status is large and every handler returns it.
#![allow(clippy::result_large_err)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;
use tonic::Status;

use crate::nix_ops::{find_in_path, no_new_privs};
use crate::proto::{
    BootEntry, BootFile, BootFileState, BootGeneration, BootInfo, BootScan, Bootloader, MaintainBootReply,
    MaintainBootRequest,
};

pub(crate) const PROFILES_DIR: &str = "/nix/var/nix/profiles";

/// Where an ESP is mounted when the installer doesn't say.
const MOUNT_CANDIDATES: [&str; 3] = ["/boot", "/boot/efi", "/efi"];

/// Marks the lines of [`SCAN_SCRIPT`]'s output that aren't file listing or file
/// content.
const MARK: &str = "@@nix-manager-boot:";

/// Lists the partition and prints the files the loaders keep their menu in.
/// $1 is the mount point. `find` prints `path<TAB>size<TAB>mtime`.
const SCAN_SCRIPT: &str = r#"cd "$1" || exit 1
echo "@@nix-manager-boot:files"
find . -xdev -type f -printf '%P\t%s\t%T@\n'
for f in boot/r-boot.toml loader/loader.conf loader/entries/*.conf grub/grub.cfg; do
  if [ -f "$f" ]; then
    echo "@@nix-manager-boot:file:$f"
    cat "$f"
    echo
    echo "@@nix-manager-boot:end"
  fi
done
"#;

/// Deletes files, then generations, then reinstalls the bootloader, stopping at
/// the first failure. pkexec wipes the environment, so $1 is the PATH to use.
/// $2 mount point, $3 nix-env, $4 system profile, $5 switch-to-configuration,
/// $6 reinstall (0|1), $7 how many files follow; then those files (relative to
/// the mount point), then the generation numbers.
const MAINTAIN_SCRIPT: &str = r#"set -e
PATH="$1"; export PATH
mount="$2"; nix_env="$3"; profile="$4"; switch="$5"; reinstall="$6"; nfiles="$7"
shift 7
cd "$mount"
i=0
while [ "$i" -lt "$nfiles" ]; do
  f="$1"; shift; i=$((i + 1))
  if [ -f "$f" ] && [ ! -L "$f" ]; then
    echo "Removing $f"
    rm -f -- "$f"
  fi
done
if [ "$#" -gt 0 ]; then
  echo "Deleting system generations: $*"
  "$nix_env" --profile "$profile" --delete-generations "$@"
fi
if [ "$reinstall" = 1 ]; then
  echo "Reinstalling the bootloader"
  "$switch" boot
fi
"#;

// ------------------------------------------------------------------- info --

/// What the installer script says about itself.
pub(crate) struct Installer {
    pub(crate) path: String,
    pub(crate) text: String,
}

/// The line of `switch-to-configuration` that names the installer:
/// `export INSTALL_BOOTLOADER='/nix/store/…-install-r-boot.sh'`.
fn installer_path(wrapper: &str) -> Option<String> {
    wrapper.lines().find_map(|line| {
        let value = line.trim().strip_prefix("export INSTALL_BOOTLOADER=")?;
        let value = value.trim().trim_matches(|c| c == '\'' || c == '"');
        (!value.is_empty()).then(|| value.to_owned())
    })
}

fn read_installer(system: &Path) -> Option<Installer> {
    let wrapper = std::fs::read(system.join("bin/switch-to-configuration")).ok()?;
    let path = installer_path(&String::from_utf8_lossy(&wrapper))?;
    let text = std::fs::read(&path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned()).unwrap_or_default();
    Some(Installer { path, text })
}

fn loader_of(installer_path: &str) -> Bootloader {
    let name = installer_path.rsplit('/').next().unwrap_or(installer_path);
    if name.contains("r-boot") {
        Bootloader::RBoot
    } else if name.contains("systemd-boot") {
        Bootloader::SystemdBoot
    } else if name.contains("grub") {
        Bootloader::Grub
    } else {
        Bootloader::Unknown
    }
}

/// The arguments r-boot's installer passes to `r-boot-conf-builder`.
fn rboot_args(installer: &str) -> Vec<String> {
    installer
        .lines()
        .find(|line| line.contains("r-boot-conf-builder") && !line.trim_start().starts_with('#'))
        .map(|line| {
            line.split_whitespace().map(|token| token.trim_matches(|c| c == '\'' || c == '"').to_owned()).collect()
        })
        .unwrap_or_default()
}

fn arg_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let at = args.iter().position(|arg| arg == flag)?;
    args.get(at + 1).map(String::as_str)
}

/// Where the loader keeps kernels and initrds, relative to the partition.
fn kernel_dir(loader: Bootloader) -> &'static str {
    match loader {
        Bootloader::RBoot => "boot/nixos",
        Bootloader::SystemdBoot => "EFI/nixos",
        Bootloader::Grub => "kernels",
        Bootloader::Unknown => "",
    }
}

struct Mount {
    point: String,
    fs_type: String,
    source: String,
}

/// `\040` and friends, which is how mountinfo writes spaces.
fn unescape_mount(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes.get(i + 1..i + 4).and_then(|digits| std::str::from_utf8(digits).ok());
        match (bytes[i], octal.and_then(|d| u8::from_str_radix(d, 8).ok())) {
            (b'\\', Some(byte)) => {
                out.push(byte);
                i += 4;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_mountinfo(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let dash = fields.iter().position(|field| *field == "-")?;
            Some(Mount {
                point: unescape_mount(fields.get(4)?),
                fs_type: (*fields.get(dash + 1)?).to_owned(),
                source: (*fields.get(dash + 2)?).to_owned(),
            })
        })
        .collect()
}

/// `(total, used, free)` in bytes of the file system `path` is on.
fn usage(path: &str) -> Option<(u64, u64, u64)> {
    let output = std::process::Command::new("df").args(["-B1", "-P", path]).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let fields: Vec<&str> = text.lines().last()?.split_whitespace().collect();
    Some((fields.get(1)?.parse().ok()?, fields.get(2)?.parse().ok()?, fields.get(3)?.parse().ok()?))
}

/// What the loader left on a partition we are allowed to read.
fn probe(mount: &str) -> Bootloader {
    let root = Path::new(mount);
    if root.join("boot/r-boot.toml").exists() {
        Bootloader::RBoot
    } else if root.join("loader/entries").is_dir() || root.join("EFI/systemd").is_dir() {
        Bootloader::SystemdBoot
    } else if root.join("grub").is_dir() {
        Bootloader::Grub
    } else {
        Bootloader::Unknown
    }
}

pub(crate) fn canonical(path: impl AsRef<Path>) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

fn unix_seconds(time: std::io::Result<std::time::SystemTime>) -> i64 {
    time.ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// The text the boot menu shows for a generation.
fn label_of(system: &Path) -> String {
    let from_spec = std::fs::read(system.join("boot.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|json| json["org.nixos.bootspec.v1"]["label"].as_str().map(str::to_owned));
    from_spec
        .or_else(|| std::fs::read_to_string(system.join("nixos-version")).ok().map(|v| v.trim().to_owned()))
        .unwrap_or_default()
}

fn generation_number(name: &str) -> Option<u64> {
    name.strip_prefix("system-")?.strip_suffix("-link")?.parse().ok()
}

/// Every generation of the system profile, newest first.
fn generations(profiles: &Path) -> Vec<BootGeneration> {
    let current = canonical(profiles.join("system"));
    let booted = canonical("/run/booted-system");
    let running = canonical("/run/current-system");

    let mut list: Vec<BootGeneration> = std::fs::read_dir(profiles)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let number = generation_number(entry.file_name().to_str()?)?;
            let link = entry.path();
            let store = canonical(&link)?;
            let kernel = canonical(store.join("kernel"));
            let initrd = canonical(store.join("initrd"));
            let size = |path: &Option<PathBuf>| {
                path.as_ref().and_then(|path| std::fs::metadata(path).ok()).map_or(0, |meta| meta.len())
            };
            let is = |other: &Option<PathBuf>| other.as_ref() == Some(&store);
            Some(BootGeneration {
                number,
                label: label_of(&store),
                created_unix: unix_seconds(std::fs::symlink_metadata(&link).and_then(|meta| meta.modified())),
                current: is(&current),
                booted: is(&booted),
                running: is(&running),
                boot_bytes: size(&kernel) + size(&initrd),
                kernel: kernel.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
                initrd: initrd.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
                store_path: store.to_string_lossy().into_owned(),
            })
        })
        .collect();
    list.sort_by_key(|generation| std::cmp::Reverse(generation.number));
    list
}

pub fn info() -> BootInfo {
    let profiles = Path::new(PROFILES_DIR);
    let system = profiles.join("system");
    let installer = read_installer(&system);

    let mut loader = installer.as_ref().map_or(Bootloader::Unknown, |installer| loader_of(&installer.path));
    let args = match (&installer, loader) {
        (Some(installer), Bootloader::RBoot) => rboot_args(&installer.text),
        _ => Vec::new(),
    };
    let generation_limit = arg_after(&args, "-g").and_then(|n| n.parse().ok()).unwrap_or(0);

    let mounts = parse_mountinfo(&std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default());
    let mount_point = arg_after(&args, "-d")
        .map(str::to_owned)
        .or_else(|| {
            MOUNT_CANDIDATES
                .iter()
                .find(|candidate| mounts.iter().any(|m| m.point == **candidate))
                .map(|c| (*c).to_owned())
        })
        .unwrap_or_else(|| MOUNT_CANDIDATES[0].to_owned());

    let mount = mounts.iter().rev().find(|mount| mount.point == mount_point);
    let listable = std::fs::read_dir(&mount_point).is_ok();
    if loader == Bootloader::Unknown && listable {
        loader = probe(&mount_point);
    }
    let (total_bytes, used_bytes, free_bytes) = usage(&mount_point).unwrap_or_default();

    BootInfo {
        bootloader: loader as i32,
        signing: Some(crate::signing::info(loader, installer.as_ref())),
        installer: installer.map(|installer| installer.path).unwrap_or_default(),
        is_mount: mount.is_some(),
        device: mount.map(|mount| mount.source.clone()).unwrap_or_default(),
        fs_type: mount.map(|mount| mount.fs_type.clone()).unwrap_or_default(),
        mount_point,
        total_bytes,
        used_bytes,
        free_bytes,
        listable,
        generation_limit,
        kernel_dir: kernel_dir(loader).to_owned(),
        generations: generations(profiles),
    }
}

// ------------------------------------------------------------------- scan --

#[derive(Debug, PartialEq)]
struct RawFile {
    path: String,
    size: u64,
    modified: i64,
}

#[derive(Debug, Default, PartialEq)]
struct RawScan {
    files: Vec<RawFile>,
    /// `(path relative to the partition, content)` of the menu files.
    configs: Vec<(String, String)>,
}

fn parse_scan_output(text: &str) -> RawScan {
    enum Section {
        None,
        Files,
        Config(String, String),
    }

    let mut scan = RawScan::default();
    let mut section = Section::None;
    for line in text.lines() {
        if let Some(marker) = line.strip_prefix(MARK) {
            section = match (marker, section) {
                ("files", _) => Section::Files,
                ("end", Section::Config(name, content)) => {
                    scan.configs.push((name, content));
                    Section::None
                }
                (marker, _) => match marker.strip_prefix("file:") {
                    Some(name) => Section::Config(name.to_owned(), String::new()),
                    None => Section::None,
                },
            };
            continue;
        }
        match &mut section {
            Section::Files => {
                let mut fields = line.split('\t');
                if let (Some(path), Some(size), Some(modified)) = (fields.next(), fields.next(), fields.next()) {
                    if let (Ok(size), Ok(modified)) = (size.parse(), modified.parse::<f64>()) {
                        scan.files.push(RawFile { path: path.to_owned(), size, modified: modified as i64 });
                    }
                }
            }
            Section::Config(_, content) => {
                content.push_str(line);
                content.push('\n');
            }
            Section::None => {}
        }
    }
    scan
}

/// `nixos-generation-12`, `nixos-generation-12-specialisation-x` -> 12.
fn generation_from_id(id: &str) -> Option<u64> {
    let rest = &id[id.find("generation-")? + "generation-".len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn normalized(path: &str) -> String {
    let path = match path.strip_prefix('(') {
        // GRUB's `($drive1)/kernels/…`.
        Some(rest) => rest.split_once(')').map_or(rest, |(_, path)| path),
        None => path,
    };
    path.trim_start_matches('/').to_owned()
}

fn toml_pair(line: &str) -> Option<(&str, String)> {
    let (key, value) = line.split_once('=')?;
    let value = value.trim();
    let value = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(value);
    Some((key.trim(), value.replace("\\\"", "\"")))
}

/// r-boot's `r-boot.toml`: `default = "…"` and a `[[entries]]` table per entry.
fn parse_rboot(text: &str, current: Option<u64>) -> Vec<BootEntry> {
    let mut default = String::new();
    let mut entries: Vec<BootEntry> = Vec::new();
    for line in text.lines().map(str::trim) {
        if line == "[[entries]]" {
            entries.push(BootEntry::default());
            continue;
        }
        let Some((key, value)) = toml_pair(line) else { continue };
        match (entries.last_mut(), key) {
            (None, "default") => default = value,
            (Some(entry), "id") => entry.id = value,
            (Some(entry), "title") => entry.title = value,
            (Some(entry), "linux" | "initrd") => entry.files.push(normalized(&value)),
            _ => {}
        }
    }
    for entry in &mut entries {
        entry.is_default = entry.id == default;
        // The default entry follows the system profile instead of naming a generation.
        entry.generation =
            if entry.id == "nixos-default" { current.unwrap_or(0) } else { generation_from_id(&entry.id).unwrap_or(0) };
    }
    entries
}

/// One `loader/entries/*.conf`: `key value` per line.
fn parse_systemd_entry(id: &str, text: &str) -> BootEntry {
    let mut entry =
        BootEntry { id: id.to_owned(), generation: generation_from_id(id).unwrap_or(0), ..Default::default() };
    for line in text.lines() {
        let Some((key, value)) = line.trim().split_once(char::is_whitespace) else { continue };
        let value = value.trim();
        match key {
            "title" => entry.title = value.to_owned(),
            "linux" | "initrd" => entry.files.push(normalized(value)),
            _ => {}
        }
    }
    entry
}

/// `grub.cfg`: every `menuentry` and the `linux`/`initrd` lines after it.
fn parse_grub(text: &str) -> Vec<BootEntry> {
    let mut entries: Vec<BootEntry> = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("menuentry ") {
            let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'');
            let title = quote
                .and_then(|quote| rest[1..].split(quote).next())
                .unwrap_or_else(|| rest.split_whitespace().next().unwrap_or(""));
            let generation = title
                .split_once("Configuration ")
                .and_then(|(_, tail)| tail.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok())
                .unwrap_or(0);
            entries.push(BootEntry {
                id: format!("grub-{}", entries.len() + 1),
                title: title.to_owned(),
                generation,
                ..Default::default()
            });
            continue;
        }
        let mut words = line.split_whitespace();
        if let (Some("linux" | "linuxefi" | "initrd" | "initrdefi"), Some(path), Some(entry)) =
            (words.next(), words.next(), entries.last_mut())
        {
            entry.files.push(normalized(path));
        }
    }
    // The first entry is the one GRUB boots unless told otherwise.
    if let Some(first) = entries.first_mut() {
        first.is_default = true;
    }
    entries
}

fn parse_entries(loader: Bootloader, configs: &[(String, String)], current: Option<u64>) -> Vec<BootEntry> {
    let config = |name: &str| configs.iter().find(|(path, _)| path == name).map(|(_, text)| text.as_str());
    match loader {
        Bootloader::RBoot => config("boot/r-boot.toml").map(|text| parse_rboot(text, current)).unwrap_or_default(),
        Bootloader::Grub => config("grub/grub.cfg").map(parse_grub).unwrap_or_default(),
        Bootloader::SystemdBoot => {
            let default = config("loader/loader.conf")
                .and_then(|text| {
                    text.lines().find_map(|line| line.trim().strip_prefix("default ").map(|v| v.trim().to_owned()))
                })
                .unwrap_or_default();
            let mut entries: Vec<BootEntry> = configs
                .iter()
                .filter_map(|(path, text)| {
                    let id = path.strip_prefix("loader/entries/")?.strip_suffix(".conf")?;
                    let mut entry = parse_systemd_entry(id, text);
                    entry.is_default = default == format!("{id}.conf") || default == id;
                    Some(entry)
                })
                .collect();
            entries.sort_by_key(|entry| std::cmp::Reverse(entry.generation));
            entries
        }
        Bootloader::Unknown => Vec::new(),
    }
}

/// Splits the partition's files into the ones menu entries use and the ones
/// nothing does. Without a single entry to go by nothing is called stale: that
/// is what an unreadable or unrecognised menu looks like, and deleting on that
/// evidence would delete everything.
fn classify(raw: Vec<RawFile>, entries: &[BootEntry], kernel_dir: &str) -> Vec<BootFile> {
    let mut users: HashMap<&str, Vec<String>> = HashMap::new();
    for entry in entries {
        for file in &entry.files {
            users.entry(file.as_str()).or_default().push(entry.id.clone());
        }
    }

    let mut files: Vec<BootFile> = raw
        .into_iter()
        .map(|file| {
            let used_by = users.get(file.path.as_str()).cloned().unwrap_or_default();
            let direct_child = !kernel_dir.is_empty()
                && file
                    .path
                    .strip_prefix(kernel_dir)
                    .and_then(|rest| rest.strip_prefix('/'))
                    .is_some_and(|name| !name.contains('/'));
            let state = if !direct_child {
                BootFileState::Other
            } else if file.path.contains(".tmp.") {
                BootFileState::Partial
            } else if !used_by.is_empty() {
                BootFileState::InUse
            } else if entries.is_empty() {
                BootFileState::Other
            } else {
                BootFileState::Stale
            };
            BootFile { path: file.path, size: file.size, modified_unix: file.modified, state: state as i32, used_by }
        })
        .collect();
    files.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
    files
}

/// Runs a script as `sh -c`, through `pkexec` when `elevate`. Returns whether
/// it succeeded and everything it printed.
pub(crate) async fn run_script(elevate: bool, script: &str, args: &[String]) -> Result<(bool, String), Status> {
    let mut command = if elevate {
        let mut command = Command::new("pkexec");
        command.args(["sh", "-c", script, "sh"]);
        command
    } else {
        let mut command = Command::new("sh");
        command.args(["-c", script, "sh"]);
        command
    };
    let output = command
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| Status::internal(format!("could not run {}: {e}", if elevate { "pkexec" } else { "sh" })))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok((output.status.success(), text))
}

pub(crate) fn require_elevation() -> Result<(), Status> {
    if no_new_privs() {
        return Err(Status::failed_precondition(
            "Cannot elevate: this backend runs with no_new_privs set (inherited from the process that started it), so \
             pkexec can't work. Start the backend from a regular session or the systemd user service.",
        ));
    }
    Ok(())
}

pub async fn scan() -> Result<BootScan, Status> {
    let info = tokio::task::spawn_blocking(info).await.map_err(|e| Status::internal(format!("worker failed: {e}")))?;
    let elevate = !info.listable;
    if elevate {
        require_elevation()?;
    }

    let (ok, text) = run_script(elevate, SCAN_SCRIPT, std::slice::from_ref(&info.mount_point)).await?;
    if !ok {
        return Err(Status::failed_precondition(format!("Could not read {}: {}", info.mount_point, text.trim())));
    }
    let raw = parse_scan_output(&text);

    // A partition that was only recognised once it could be read.
    let loader = match Bootloader::try_from(info.bootloader).unwrap_or(Bootloader::Unknown) {
        Bootloader::Unknown => {
            let names: HashSet<&str> = raw.configs.iter().map(|(path, _)| path.as_str()).collect();
            if names.contains("boot/r-boot.toml") {
                Bootloader::RBoot
            } else if names.iter().any(|name| name.starts_with("loader/")) {
                Bootloader::SystemdBoot
            } else if names.contains("grub/grub.cfg") {
                Bootloader::Grub
            } else {
                Bootloader::Unknown
            }
        }
        known => known,
    };

    let current = info.generations.iter().find(|generation| generation.current).map(|generation| generation.number);
    let entries = parse_entries(loader, &raw.configs, current);
    let dir = if info.kernel_dir.is_empty() { kernel_dir(loader) } else { info.kernel_dir.as_str() };
    Ok(BootScan { elevated: elevate, files: classify(raw.files, &entries, dir), entries })
}

// --------------------------------------------------------------- maintain --

/// A file the frontend asks to delete: a kernel or initrd directly inside the
/// loader's own directory, nothing else.
fn check_deletable(kernel_dir: &str, path: &str) -> Result<(), Status> {
    let name = path.strip_prefix(kernel_dir).and_then(|rest| rest.strip_prefix('/'));
    match name {
        Some(name)
            if !kernel_dir.is_empty()
                && !name.is_empty()
                && !name.contains(['/', '\0', '\n'])
                && name != "."
                && name != ".." =>
        {
            Ok(())
        }
        _ => Err(Status::invalid_argument(format!("'{path}' is not a kernel or initrd the bootloader keeps"))),
    }
}

pub async fn maintain(request: MaintainBootRequest) -> Result<MaintainBootReply, Status> {
    let info = tokio::task::spawn_blocking(info).await.map_err(|e| Status::internal(format!("worker failed: {e}")))?;

    for path in &request.delete_files {
        check_deletable(&info.kernel_dir, path)?;
    }
    for number in &request.delete_generations {
        let generation = info
            .generations
            .iter()
            .find(|generation| generation.number == *number)
            .ok_or_else(|| Status::not_found(format!("there is no generation {number}")))?;
        if generation.current || generation.booted {
            let which = if generation.current { "current" } else { "booted" };
            return Err(Status::failed_precondition(format!("generation {number} is the {which} system and stays")));
        }
    }
    let reinstall = request.reinstall || !request.delete_generations.is_empty();
    if request.delete_files.is_empty() && request.delete_generations.is_empty() && !reinstall {
        return Err(Status::invalid_argument("nothing to do"));
    }
    require_elevation()?;

    let system = canonical(Path::new(PROFILES_DIR).join("system"))
        .ok_or_else(|| Status::failed_precondition("the system profile does not exist"))?;
    let switch = system.join("bin/switch-to-configuration");
    let nix_env =
        find_in_path("nix-env").ok_or_else(|| Status::failed_precondition("Could not find nix-env in PATH"))?;

    let mut args = vec![
        std::env::var("PATH").unwrap_or_default(),
        info.mount_point.clone(),
        nix_env,
        format!("{PROFILES_DIR}/system"),
        switch.to_string_lossy().into_owned(),
        if reinstall { "1" } else { "0" }.to_owned(),
        request.delete_files.len().to_string(),
    ];
    args.extend(request.delete_files.iter().map(|path| path.to_owned()));
    args.extend(request.delete_generations.iter().map(u64::to_string));

    let (ok, output) = run_script(true, MAINTAIN_SCRIPT, &args).await?;
    if ok {
        Ok(MaintainBootReply { output })
    } else {
        Err(Status::failed_precondition(output.trim().to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_installer_in_the_wrapper() {
        let wrapper = "#!/bin/bash\nexport TOPLEVEL='/x'\nexport INSTALL_BOOTLOADER='/nix/store/abc-install-r-boot.sh'\nexport Y=1\n";
        assert_eq!(installer_path(wrapper).as_deref(), Some("/nix/store/abc-install-r-boot.sh"));
        assert_eq!(installer_path("nothing here"), None);
    }

    #[test]
    fn names_the_loader_from_the_installer() {
        assert_eq!(loader_of("/nix/store/x-install-r-boot.sh"), Bootloader::RBoot);
        assert_eq!(loader_of("/nix/store/x-install-systemd-boot.sh"), Bootloader::SystemdBoot);
        assert_eq!(loader_of("/nix/store/x-install-grub.sh"), Bootloader::Grub);
        assert_eq!(loader_of("/nix/store/x-something.sh"), Bootloader::Unknown);
        // The store hash is not part of the name.
        assert_eq!(loader_of("/nix/store/grub0000-install-r-boot.sh"), Bootloader::RBoot);
    }

    #[test]
    fn reads_rboots_arguments() {
        let text = "set -e\n# r-boot-conf-builder is run last\n/nix/store/h-r-boot-0.1.0/bin/r-boot-conf-builder -t 5 -d '/boot' -b \"$BOOT_EFI\" -g 6 -e -c \"$@\"\n";
        let args = rboot_args(text);
        assert_eq!(arg_after(&args, "-d"), Some("/boot"));
        assert_eq!(arg_after(&args, "-g"), Some("6"));
        assert_eq!(arg_after(&args, "-x"), None);
    }

    #[test]
    fn reads_mountinfo() {
        let text = "53 72 259:1 / /boot rw,relatime shared:129 - vfat /dev/nvme1n1p1 rw,fmask=0077\n\
                    54 72 259:2 / /mnt/with\\040space rw - ext4 /dev/sda1 rw\n";
        let mounts = parse_mountinfo(text);
        assert_eq!(mounts.len(), 2);
        assert_eq!(
            (mounts[0].point.as_str(), mounts[0].fs_type.as_str(), mounts[0].source.as_str()),
            ("/boot", "vfat", "/dev/nvme1n1p1")
        );
        assert_eq!(mounts[1].point, "/mnt/with space");
    }

    #[test]
    fn numbers_generations() {
        assert_eq!(generation_number("system-1187-link"), Some(1187));
        assert_eq!(generation_number("system"), None);
        assert_eq!(generation_number("system-x-link"), None);
        assert_eq!(generation_from_id("nixos-generation-12"), Some(12));
        assert_eq!(generation_from_id("nixos-generation-12-specialisation-gaming"), Some(12));
        assert_eq!(generation_from_id("nixos-default"), None);
    }

    #[test]
    fn parses_the_scan_output() {
        let text = "@@nix-manager-boot:files\n\
                    boot/nixos/a-linux-bzImage\t13631488\t1790000000.5\n\
                    EFI/BOOT/BOOTX64.EFI\t100\t1700000000.0\n\
                    not a record\n\
                    @@nix-manager-boot:file:boot/r-boot.toml\n\
                    default = \"nixos-default\"\n\
                    \n\
                    @@nix-manager-boot:end\n";
        let scan = parse_scan_output(text);
        assert_eq!(scan.files.len(), 2);
        assert_eq!(
            scan.files[0],
            RawFile { path: "boot/nixos/a-linux-bzImage".into(), size: 13631488, modified: 1790000000 }
        );
        assert_eq!(scan.configs.len(), 1);
        assert_eq!(scan.configs[0].0, "boot/r-boot.toml");
        assert!(scan.configs[0].1.starts_with("default = "));
    }

    const RBOOT_TOML: &str = "# Generated file\ndefault = \"nixos-default\"\ntimeout = 5\n\n\
        [[entries]]\nid = \"nixos-default\"\ntitle = \"NixOS (26.11, nixos-default)\"\nkind = \"linux\"\nimage = \"nixos\"\n\
        linux = \"/boot/nixos/k3-linux-bzImage\"\ninitrd = \"/boot/nixos/i3-initrd\"\noptions = \"init=/x\"\n\n\
        [[entries]]\nid = \"nixos-generation-2\"\ntitle = \"NixOS (26.11, nixos-generation-2)\"\n\
        linux = \"/boot/nixos/k2-linux-bzImage\"\ninitrd = \"/boot/nixos/i2-initrd\"\n";

    #[test]
    fn parses_rboot_entries() {
        let entries = parse_rboot(RBOOT_TOML, Some(3));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "nixos-default");
        assert!(entries[0].is_default);
        assert_eq!(entries[0].generation, 3);
        assert_eq!(entries[0].files, vec!["boot/nixos/k3-linux-bzImage", "boot/nixos/i3-initrd"]);
        assert_eq!(entries[1].generation, 2);
        assert!(!entries[1].is_default);
    }

    #[test]
    fn parses_systemd_boot_entries() {
        let conf = "title NixOS\nversion Generation 7 NixOS 26.11, Linux 6.18\nlinux /EFI/nixos/k-linux-bzImage.efi\ninitrd /EFI/nixos/i-initrd.efi\noptions init=/x\n";
        let configs = vec![
            ("loader/loader.conf".to_owned(), "timeout 5\ndefault nixos-generation-7.conf\n".to_owned()),
            ("loader/entries/nixos-generation-7.conf".to_owned(), conf.to_owned()),
            ("loader/entries/nixos-generation-6.conf".to_owned(), conf.to_owned()),
        ];
        let entries = parse_entries(Bootloader::SystemdBoot, &configs, None);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].generation, 7);
        assert!(entries[0].is_default);
        assert!(!entries[1].is_default);
        assert_eq!(entries[0].files, vec!["EFI/nixos/k-linux-bzImage.efi", "EFI/nixos/i-initrd.efi"]);
    }

    #[test]
    fn parses_grub_entries() {
        let cfg = "set default=\"0\"\nmenuentry \"NixOS - Default\" --class nixos {\n  linux ($drive1)/kernels/k1-bzImage init=/x\n  initrd /kernels/i1-initrd\n}\n\
                   submenu \"NixOS - All configurations\" {\n\
                   menuentry \"NixOS - Configuration 4 (2026-11-01 - 6.18)\" {\n  linux /kernels/k0-bzImage\n  initrd /kernels/i0-initrd\n}\n}\n";
        let entries = parse_grub(cfg);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_default);
        assert_eq!(entries[0].files, vec!["kernels/k1-bzImage", "kernels/i1-initrd"]);
        assert_eq!(entries[1].generation, 4);
        assert_eq!(entries[1].files, vec!["kernels/k0-bzImage", "kernels/i0-initrd"]);
    }

    fn raw(path: &str) -> RawFile {
        RawFile { path: path.to_owned(), size: 1, modified: 0 }
    }

    #[test]
    fn classifies_files() {
        let entries = parse_rboot(RBOOT_TOML, Some(3));
        let files = classify(
            vec![
                raw("boot/nixos/k3-linux-bzImage"),
                raw("boot/nixos/old-linux-bzImage"),
                raw("boot/nixos/x-initrd.tmp.123"),
                raw("boot/r-boot.toml"),
                raw("boot/nixos/sub/dir-file"),
                raw("EFI/BOOT/BOOTX64.EFI"),
            ],
            &entries,
            "boot/nixos",
        );
        let state = |path: &str| {
            let file = files.iter().find(|file| file.path == path).unwrap();
            BootFileState::try_from(file.state).unwrap()
        };
        assert_eq!(state("boot/nixos/k3-linux-bzImage"), BootFileState::InUse);
        assert_eq!(state("boot/nixos/old-linux-bzImage"), BootFileState::Stale);
        assert_eq!(state("boot/nixos/x-initrd.tmp.123"), BootFileState::Partial);
        assert_eq!(state("boot/r-boot.toml"), BootFileState::Other);
        assert_eq!(state("boot/nixos/sub/dir-file"), BootFileState::Other);
        assert_eq!(state("EFI/BOOT/BOOTX64.EFI"), BootFileState::Other);
        let used = files.iter().find(|file| file.path == "boot/nixos/k3-linux-bzImage").unwrap();
        assert_eq!(used.used_by, vec!["nixos-default"]);
    }

    #[test]
    fn never_calls_files_stale_without_entries() {
        let files = classify(vec![raw("boot/nixos/a"), raw("boot/nixos/b.tmp.1")], &[], "boot/nixos");
        assert_eq!(
            BootFileState::try_from(files.iter().find(|f| f.path == "boot/nixos/a").unwrap().state),
            Ok(BootFileState::Other)
        );
        // A half-written copy is junk whatever the menu says.
        assert_eq!(
            BootFileState::try_from(files.iter().find(|f| f.path == "boot/nixos/b.tmp.1").unwrap().state),
            Ok(BootFileState::Partial)
        );
    }

    #[test]
    fn only_loader_files_can_be_deleted() {
        assert!(check_deletable("boot/nixos", "boot/nixos/abc-initrd").is_ok());
        assert!(check_deletable("boot/nixos", "boot/nixos/../../etc").is_err());
        assert!(check_deletable("boot/nixos", "boot/nixos/sub/x").is_err());
        assert!(check_deletable("boot/nixos", "boot/nixos/").is_err());
        assert!(check_deletable("boot/nixos", "boot/r-boot.toml").is_err());
        assert!(check_deletable("boot/nixos", "/boot/nixos/x").is_err());
        assert!(check_deletable("", "x").is_err());
    }
}
