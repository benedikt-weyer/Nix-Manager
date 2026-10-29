//! The files Nix Manager writes the packages you add from the store into.
//!
//! Each is a small NixOS module. Only the lines between its marker comments
//! are ours, so anything else the user puts in the file survives.

// tonic::Status is large and every handler returns it.
#![allow(clippy::result_large_err)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tonic::Status;

use crate::proto::{Destination, SetConfigPackageReply};

pub const DEFAULT_FILE: &str = "nix-manager-config.nix";

const PREFIX: &str = "nix-manager-";
const SUFFIX: &str = ".nix";

/// Present in every file we create, so we never edit one we didn't.
const MANAGED: &str = "# nix-manager:managed";
const BEGIN: &str = "# nix-manager:begin";
const END: &str = "# nix-manager:end";

const TEMPLATE: &str = "\
# nix-manager:managed
# Packages added from the store page of Nix Manager are listed between the
# two markers below. Keep one `pkgs.<attribute>` per line there; everything
# else in this file is yours. Your configuration must import this file.
{ pkgs, ... }:
{
  environment.systemPackages = [
    # nix-manager:begin
    # nix-manager:end
  ];
}
";

/// How far down the config directory to look for an import.
const MAX_DEPTH: usize = 6;
const MAX_FILES: usize = 2000;
const MAX_FILE_BYTES: u64 = 1 << 20;

/// `nix-manager-<letters, digits, - or _>.nix`
pub fn is_destination_name(file: &str) -> bool {
    file.strip_prefix(PREFIX)
        .and_then(|rest| rest.strip_suffix(SUFFIX))
        .is_some_and(|name| !name.is_empty() && name.chars().all(|c| matches!(c, 'a'..='z' | '0'..='9' | '-' | '_')))
}

/// The file name for a name the user typed: "Gaming" -> "nix-manager-gaming.nix".
fn file_for_name(name: &str) -> Result<String, Status> {
    let name = name.trim().to_lowercase();
    let name = name.strip_suffix(SUFFIX).unwrap_or(&name);
    let name = name.strip_prefix(PREFIX).unwrap_or(name);
    let file = format!("{PREFIX}{name}{SUFFIX}");
    if is_destination_name(&file) {
        Ok(file)
    } else {
        Err(Status::invalid_argument("a name may only contain letters, digits, '-' and '_'"))
    }
}

/// A nixpkgs attribute path that can be written after `pkgs.` as it is.
fn is_attr_path(attr: &str) -> bool {
    !attr.is_empty()
        && attr.split('.').all(|segment| {
            let mut chars = segment.chars();
            chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '\''))
        })
}

fn config_path(config_dir: &str) -> Result<PathBuf, Status> {
    let dir = Path::new(config_dir.strip_prefix("path:").unwrap_or(config_dir));
    if dir.is_dir() {
        Ok(dir.to_owned())
    } else {
        Err(Status::failed_precondition(format!("{config_dir} is not a directory on this machine")))
    }
}

fn io_error(dir: &Path, action: &str, error: io::Error) -> Status {
    if error.kind() == io::ErrorKind::PermissionDenied {
        Status::permission_denied(format!(
            "Cannot {action} in {}: permission denied. Make the directory writable for your user \
             (for example `sudo chown -R $USER {}`) or point Nix Manager at a copy of the configuration you own.",
            dir.display(),
            dir.display(),
        ))
    } else {
        Status::internal(format!("cannot {action} in {}: {error}", dir.display()))
    }
}

/// The attributes listed between the markers.
fn parse_packages(content: &str) -> Vec<String> {
    region(content)
        .map(|(begin, end)| {
            content
                .lines()
                .skip(begin + 1)
                .take(end - begin - 1)
                .filter_map(|line| line.trim().strip_prefix("pkgs."))
                .filter(|attr| is_attr_path(attr))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The line numbers of the begin and end markers.
fn region(content: &str) -> Option<(usize, usize)> {
    let mut lines = content.lines().map(str::trim).enumerate();
    let begin = lines.find(|(_, line)| *line == BEGIN)?.0;
    let end = lines.find(|(_, line)| *line == END)?.0;
    Some((begin, end))
}

/// `content` with `attr` added or removed; `None` when that changes nothing.
fn edited(content: &str, attr: &str, present: bool) -> Result<Option<String>, Status> {
    let (begin, end) = region(content).ok_or_else(|| {
        Status::failed_precondition(format!("the file has lost its `{BEGIN}` / `{END}` markers, so it can't be edited"))
    })?;
    let line = format!("pkgs.{attr}");
    let lines: Vec<&str> = content.lines().collect();
    let listed = lines[begin + 1..end].iter().any(|l| l.trim() == line);

    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 1);
    match (present, listed) {
        (true, false) => {
            let indent: String = lines[begin].chars().take_while(|c| c.is_whitespace()).collect();
            out.extend(lines[..end].iter().map(|l| (*l).to_owned()));
            out.push(format!("{indent}{line}"));
            out.extend(lines[end..].iter().map(|l| (*l).to_owned()));
        }
        (false, true) => {
            out.extend(
                lines
                    .iter()
                    .enumerate()
                    .filter(|(i, l)| !(*i > begin && *i < end && l.trim() == line))
                    .map(|(_, l)| (*l).to_owned()),
            );
        }
        _ => return Ok(None),
    }
    Ok(Some(out.join("\n") + "\n"))
}

fn write_file(dir: &Path, file: &str, content: &str) -> Result<(), Status> {
    let temporary = dir.join(format!(".{file}.{}.tmp", std::process::id()));
    let result = fs::write(&temporary, content).and_then(|()| fs::rename(&temporary, dir.join(file)));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|e| io_error(dir, "write the file", e))
}

/// Everything in the config directory's `.nix` files that isn't a comment or
/// one of our own files: where an import of a destination would be.
fn config_text(dir: &Path) -> String {
    let mut text = String::new();
    let mut stack = vec![(dir.to_owned(), 0)];
    let mut files = 0;
    while let Some((current, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                let skipped = name.starts_with('.')
                    || name.starts_with("result")
                    || matches!(name.as_str(), "node_modules" | "target");
                if depth < MAX_DEPTH && !skipped {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            // A symlinked file (dotfile managers do that) counts; a symlinked directory is not followed.
            let is_file = kind.is_file() || (kind.is_symlink() && fs::metadata(&path).is_ok_and(|m| m.is_file()));
            if !is_file || !name.ends_with(SUFFIX) || is_destination_name(&name) {
                continue;
            }
            files += 1;
            if files > MAX_FILES {
                return text;
            }
            if fs::metadata(&path).is_ok_and(|m| m.len() <= MAX_FILE_BYTES) {
                if let Ok(content) = fs::read_to_string(&path) {
                    for line in content.lines().filter(|l| !l.trim_start().starts_with('#')) {
                        text.push_str(line);
                        text.push('\n');
                    }
                }
            }
        }
    }
    text
}

fn describe(dir: &Path, file: &str, content: Option<&str>, config_text: &str) -> Destination {
    Destination {
        file: file.to_owned(),
        path: dir.join(file).to_string_lossy().into_owned(),
        exists: content.is_some(),
        is_default: file == DEFAULT_FILE,
        imported: config_text.contains(file),
        packages: content.map(parse_packages).unwrap_or_default(),
    }
}

/// The content of a destination, or `None` if there is no such file. A file
/// that exists but isn't ours is an error: it is never touched.
fn read_managed(dir: &Path, file: &str) -> Result<Option<String>, Status> {
    match fs::read_to_string(dir.join(file)) {
        Ok(content) if content.lines().any(|l| l.trim() == MANAGED) => Ok(Some(content)),
        Ok(_) => Err(Status::failed_precondition(format!(
            "{file} exists but wasn't created by Nix Manager, so it is left alone. Pick another name."
        ))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error(dir, "read the file", e)),
    }
}

pub fn list(config_dir: &str) -> Result<Vec<Destination>, Status> {
    let dir = config_path(config_dir)?;
    let text = config_text(&dir);

    let mut files: Vec<String> = fs::read_dir(&dir)
        .map_err(|e| io_error(&dir, "list the directory", e))?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| is_destination_name(name) && name != DEFAULT_FILE)
        .collect();
    files.sort();
    files.insert(0, DEFAULT_FILE.to_owned());

    Ok(files
        .into_iter()
        // Files that happen to match the name but aren't ours stay out of the list.
        .filter_map(|file| {
            let content = read_managed(&dir, &file).ok()?;
            Some(describe(&dir, &file, content.as_deref(), &text))
        })
        .collect())
}

pub fn create(config_dir: &str, name: &str) -> Result<Destination, Status> {
    let dir = config_path(config_dir)?;
    let file = file_for_name(name)?;
    let content = match read_managed(&dir, &file)? {
        Some(existing) => existing,
        None => {
            write_file(&dir, &file, TEMPLATE)?;
            stage_in_git(&dir, &file);
            TEMPLATE.to_owned()
        }
    };
    Ok(describe(&dir, &file, Some(&content), &config_text(&dir)))
}

pub fn set_package(config_dir: &str, file: &str, attr: &str, present: bool) -> Result<SetConfigPackageReply, Status> {
    let dir = config_path(config_dir)?;
    if !is_destination_name(file) {
        return Err(Status::invalid_argument(format!("'{file}' is not one of Nix Manager's files")));
    }
    if !is_attr_path(attr) {
        return Err(Status::invalid_argument(format!("'{attr}' can't be written as a package attribute")));
    }

    let existing = read_managed(&dir, file)?;
    if existing.is_none() && file != DEFAULT_FILE {
        return Err(Status::not_found(format!("{file} doesn't exist; create it first")));
    }
    let current = existing.clone().unwrap_or_else(|| TEMPLATE.to_owned());

    let (content, staged_in_git) = match edited(&current, attr, present)? {
        Some(changed) => {
            write_file(&dir, file, &changed)?;
            (changed, existing.is_none() && stage_in_git(&dir, file))
        }
        None => (current, false),
    };

    let destination =
        describe(&dir, file, (existing.is_some() || present).then_some(content.as_str()), &config_text(&dir));
    Ok(SetConfigPackageReply { destination: Some(destination), staged_in_git })
}

/// A flake only sees the files git knows about, so a file we just created has
/// to be added to the index. Returns whether we did.
fn stage_in_git(dir: &Path, file: &str) -> bool {
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    git(&["rev-parse", "--is-inside-work-tree"])
        && !git(&["ls-files", "--error-unmatch", "--", file])
        && git(&["add", "--", file])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("destinations-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn set(dir: &Path, file: &str, attr: &str, present: bool) -> Result<SetConfigPackageReply, Status> {
        set_package(dir.to_str().unwrap(), file, attr, present)
    }

    #[test]
    fn names_become_file_names() {
        assert_eq!(file_for_name("Gaming").unwrap(), "nix-manager-gaming.nix");
        assert_eq!(file_for_name("nix-manager-dev.nix").unwrap(), "nix-manager-dev.nix");
        assert!(file_for_name("../evil").is_err());
        assert!(file_for_name("with space").is_err());
        assert!(file_for_name("  ").is_err());
        assert!(is_destination_name(DEFAULT_FILE));
        assert!(!is_destination_name("configuration.nix"));
        assert!(!is_destination_name("nix-manager-.nix"));
    }

    #[test]
    fn only_plain_attribute_paths_are_written() {
        assert!(is_attr_path("hello"));
        assert!(is_attr_path("kdePackages.kate"));
        assert!(is_attr_path("gcc13-unwrapped"));
        assert!(!is_attr_path(""));
        assert!(!is_attr_path("a..b"));
        assert!(!is_attr_path("hello; evil"));
        assert!(!is_attr_path("1password"));
        assert!(!is_attr_path("gtk+"));
    }

    #[test]
    fn adds_and_removes_between_the_markers() {
        let added = edited(TEMPLATE, "hello", true).unwrap().unwrap();
        assert!(added.contains("    # nix-manager:begin\n    pkgs.hello\n    # nix-manager:end"), "{added}");
        assert_eq!(parse_packages(&added), ["hello"]);

        // Adding twice changes nothing.
        assert_eq!(edited(&added, "hello", true).unwrap(), None);

        let more = edited(&added, "kdePackages.kate", true).unwrap().unwrap();
        assert_eq!(parse_packages(&more), ["hello", "kdePackages.kate"]);

        let removed = edited(&more, "hello", false).unwrap().unwrap();
        assert_eq!(parse_packages(&removed), ["kdePackages.kate"]);
        assert_eq!(edited(&removed, "hello", false).unwrap(), None);
    }

    #[test]
    fn leaves_the_users_own_lines_alone() {
        let content = TEMPLATE.replace("  ];", "    pkgs.mine # not between the markers\n  ];") + "# trailing note\n";
        let added = edited(&content, "hello", true).unwrap().unwrap();
        assert!(added.contains("pkgs.mine # not between the markers"));
        assert!(added.ends_with("# trailing note\n"));

        // A line in the region that isn't a plain attribute is kept, and not listed.
        let fancy =
            TEMPLATE.replace("    # nix-manager:end", "    (pkgs.foo.override { x = 1; })\n    # nix-manager:end");
        let added = edited(&fancy, "hello", true).unwrap().unwrap();
        assert!(added.contains("(pkgs.foo.override { x = 1; })"));
        assert_eq!(parse_packages(&added), ["hello"]);
    }

    #[test]
    fn a_file_without_markers_is_not_edited() {
        assert!(edited("{ }", "hello", true).is_err());
    }

    #[test]
    fn the_default_file_is_created_on_first_add() {
        let dir = temp_dir("create");
        let listed = list(dir.to_str().unwrap()).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].is_default && !listed[0].exists);

        let reply = set(&dir, DEFAULT_FILE, "hello", true).unwrap();
        let destination = reply.destination.unwrap();
        assert!(destination.exists);
        assert_eq!(destination.packages, ["hello"]);
        assert!(fs::read_to_string(dir.join(DEFAULT_FILE)).unwrap().contains("pkgs.hello"));
        // No leftovers from the atomic write.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn other_destinations_are_listed_and_used() {
        let dir = temp_dir("others");
        let root = dir.to_str().unwrap();
        let gaming = create(root, "gaming").unwrap();
        assert_eq!(gaming.file, "nix-manager-gaming.nix");
        // Creating again returns the same one untouched.
        set(&dir, "nix-manager-gaming.nix", "steam", true).unwrap();
        assert_eq!(create(root, "gaming").unwrap().packages, ["steam"]);

        let files: Vec<String> = list(root).unwrap().into_iter().map(|d| d.file).collect();
        assert_eq!(files, [DEFAULT_FILE, "nix-manager-gaming.nix"]);

        // Only the default is created implicitly.
        assert_eq!(set(&dir, "nix-manager-nope.nix", "hello", true).unwrap_err().code(), tonic::Code::NotFound);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_that_are_not_ours_are_never_touched() {
        let dir = temp_dir("foreign");
        fs::write(dir.join(DEFAULT_FILE), "{ pkgs, ... }: { }\n").unwrap();
        let error = set(&dir, DEFAULT_FILE, "hello", true).unwrap_err();
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        assert_eq!(fs::read_to_string(dir.join(DEFAULT_FILE)).unwrap(), "{ pkgs, ... }: { }\n");
        assert!(list(dir.to_str().unwrap()).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn removing_the_last_package_keeps_the_file() {
        let dir = temp_dir("remove");
        set(&dir, DEFAULT_FILE, "hello", true).unwrap();
        let destination = set(&dir, DEFAULT_FILE, "hello", false).unwrap().destination.unwrap();
        assert!(destination.exists && destination.packages.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_import_is_noticed() {
        let dir = temp_dir("imported");
        let root = dir.to_str().unwrap();
        set(&dir, DEFAULT_FILE, "hello", true).unwrap();
        assert!(!list(root).unwrap()[0].imported);

        fs::write(dir.join("configuration.nix"), "{ imports = [\n  # ./nix-manager-config.nix\n ]; }\n").unwrap();
        assert!(!list(root).unwrap()[0].imported, "a commented-out import isn't one");

        fs::create_dir_all(dir.join("hosts")).unwrap();
        fs::write(dir.join("hosts/pc.nix"), "{ imports = [ ../nix-manager-config.nix ]; }\n").unwrap();
        assert!(list(root).unwrap()[0].imported);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_template_is_valid_nix() {
        let Ok(output) = Command::new("nix-instantiate")
            .arg("--parse")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        else {
            return; // no nix here (e.g. a build sandbox)
        };
        let mut child = output;
        let content = edited(TEMPLATE, "kdePackages.kate", true).unwrap().unwrap();
        io::Write::write_all(child.stdin.as_mut().unwrap(), content.as_bytes()).unwrap();
        let result = child.wait_with_output().unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    }
}
