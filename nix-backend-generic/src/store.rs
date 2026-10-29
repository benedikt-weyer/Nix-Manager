//! Package search and details for the store page, answered by `nix` itself so
//! they always match the nixpkgs the target flake has locked.

// tonic::Status is large and every handler returns it.
#![allow(clippy::result_large_err)]

use std::process::Stdio;

use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command;
use tonic::Status;

use crate::config::ResolvedTarget;
use crate::proto::{ListInstalledReply, Maintainer, Package, PackageSummary, SearchPackagesReply};

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;

/// `p: { ... }` - the facts the UI shows about one derivation. Every field is
/// coerced to the type the backend expects, because nixpkgs metadata is
/// hand-written and not always what it should be.
const PACKAGE_INFO: &str = r#"
p:
let
  str = x: if builtins.isString x then x else "";
  m = if builtins.isAttrs (p.meta or null) then p.meta else { };
  first = x: if builtins.isList x then (if x == [ ] then "" else str (builtins.head x)) else str x;
  licenseName = x: if builtins.isAttrs x then str (x.fullName or x.shortName or x.spdxId or "") else str x;
  licenses =
    l:
    if l == null then
      [ ]
    else
      builtins.filter (n: n != "") (map licenseName (if builtins.isList l then l else [ l ]));
  maintainer = x: {
    name = str (x.name or "");
    github = str (x.github or "");
  };
in
{
  name = str (p.name or "");
  pname = str (p.pname or "");
  version = str (p.version or "");
  description = str (m.description or "");
  longDescription = str (m.longDescription or "");
  homepage = first (m.homepage or "");
  position = str (m.position or "");
  mainProgram = str (m.mainProgram or "");
  licenses = licenses (m.license or null);
  maintainers = map maintainer (builtins.filter builtins.isAttrs (if builtins.isList (m.maintainers or null) then m.maintainers else [ ]));
}
"#;

/// `info: cfg: [ ... ]` - every package a NixOS configuration installs, as
/// `info` describes them: system packages, per-user packages and
/// home-manager packages when that module is in use. A package that fails to
/// evaluate is reduced to its name rather than losing the whole list.
const INSTALLED: &str = r#"
info: cfg:
let
  lib = cfg.pkgs.lib;
  fromUsers = lib.concatMap (u: u.packages or [ ]) (lib.attrValues cfg.config.users.users);
  fromHome =
    if cfg.config ? home-manager then
      lib.concatMap (u: u.home.packages or [ ]) (lib.attrValues cfg.config.home-manager.users)
    else
      [ ];
  all = cfg.config.environment.systemPackages ++ fromUsers ++ fromHome;
  describe =
    p:
    let
      i = info p;
      r = builtins.tryEval (builtins.deepSeq i i);
    in
    if r.success then r.value else { name = if builtins.isString (p.name or null) then p.name else ""; };
in
map describe (builtins.filter builtins.isAttrs all)
"#;

/// The nixpkgs a flake has locked, and how to refer to it.
#[derive(Debug, PartialEq)]
pub struct Nixpkgs {
    /// Something `nix search` / `nix eval` accept in front of `#`.
    flake_ref: String,
    /// For display, e.g. "nixpkgs @ 31ecbac".
    label: String,
    /// `(owner, repo, rev)` when it is a github nixpkgs, which is what makes
    /// links to the exact source possible.
    github: Option<(String, String, String)>,
}

impl Nixpkgs {
    /// Whatever the registry calls `nixpkgs`: used when the flake doesn't lock
    /// one we can read.
    fn registry() -> Self {
        Self { flake_ref: "nixpkgs".to_owned(), label: "nixpkgs (registry)".to_owned(), github: None }
    }

    /// Reads `flake.lock` in `config_dir` (a directory, or a `path:` flake reference).
    pub fn locate(config_dir: &str) -> Self {
        let dir = config_dir.strip_prefix("path:").unwrap_or(config_dir);
        std::fs::read_to_string(std::path::Path::new(dir).join("flake.lock"))
            .ok()
            .and_then(|lock| Self::from_lock(&lock))
            .unwrap_or_else(Self::registry)
    }

    /// Prefers the root input called `nixpkgs`, else any root input that locks
    /// NixOS/nixpkgs.
    fn from_lock(lock: &str) -> Option<Self> {
        let lock: Value = serde_json::from_str(lock).ok()?;
        let nodes = lock.get("nodes")?;
        let root = lock.get("root")?.as_str()?;
        let inputs = nodes.get(root)?.get("inputs")?.as_object()?;

        let locked_of = |input: &Value| -> Option<&Value> {
            let name = node_name(nodes, root, input)?;
            nodes.get(&name)?.get("locked")
        };
        let is_nixpkgs = |locked: &Value| {
            locked.get("owner").and_then(Value::as_str) == Some("NixOS")
                && locked.get("repo").and_then(Value::as_str) == Some("nixpkgs")
        };

        let locked = inputs
            .get("nixpkgs")
            .and_then(locked_of)
            .or_else(|| inputs.values().filter_map(locked_of).find(|locked| is_nixpkgs(locked)))?;
        Self::from_locked(locked)
    }

    fn from_locked(locked: &Value) -> Option<Self> {
        let field = |name: &str| locked.get(name).and_then(Value::as_str);
        match field("type")? {
            kind @ ("github" | "gitlab") => {
                let (owner, repo, rev) = (field("owner")?, field("repo")?, field("rev")?);
                Some(Self {
                    flake_ref: format!("{kind}:{owner}/{repo}/{rev}"),
                    label: format!("{repo} @ {}", rev.get(..7).unwrap_or(rev)),
                    github: (kind == "github" && repo == "nixpkgs")
                        .then(|| (owner.to_owned(), repo.to_owned(), rev.to_owned())),
                })
            }
            "git" => {
                let (url, rev) = (field("url")?, field("rev")?);
                Some(Self {
                    flake_ref: format!("git+{url}?rev={rev}"),
                    label: format!("{url} @ {}", rev.get(..7).unwrap_or(rev)),
                    github: None,
                })
            }
            "tarball" => {
                field("url").map(|url| Self { flake_ref: url.to_owned(), label: url.to_owned(), github: None })
            }
            "path" => field("path").map(|path| Self {
                flake_ref: format!("path:{path}"),
                label: path.to_owned(),
                github: None,
            }),
            _ => None,
        }
    }

    /// Where `relative` (a path inside nixpkgs) lives on GitHub.
    fn blob_url(&self, relative: &str, line: u32) -> String {
        let (owner, repo, rev) = match &self.github {
            Some((owner, repo, rev)) => (owner.as_str(), repo.as_str(), rev.as_str()),
            None => ("NixOS", "nixpkgs", "master"),
        };
        let anchor = if line > 0 { format!("#L{line}") } else { String::new() };
        format!("https://github.com/{owner}/{repo}/blob/{rev}/{relative}{anchor}")
    }
}

/// Follows a lock-file input (a node name, or a path of input names for an
/// input that `follows` another) to the node it names.
fn node_name(nodes: &Value, from: &str, input: &Value) -> Option<String> {
    match input {
        Value::String(name) => Some(name.clone()),
        Value::Array(path) => {
            let mut current = from.to_owned();
            for segment in path {
                let next = nodes.get(&current)?.get("inputs")?.get(segment.as_str()?)?;
                current = node_name(nodes, &current, next)?;
            }
            Some(current)
        }
        _ => None,
    }
}

/// The platform part of nixpkgs' `legacyPackages.<system>`.
fn current_system() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

/// Runs `nix` and parses its JSON output. Dropping the future (a client that
/// went away) kills the process.
async fn nix_json(args: &[&str]) -> Result<Value, Status> {
    let output = Command::new("nix")
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| Status::unavailable(format!("failed to start nix: {e}")))?;
    if !output.status.success() {
        return Err(Status::internal(error_text(&output.stderr)));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| Status::internal(format!("unreadable nix output: {e}")))
}

/// The end of nix's stderr, which is where the actual error is. `nix search`
/// narrates every attribute it evaluates there; that isn't an error.
fn error_text(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> =
        text.lines().filter(|line| !line.trim().is_empty() && !line.starts_with("evaluating '")).collect();
    let tail = &lines[lines.len().saturating_sub(8)..];
    if tail.is_empty() {
        "nix failed without saying why".to_owned()
    } else {
        tail.join("\n")
    }
}

/// Makes `word` match literally when nix compiles it as a regex.
fn escape_regex(word: &str) -> String {
    let mut out = String::with_capacity(word.len() + 4);
    for (index, c) in word.chars().enumerate() {
        // A leading '-' would be taken for an option.
        if "\\^$.|?*+()[]{}".contains(c) || (c == '-' && index == 0) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Library sets and cross-compilation variants drown the programs in noise;
/// everything at the top level, and the sets people install programs from
/// (`kdePackages`, `xfce`, ...), stay.
fn is_app(attr: &str) -> bool {
    let Some((set, _)) = attr.split_once('.') else { return true };
    !(set.starts_with("pkgs")
        || set.starts_with("haskell")
        || (set.ends_with("Packages") && set != "kdePackages")
        || set.ends_with("Plugins")
        || matches!(set, "tests" | "nixosTests" | "dockerTools" | "linuxKernel"))
}

/// Smaller is a better match for `query` (lowercase, whole).
fn rank(summary: &PackageSummary, query: &str) -> (u8, bool, usize) {
    let attr = summary.attr.to_lowercase();
    let pname = summary.pname.to_lowercase();
    let quality = if attr == query || pname == query {
        0
    } else if attr.starts_with(query) || pname.starts_with(query) {
        1
    } else if attr.contains(query) || pname.contains(query) {
        2
    } else {
        3
    };
    (quality, attr.contains('.'), attr.len())
}

pub async fn search(target: &ResolvedTarget, query: &str, limit: u32) -> Result<SearchPackagesReply, Status> {
    let words: Vec<String> = query.split_whitespace().map(escape_regex).collect();
    if words.is_empty() {
        return Err(Status::invalid_argument("the search query is empty"));
    }
    let limit = if limit == 0 { DEFAULT_LIMIT } else { (limit as usize).min(MAX_LIMIT) };

    let source = Nixpkgs::locate(&target.config_dir);
    let mut args = vec!["search", source.flake_ref.as_str()];
    args.extend(words.iter().map(String::as_str));
    args.push("--json");
    let found = nix_json(&args).await?;

    let mut packages: Vec<PackageSummary> = found
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| {
            let attr = key.strip_prefix("legacyPackages.")?.split_once('.')?.1;
            let text = |name: &str| value.get(name).and_then(Value::as_str).unwrap_or_default().to_owned();
            is_app(attr).then(|| PackageSummary {
                attr: attr.to_owned(),
                pname: text("pname"),
                version: text("version"),
                description: text("description"),
            })
        })
        .collect();

    let query = query.trim().to_lowercase();
    packages.sort_by_cached_key(|summary| (rank(summary, &query), summary.attr.clone()));
    let total_matches = packages.len() as u32;
    packages.truncate(limit);

    Ok(SearchPackagesReply { packages, total_matches, source: source.label })
}

pub async fn package(target: &ResolvedTarget, attr: &str) -> Result<Package, Status> {
    if attr.is_empty() || !attr.chars().all(|c| c.is_ascii_alphanumeric() || "_.-'+".contains(c)) {
        return Err(Status::invalid_argument(format!("'{attr}' is not a package attribute")));
    }

    let source = Nixpkgs::locate(&target.config_dir);
    let installable = format!("{}#legacyPackages.{}.{attr}", source.flake_ref, current_system());
    let value = nix_json(&["eval", "--json", &installable, "--apply", PACKAGE_INFO]).await?;
    let info: Info =
        serde_json::from_value(value).map_err(|e| Status::internal(format!("unreadable package details: {e}")))?;
    Ok(info.into_package(attr, &source))
}

pub async fn installed(target: &ResolvedTarget) -> Result<ListInstalledReply, Status> {
    let source = Nixpkgs::locate(&target.config_dir);
    let installable = format!("{}#nixosConfigurations.{}", target.config_dir, target.host_name);
    let apply = format!("({INSTALLED}) ({PACKAGE_INFO})");
    let value = nix_json(&["eval", "--json", &installable, "--apply", &apply]).await?;
    let infos: Vec<Info> =
        serde_json::from_value(value).map_err(|e| Status::internal(format!("unreadable package list: {e}")))?;

    let mut packages: Vec<Package> =
        infos.into_iter().filter(|info| !info.name.is_empty()).map(|info| info.into_package("", &source)).collect();
    packages.sort_by_cached_key(|package| (package.name.to_lowercase(), package.name.clone()));
    packages.dedup_by(|a, b| a.name == b.name);
    Ok(ListInstalledReply { packages })
}

/// What [`PACKAGE_INFO`] returns.
#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Info {
    name: String,
    pname: String,
    version: String,
    description: String,
    long_description: String,
    homepage: String,
    position: String,
    main_program: String,
    licenses: Vec<String>,
    maintainers: Vec<Contact>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Contact {
    name: String,
    github: String,
}

impl Info {
    fn into_package(self, attr: &str, source: &Nixpkgs) -> Package {
        let (source_path, source_line, source_url) = locate_definition(&self.position, source);
        Package {
            attr: attr.to_owned(),
            name: self.name,
            pname: self.pname,
            version: self.version,
            description: self.description,
            long_description: self.long_description.trim().to_owned(),
            homepage: self.homepage,
            licenses: self.licenses,
            maintainers: self
                .maintainers
                .into_iter()
                .map(|contact| Maintainer { name: contact.name, github: contact.github })
                .collect(),
            main_program: self.main_program,
            source_path,
            source_line,
            source_url,
        }
    }
}

/// Splits `meta.position` (`/nix/store/<hash>-source/pkgs/x/package.nix:58`)
/// into the path inside nixpkgs, the line, and where that is on GitHub.
fn locate_definition(position: &str, source: &Nixpkgs) -> (String, u32, String) {
    if position.is_empty() {
        return (String::new(), 0, String::new());
    }
    let (path, line) = match position.rsplit_once(':') {
        Some((path, line)) => (path, line.parse().unwrap_or(0)),
        None => (position, 0),
    };
    let Some((_, relative)) = path.split_once("-source/") else {
        return (path.to_owned(), line, String::new());
    };
    let in_nixpkgs = ["pkgs/", "nixos/", "lib/", "maintainers/"].iter().any(|dir| relative.starts_with(dir));
    let url = if in_nixpkgs { source.blob_url(relative, line) } else { String::new() };
    (relative.to_owned(), line, url)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = r#"{
      "nodes": {
        "crane": { "locked": { "type": "github", "owner": "ipetkov", "repo": "crane", "rev": "e675a36b7377bebe41d548388a2501cbb16d380d" } },
        "nixpkgs": { "locked": { "type": "github", "owner": "NixOS", "repo": "nixpkgs", "rev": "31ecbaca1bdefa45dffc409f0250bd8039ea0010" } },
        "root": { "inputs": { "crane": "crane", "nixpkgs": "nixpkgs" } }
      },
      "root": "root", "version": 7
    }"#;

    #[test]
    fn finds_the_locked_nixpkgs() {
        let nixpkgs = Nixpkgs::from_lock(LOCK).unwrap();
        assert_eq!(nixpkgs.flake_ref, "github:NixOS/nixpkgs/31ecbaca1bdefa45dffc409f0250bd8039ea0010");
        assert_eq!(nixpkgs.label, "nixpkgs @ 31ecbac");
        assert!(nixpkgs.github.is_some());
    }

    #[test]
    fn follows_an_input_that_follows_another() {
        let lock = r#"{
          "nodes": {
            "nixpkgs": { "locked": { "type": "github", "owner": "NixOS", "repo": "nixpkgs", "rev": "31ecbaca1bdefa45dffc409f0250bd8039ea0010" } },
            "other": { "inputs": { "nixpkgs": "nixpkgs" } },
            "root": { "inputs": { "other": "other", "unstable": ["other", "nixpkgs"] } }
          },
          "root": "root", "version": 7
        }"#;
        // No input is called `nixpkgs`; it is found by following `unstable` and by what it locks.
        assert_eq!(Nixpkgs::from_lock(lock).unwrap().label, "nixpkgs @ 31ecbac");
    }

    #[test]
    fn no_nixpkgs_in_the_lock_means_the_registry() {
        let lock = r#"{"nodes":{"root":{"inputs":{}}},"root":"root","version":7}"#;
        assert_eq!(Nixpkgs::from_lock(lock), None);
        assert_eq!(Nixpkgs::locate("/definitely/not/here"), Nixpkgs::registry());
    }

    #[test]
    fn links_to_the_locked_revision() {
        let source = Nixpkgs::from_lock(LOCK).unwrap();
        let (path, line, url) =
            locate_definition("/nix/store/abc-source/pkgs/by-name/he/hello/package.nix:58", &source);
        assert_eq!(path, "pkgs/by-name/he/hello/package.nix");
        assert_eq!(line, 58);
        assert_eq!(
            url,
            "https://github.com/NixOS/nixpkgs/blob/31ecbaca1bdefa45dffc409f0250bd8039ea0010/pkgs/by-name/he/hello/package.nix#L58"
        );
    }

    #[test]
    fn other_sources_get_no_github_link() {
        let source = Nixpkgs::registry();
        let (path, _, url) = locate_definition("/nix/store/abc-source/modules/foo.nix:3", &source);
        assert_eq!((path.as_str(), url.as_str()), ("modules/foo.nix", ""));
        assert_eq!(locate_definition("", &source), (String::new(), 0, String::new()));
        // Without a known revision the link falls back to master.
        assert!(locate_definition("/nix/store/abc-source/pkgs/a.nix:1", &source).2.contains("/blob/master/"));
    }

    #[test]
    fn queries_match_literally() {
        assert_eq!(escape_regex("g++"), r"g\+\+");
        assert_eq!(escape_regex("-v"), r"\-v");
        assert_eq!(escape_regex("a-b"), "a-b");
    }

    #[test]
    fn library_sets_are_not_apps() {
        assert!(is_app("hello"));
        assert!(is_app("kdePackages.kate"));
        assert!(is_app("xfce.thunar"));
        assert!(!is_app("python3Packages.requests"));
        assert!(!is_app("haskellPackages.pandoc"));
        assert!(!is_app("pkgsCross.aarch64-multiplatform.hello"));
    }

    #[test]
    fn exact_matches_rank_first() {
        let package =
            |attr: &str| PackageSummary { attr: attr.to_owned(), pname: attr.to_owned(), ..Default::default() };
        let mut found =
            vec![package("hello-wayland"), package("kdePackages.hello"), package("hello"), package("shello")];
        found.sort_by_cached_key(|p| (rank(p, "hello"), p.attr.clone()));
        let order: Vec<&str> = found.iter().map(|p| p.attr.as_str()).collect();
        assert_eq!(order, ["hello", "hello-wayland", "shello", "kdePackages.hello"]);
    }

    #[test]
    fn nix_noise_is_not_an_error() {
        let stderr = b"evaluating 'legacyPackages.x86_64-linux.a'...\nerror: attribute 'nope' missing\n";
        assert_eq!(error_text(stderr), "error: attribute 'nope' missing");
    }

    /// Against a real nix and a real flake; run with
    /// `NIX_BACKEND_TEST_FLAKE=path:<dir> cargo test -- --ignored`, where the
    /// flake has a `nixosConfigurations.testhost` that installs `hello` and
    /// `git` system-wide and `cowsay` for a user.
    #[tokio::test]
    #[ignore]
    async fn live() {
        let config_dir = std::env::var("NIX_BACKEND_TEST_FLAKE").expect("NIX_BACKEND_TEST_FLAKE");
        let target = ResolvedTarget { config_dir, host_name: "testhost".to_owned() };

        let found = search(&target, "hello", 10).await.unwrap();
        assert_eq!(found.packages[0].attr, "hello");
        assert!(found.source.starts_with("nixpkgs @ "));

        let hello = package(&target, "hello").await.unwrap();
        assert_eq!(hello.pname, "hello");
        assert!(!hello.maintainers.is_empty());
        assert!(hello.source_url.contains("pkgs/by-name/he/hello/package.nix#L"), "{}", hello.source_url);

        let installed = installed(&target).await.unwrap();
        let names: Vec<&str> = installed.packages.iter().map(|p| p.pname.as_str()).collect();
        for expected in ["hello", "git", "cowsay"] {
            assert!(names.contains(&expected), "{expected} missing from {names:?}");
        }
    }
}
