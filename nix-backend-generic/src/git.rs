//! The git side of the configuration: what changed, what is staged, committing
//! and the diff of a file. It drives the `git` command line, as the rest of the
//! backend does, so it behaves exactly as git does in the user's terminal.

// tonic::Status is large and every handler returns it.
#![allow(clippy::result_large_err)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tonic::Status;

use crate::destinations::config_path;
use crate::proto::{GitChange, GitCommitReply, GitDiffReply, GitFile, GitStatusReply};

/// A diff larger than this is cut off.
const MAX_DIFF_BYTES: usize = 1 << 20;

/// The repository that contains the config directory.
struct Repo {
    root: PathBuf,
}

impl Repo {
    /// `None` when the config directory isn't inside a git repository.
    fn open(config_dir: &str) -> Result<Option<Self>, Status> {
        let dir = config_path(config_dir)?;
        let output = git_in(&dir).args(["rev-parse", "--show-toplevel"]).output().map_err(spawn_error)?;
        if output.status.success() {
            let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            return Ok(Some(Self { root: PathBuf::from(root) }));
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("not a git repository") {
            Ok(None)
        } else {
            Err(failure(&output))
        }
    }

    fn git(&self) -> Command {
        git_in(&self.root)
    }

    /// Runs git and returns its output, or git's own complaint if it fails.
    fn run(&self, args: &[&str], paths: &[String]) -> Result<Output, Status> {
        let mut command = self.git();
        command.args(args);
        if !paths.is_empty() {
            command.arg("--").args(paths);
        }
        let output = command.output().map_err(spawn_error)?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(failure(&output))
        }
    }

    fn status(&self) -> Result<GitStatusReply, Status> {
        let output = self.run(&["status", "--porcelain=v1", "-z", "--branch", "--untracked-files=all"], &[])?;
        Ok(parse_status(&output.stdout, &self.root))
    }
}

fn git_in(dir: &Path) -> Command {
    let mut command = Command::new("git");
    // Paths from the frontend are file names, never pathspec patterns.
    command.arg("--literal-pathspecs").arg("-C").arg(dir);
    command
        // Messages are parsed, and shown to the user, in one language.
        .env("LC_ALL", "C")
        // Looking at the status must not make the repository busy for the
        // user's own git (or another frontend).
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    command
}

fn spawn_error(error: std::io::Error) -> Status {
    Status::internal(format!("could not run git: {error}"))
}

fn failure(output: &Output) -> Status {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let text = if stderr.trim().is_empty() { stdout } else { stderr };
    Status::failed_precondition(text.trim().to_owned())
}

/// A path as the frontend sends it: relative to the repository root, and
/// staying inside it.
fn check_path(path: &str) -> Result<(), Status> {
    let bad =
        path.is_empty() || path.contains('\0') || path.starts_with('/') || path.split('/').any(|part| part == "..");
    if bad {
        Err(Status::invalid_argument(format!("'{path}' is not a path inside the repository")))
    } else {
        Ok(())
    }
}

// ----------------------------------------------------------------- status --

fn change(code: u8) -> GitChange {
    match code {
        b'A' => GitChange::Added,
        b'M' => GitChange::Modified,
        b'D' => GitChange::Deleted,
        b'R' => GitChange::Renamed,
        b'C' => GitChange::Copied,
        b'T' => GitChange::TypeChanged,
        b'?' => GitChange::Untracked,
        b'U' => GitChange::Conflicted,
        _ => GitChange::None,
    }
}

/// `git status --porcelain=v1 -z --branch`: a `## branch` header, then one
/// `XY path` entry per file, each ended by a NUL. A rename or copy is followed
/// by one more entry, the path it came from.
fn parse_status(raw: &[u8], root: &Path) -> GitStatusReply {
    let mut reply = GitStatusReply { is_repo: true, repo_root: root.display().to_string(), ..Default::default() };
    let mut entries = raw.split(|byte| *byte == 0).map(|entry| String::from_utf8_lossy(entry).into_owned());

    while let Some(entry) = entries.next() {
        if let Some(header) = entry.strip_prefix("## ") {
            parse_branch(header, &mut reply);
            continue;
        }
        let bytes = entry.as_bytes();
        if bytes.len() < 4 || bytes[2] != b' ' {
            continue;
        }
        let (x, y) = (bytes[0], bytes[1]);
        let path = entry[3..].to_owned();
        let old_path = if matches!(x, b'R' | b'C') || matches!(y, b'R' | b'C') {
            entries.next().unwrap_or_default()
        } else {
            String::new()
        };

        let (staged, unstaged) = match (x, y) {
            (b'!', b'!') => continue,
            (b'?', b'?') => (GitChange::None, GitChange::Untracked),
            (b'U', _) | (_, b'U') | (b'A', b'A') | (b'D', b'D') => (GitChange::None, GitChange::Conflicted),
            _ => (change(x), change(y)),
        };
        reply.files.push(GitFile { path, old_path, staged: staged as i32, unstaged: unstaged as i32 });
    }
    reply.files.sort_by(|a, b| a.path.cmp(&b.path));
    reply
}

/// The part of the header after `## `: `main...origin/main [ahead 1, behind 2]`,
/// `main`, `No commits yet on main` or `HEAD (no branch)`.
fn parse_branch(header: &str, reply: &mut GitStatusReply) {
    if header.starts_with("HEAD (no branch)") {
        reply.detached = true;
        return;
    }
    let header = header
        .strip_prefix("No commits yet on ")
        .or_else(|| header.strip_prefix("Initial commit on "))
        .unwrap_or(header);
    let (names, counts) = match header.split_once(" [") {
        Some((names, counts)) => (names, counts.trim_end_matches(']')),
        None => (header, ""),
    };
    match names.split_once("...") {
        Some((branch, upstream)) => {
            reply.branch = branch.to_owned();
            reply.upstream = upstream.to_owned();
        }
        None => reply.branch = names.to_owned(),
    }
    for part in counts.split(", ") {
        if let Some(n) = part.strip_prefix("ahead ") {
            reply.ahead = n.parse().unwrap_or(0);
        } else if let Some(n) = part.strip_prefix("behind ") {
            reply.behind = n.parse().unwrap_or(0);
        }
    }
}

/// What the repository looks like, or an empty reply if the config directory
/// isn't in one.
pub fn status(config_dir: &str) -> Result<GitStatusReply, Status> {
    match Repo::open(config_dir)? {
        Some(repo) => repo.status(),
        None => Ok(GitStatusReply::default()),
    }
}

fn open(config_dir: &str) -> Result<Repo, Status> {
    Repo::open(config_dir)?
        .ok_or_else(|| Status::failed_precondition(format!("{config_dir} is not inside a git repository")))
}

// ---------------------------------------------------------------- staging --

/// Stages (or unstages) `paths`, or everything when it is empty.
pub fn set_staged(config_dir: &str, paths: &[String], staged: bool) -> Result<GitStatusReply, Status> {
    paths.iter().try_for_each(|path| check_path(path))?;
    let repo = open(config_dir)?;
    if staged {
        // -A so that a deleted file is staged as deleted too.
        repo.run(&["add", "-A"], paths)?;
    } else {
        // `reset` rather than `restore --staged`: it also works before the first commit.
        repo.run(&["reset", "-q"], paths)?;
    }
    repo.status()
}

/// Commits what is staged with `message`.
pub fn commit(config_dir: &str, message: &str) -> Result<GitCommitReply, Status> {
    let message = message.trim();
    if message.is_empty() {
        return Err(Status::invalid_argument("a commit needs a message"));
    }
    let repo = open(config_dir)?;
    if !repo.status()?.files.iter().any(|file| file.staged != GitChange::None as i32) {
        return Err(Status::failed_precondition("nothing is staged, so there is nothing to commit"));
    }

    // The message goes in on stdin so that it can be as long and as odd as the user likes.
    let mut child = repo
        .git()
        .args(["commit", "--file=-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(spawn_error)?;
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let text = format!("{message}\n");
    // A hook that exits early closes the pipe; git's own message explains why.
    let _ = stdin.write_all(text.as_bytes());
    drop(stdin);
    let output = child.wait_with_output().map_err(spawn_error)?;
    if !output.status.success() {
        return Err(failure(&output));
    }

    let hash = repo.run(&["rev-parse", "--short", "HEAD"], &[])?;
    let subject = repo.run(&["log", "-1", "--format=%s"], &[])?;
    Ok(GitCommitReply {
        hash: String::from_utf8_lossy(&hash.stdout).trim().to_owned(),
        subject: String::from_utf8_lossy(&subject.stdout).trim().to_owned(),
    })
}

// ------------------------------------------------------------------- diff --

/// The unified diff of one file: the staged change, or the unstaged one (for
/// a file git doesn't know yet, the whole file as an addition).
pub fn diff(config_dir: &str, path: &str, old_path: &str, staged: bool) -> Result<GitDiffReply, Status> {
    check_path(path)?;
    if !old_path.is_empty() {
        check_path(old_path)?;
    }
    let repo = open(config_dir)?;

    let mut command = repo.git();
    command.args(["diff", "--no-color", "--no-ext-diff", "--no-textconv"]);
    let mut exit_one_is_a_diff = false;
    if staged {
        command.args(["--cached", "-M", "--", path]);
        if !old_path.is_empty() {
            command.arg(old_path);
        }
    } else if tracked(&repo, path)? {
        command.args(["--", path]);
    } else {
        // Not in git yet: compare with nothing. `--no-index` exits with 1 when
        // the files differ, which is the point here.
        command.args(["--no-index", "--", "/dev/null", path]);
        exit_one_is_a_diff = true;
    }

    let mut child = command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(spawn_error)?;
    let mut out = Vec::new();
    // One byte over the limit tells a diff of exactly the limit from a longer one.
    child
        .stdout
        .take()
        .expect("stdout was piped")
        .take(MAX_DIFF_BYTES as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|error| Status::internal(format!("could not read git's output: {error}")))?;
    let truncated = out.len() > MAX_DIFF_BYTES;
    if truncated {
        let _ = child.kill();
        out.truncate(MAX_DIFF_BYTES);
    }
    let output = child.wait_with_output().map_err(spawn_error)?;
    let code = output.status.code();
    if !truncated && code != Some(0) && !(exit_one_is_a_diff && code == Some(1)) {
        return Err(failure(&output));
    }

    let mut text = String::from_utf8_lossy(&out).into_owned();
    if truncated {
        // Do not end on half a line.
        text.truncate(text.rfind('\n').map_or(0, |end| end + 1));
    }
    Ok(GitDiffReply { text, truncated })
}

/// Whether git knows the file (it may be gone from the working tree).
fn tracked(repo: &Repo, path: &str) -> Result<bool, Status> {
    let output = repo
        .git()
        .args(["ls-files", "--error-unmatch", "--", path])
        .stderr(Stdio::null())
        .output()
        .map_err(spawn_error)?;
    Ok(output.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn parse(text: &str) -> GitStatusReply {
        parse_status(text.replace('|', "\0").as_bytes(), Path::new("/repo"))
    }

    fn file<'a>(reply: &'a GitStatusReply, path: &str) -> &'a GitFile {
        reply.files.iter().find(|f| f.path == path).unwrap_or_else(|| panic!("{path} is not listed"))
    }

    #[test]
    fn reads_the_branch_header() {
        let reply = parse("## main...origin/main [ahead 2, behind 1]|");
        assert_eq!((reply.branch.as_str(), reply.upstream.as_str()), ("main", "origin/main"));
        assert_eq!((reply.ahead, reply.behind), (2, 1));

        let reply = parse("## work|");
        assert_eq!((reply.branch.as_str(), reply.upstream.as_str(), reply.ahead), ("work", "", 0));

        assert_eq!(parse("## No commits yet on main|").branch, "main");
        assert!(parse("## HEAD (no branch)|").detached);
        assert_eq!(parse("## main...origin/main [gone]|").upstream, "origin/main");
    }

    #[test]
    fn reads_every_kind_of_entry() {
        let reply = parse(
            "## main|M  staged.nix| M edited.nix|MM both.nix|A  new.nix|?? loose.nix| D gone.nix|\
             R  renamed.nix|old name.nix|UU clash.nix|!! ignored|",
        );
        assert_eq!(reply.files.len(), 8, "{:?}", reply.files);

        let staged = file(&reply, "staged.nix");
        assert_eq!((staged.staged, staged.unstaged), (GitChange::Modified as i32, GitChange::None as i32));
        let edited = file(&reply, "edited.nix");
        assert_eq!((edited.staged, edited.unstaged), (GitChange::None as i32, GitChange::Modified as i32));
        let both = file(&reply, "both.nix");
        assert_eq!((both.staged, both.unstaged), (GitChange::Modified as i32, GitChange::Modified as i32));
        assert_eq!(file(&reply, "new.nix").staged, GitChange::Added as i32);
        assert_eq!(file(&reply, "loose.nix").unstaged, GitChange::Untracked as i32);
        assert_eq!(file(&reply, "gone.nix").unstaged, GitChange::Deleted as i32);
        assert_eq!(file(&reply, "clash.nix").unstaged, GitChange::Conflicted as i32);

        let renamed = file(&reply, "renamed.nix");
        assert_eq!((renamed.staged, renamed.old_path.as_str()), (GitChange::Renamed as i32, "old name.nix"));
    }

    #[test]
    fn paths_must_stay_inside_the_repository() {
        assert!(check_path("flake.nix").is_ok());
        assert!(check_path("hosts/laptop/default.nix").is_ok());
        assert!(check_path("").is_err());
        assert!(check_path("/etc/passwd").is_err());
        assert!(check_path("../outside").is_err());
        assert!(check_path("a/../../b").is_err());
    }

    fn repo_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("git-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.name", "Test"],
            &["config", "user.email", "test@example.com"],
            &["config", "commit.gpgsign", "false"],
        ] {
            assert!(git_in(&dir).args(args).status().unwrap().success());
        }
        dir
    }

    fn names(reply: &GitStatusReply, staged: bool) -> Vec<&str> {
        reply
            .files
            .iter()
            .filter(|f| (if staged { f.staged } else { f.unstaged }) != GitChange::None as i32)
            .map(|f| f.path.as_str())
            .collect()
    }

    #[test]
    fn a_directory_that_is_not_a_repository_says_so() {
        let dir = std::env::temp_dir().join(format!("git-plain-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // A temp dir can sit inside a repository on some machines; only check the usual case.
        if Repo::open(dir.to_str().unwrap()).unwrap().is_none() {
            assert!(!status(dir.to_str().unwrap()).unwrap().is_repo);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stages_unstages_and_commits_from_the_first_commit_on() {
        let dir = repo_dir("flow");
        let cfg = dir.to_str().unwrap();
        fs::write(dir.join("a.nix"), "one\n").unwrap();
        fs::write(dir.join("b.nix"), "two\n").unwrap();

        let reply = status(cfg).unwrap();
        assert!(reply.is_repo);
        assert_eq!(reply.branch, "main");
        assert_eq!(names(&reply, false), ["a.nix", "b.nix"]);

        // Before there is a commit at all.
        let reply = set_staged(cfg, &["a.nix".to_owned()], true).unwrap();
        assert_eq!(names(&reply, true), ["a.nix"]);
        let reply = set_staged(cfg, &["a.nix".to_owned()], false).unwrap();
        assert!(names(&reply, true).is_empty());
        let reply = set_staged(cfg, &[], true).unwrap();
        assert_eq!(names(&reply, true), ["a.nix", "b.nix"]);
        let reply = set_staged(cfg, &[], false).unwrap();
        assert!(names(&reply, true).is_empty());

        // The message is required, and so is something to commit.
        assert_eq!(commit(cfg, "  ").unwrap_err().code(), tonic::Code::InvalidArgument);
        assert_eq!(commit(cfg, "empty").unwrap_err().code(), tonic::Code::FailedPrecondition);

        set_staged(cfg, &["a.nix".to_owned()], true).unwrap();
        let done = commit(cfg, "Add a\n\nWith a body # and a hash").unwrap();
        assert_eq!(done.subject, "Add a");
        assert!(!done.hash.is_empty());
        let body = git_in(&dir).args(["log", "-1", "--format=%B"]).output().unwrap();
        assert!(String::from_utf8_lossy(&body.stdout).contains("With a body # and a hash"));

        let reply = status(cfg).unwrap();
        assert_eq!(names(&reply, false), ["b.nix"], "the other file stays out of the commit");
        assert!(names(&reply, true).is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn diffs_staged_unstaged_untracked_and_renamed_files() {
        let dir = repo_dir("diff");
        let cfg = dir.to_str().unwrap();
        fs::write(dir.join("a.nix"), "one\ntwo\nthree\n").unwrap();
        set_staged(cfg, &[], true).unwrap();
        commit(cfg, "start").unwrap();

        // Staged, and then edited again: the two diffs are different.
        fs::write(dir.join("a.nix"), "one\nTWO\nthree\n").unwrap();
        set_staged(cfg, &["a.nix".to_owned()], true).unwrap();
        fs::write(dir.join("a.nix"), "one\nTWO\nTHREE\n").unwrap();

        let staged = diff(cfg, "a.nix", "", true).unwrap();
        assert!(staged.text.contains("-two\n+TWO\n"), "{}", staged.text);
        assert!(!staged.text.contains("THREE"));
        let unstaged = diff(cfg, "a.nix", "", false).unwrap();
        assert!(unstaged.text.contains("-three\n+THREE\n"), "{}", unstaged.text);
        assert!(!unstaged.truncated);

        // Nothing differs.
        set_staged(cfg, &[], true).unwrap();
        assert_eq!(diff(cfg, "a.nix", "", false).unwrap().text, "");

        // A file git has never seen is shown as added.
        fs::write(dir.join("new.nix"), "fresh\n").unwrap();
        let untracked = diff(cfg, "new.nix", "", false).unwrap();
        assert!(untracked.text.contains("+fresh") && untracked.text.contains("new file mode"), "{}", untracked.text);

        // A rename is a rename only when both names are asked about.
        commit(cfg, "second").unwrap();
        git_in(&dir).args(["mv", "a.nix", "b.nix"]).status().unwrap();
        let reply = status(cfg).unwrap();
        let renamed = file(&reply, "b.nix");
        assert_eq!(renamed.old_path, "a.nix");
        let text = diff(cfg, "b.nix", "a.nix", true).unwrap().text;
        assert!(text.contains("rename from a.nix") && text.contains("rename to b.nix"), "{text}");

        // Unstaging a rename needs both names too.
        let reply = set_staged(cfg, &["b.nix".to_owned(), "a.nix".to_owned()], false).unwrap();
        assert!(names(&reply, true).is_empty(), "{:?}", reply.files);

        // A deleted file still has a diff.
        fs::remove_file(dir.join("b.nix")).ok();
        fs::remove_file(dir.join("a.nix")).ok();
        let gone = diff(cfg, "a.nix", "", false).unwrap();
        assert!(gone.text.contains("deleted file mode"), "{}", gone.text);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_huge_diff_is_cut_off_at_a_line() {
        let dir = repo_dir("huge");
        let cfg = dir.to_str().unwrap();
        let line = "x".repeat(99) + "\n";
        fs::write(dir.join("big.txt"), line.repeat(MAX_DIFF_BYTES / 100 + 500)).unwrap();

        let reply = diff(cfg, "big.txt", "", false).unwrap();
        assert!(reply.truncated);
        assert!(reply.text.len() <= MAX_DIFF_BYTES && reply.text.ends_with('\n'));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_git_command_reports_gits_own_words() {
        let dir = repo_dir("fail");
        let cfg = dir.to_str().unwrap();
        // Staging something that isn't there.
        let error = set_staged(cfg, &["missing.nix".to_owned()], true).unwrap_err();
        assert!(error.message().contains("missing.nix"), "{}", error.message());
        let _ = fs::remove_dir_all(&dir);
    }
}
