//! Reads the unified diff text the backend sends into lines a frontend can
//! draw: what kind each is, and its line numbers in the old and new file.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `@@ -1,4 +1,5 @@ context`: where a group of changes starts.
    Hunk,
    Context,
    Added,
    Removed,
    /// Something about the file rather than its content: `new file mode`,
    /// `rename from`, `Binary files differ`, `\ No newline at end of file`.
    Note,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub kind: Kind,
    /// Line number in the old file, for context and removed lines.
    pub old: Option<u32>,
    /// Line number in the new file, for context and added lines.
    pub new: Option<u32>,
    /// Without the leading `+`, `-` or space. Tabs are expanded and a trailing
    /// carriage return dropped, so that every frontend can draw it as it is.
    pub text: String,
}

/// How many lines are added and removed.
pub fn counts(lines: &[Line]) -> (usize, usize) {
    lines.iter().fold((0, 0), |(added, removed), line| match line.kind {
        Kind::Added => (added + 1, removed),
        Kind::Removed => (added, removed + 1),
        _ => (added, removed),
    })
}

/// Where a hunk is, and how much of it is left to read.
struct Hunk {
    old: u32,
    new: u32,
    old_left: u32,
    new_left: u32,
    /// A merge's combined diff, which has two columns of markers and no
    /// counts worth following.
    combined: bool,
}

/// The diff of one file, as `git diff` prints it.
pub fn parse(diff: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut hunk: Option<Hunk> = None;

    for raw in diff.lines() {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);

        // The counts in a hunk's header say how many lines belong to it, which
        // is what tells a removed line that starts with "--" from a file header.
        if let Some(open) = hunk.as_mut() {
            let reading = open.combined || open.old_left > 0 || open.new_left > 0;
            let ends = open.combined && (raw.starts_with("diff ") || raw.starts_with("@@"));
            if reading && !ends {
                if let Some(body) = body_line(open, raw) {
                    lines.push(body);
                    continue;
                }
            }
        }

        let note = |kind, text: &str| Line { kind, old: None, new: None, text: text.to_owned() };
        if raw.starts_with("@@") {
            hunk = parse_hunk_header(raw);
            lines.push(note(Kind::Hunk, raw));
        } else if is_file_header(raw) {
            hunk = None;
        } else if !raw.is_empty() {
            hunk = None;
            lines.push(note(Kind::Note, raw));
        }
    }
    for line in &mut lines {
        line.text = line.text.replace('\t', "    ");
    }
    lines
}

/// One line inside a hunk, and the hunk's progress. `None` if it isn't one.
fn body_line(hunk: &mut Hunk, raw: &str) -> Option<Line> {
    let (old, new) = (hunk.old, hunk.new);
    if hunk.combined {
        let markers: String = raw.chars().take(2).collect();
        let text: String = raw.chars().skip(2).collect();
        let kind = if markers.contains('+') {
            Kind::Added
        } else if markers.contains('-') {
            Kind::Removed
        } else {
            Kind::Context
        };
        return Some(Line { kind, old: None, new: None, text });
    }

    let (kind, text) = match raw.chars().next() {
        Some('+') => (Kind::Added, &raw[1..]),
        Some('-') => (Kind::Removed, &raw[1..]),
        Some(' ') => (Kind::Context, &raw[1..]),
        // An empty context line whose lone space was stripped on the way.
        None => (Kind::Context, ""),
        Some('\\') => return Some(Line { kind: Kind::Note, old: None, new: None, text: raw[1..].trim().to_owned() }),
        Some(_) => return None,
    };
    let (old, new) = match kind {
        Kind::Added => {
            hunk.new += 1;
            hunk.new_left = hunk.new_left.saturating_sub(1);
            (None, Some(new))
        }
        Kind::Removed => {
            hunk.old += 1;
            hunk.old_left = hunk.old_left.saturating_sub(1);
            (Some(old), None)
        }
        _ => {
            hunk.old += 1;
            hunk.new += 1;
            hunk.old_left = hunk.old_left.saturating_sub(1);
            hunk.new_left = hunk.new_left.saturating_sub(1);
            (Some(old), Some(new))
        }
    };
    Some(Line { kind, old, new, text: text.to_owned() })
}

/// `@@ -12,5 +14,7 @@ section`, or a combined diff's `@@@ -1,2 -1,2 +1,3 @@@`.
fn parse_hunk_header(raw: &str) -> Option<Hunk> {
    if raw.starts_with("@@@") {
        return Some(Hunk { old: 0, new: 0, old_left: 0, new_left: 0, combined: true });
    }
    let mut words = raw.split_whitespace().skip(1);
    let (old_start, old_count) = range(words.next()?.strip_prefix('-')?)?;
    let (new_start, new_count) = range(words.next()?.strip_prefix('+')?)?;
    Some(Hunk { old: old_start, new: new_start, old_left: old_count, new_left: new_count, combined: false })
}

/// `12,5` is five lines from line 12; `12` is one line.
fn range(text: &str) -> Option<(u32, u32)> {
    match text.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((text.parse().ok()?, 1)),
    }
}

/// The lines that only say which file the diff is about.
fn is_file_header(raw: &str) -> bool {
    raw.starts_with("diff --git ")
        || raw.starts_with("diff --cc ")
        || raw.starts_with("diff --combined ")
        || raw.starts_with("index ")
        || raw.starts_with("--- ")
        || raw.starts_with("+++ ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(lines: &[Line]) -> Vec<Kind> {
        lines.iter().map(|l| l.kind).collect()
    }

    const EDIT: &str = "\
diff --git a/flake.nix b/flake.nix
index 83d6f2a..0f1e2b3 100644
--- a/flake.nix
+++ b/flake.nix
@@ -3,5 +3,6 @@ {
   description = \"x\";
-  inputs = old;
+  inputs = new;
+  extra = true;
   outputs = y;

 }
";

    #[test]
    fn numbers_the_lines_of_both_files() {
        let lines = parse(EDIT);
        assert_eq!(
            kinds(&lines),
            [
                Kind::Hunk,
                Kind::Context,
                Kind::Removed,
                Kind::Added,
                Kind::Added,
                Kind::Context,
                Kind::Context,
                Kind::Context
            ]
        );
        assert_eq!((lines[1].old, lines[1].new), (Some(3), Some(3)));
        assert_eq!((lines[2].old, lines[2].new), (Some(4), None));
        assert_eq!((lines[3].old, lines[3].new), (None, Some(4)));
        assert_eq!((lines[4].old, lines[4].new), (None, Some(5)));
        assert_eq!((lines[5].old, lines[5].new), (Some(5), Some(6)));
        assert_eq!(lines[2].text, "  inputs = old;");
        assert_eq!(counts(&lines), (2, 1));
    }

    #[test]
    fn a_removed_line_that_looks_like_a_header_is_still_content() {
        let lines = parse("@@ -1,2 +1,1 @@\n--- not a header\n keep\n");
        assert_eq!(kinds(&lines), [Kind::Hunk, Kind::Removed, Kind::Context]);
        assert_eq!(lines[1].text, "-- not a header");

        let lines = parse("@@ -0,0 +1,1 @@\n+++ not a header\n");
        assert_eq!(lines[1].kind, Kind::Added);
        assert_eq!(lines[1].text, "++ not a header");
    }

    #[test]
    fn several_hunks_and_files_each_start_over() {
        let lines = parse(
            "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-a\n+b\n@@ -10 +10 @@\n-c\n+d\n\
             diff --git a/z b/z\n--- a/z\n+++ b/z\n@@ -5 +5 @@\n-e\n+f\n",
        );
        assert_eq!(lines.iter().filter(|l| l.kind == Kind::Hunk).count(), 3);
        assert_eq!(lines.iter().find(|l| l.text == "d").unwrap().new, Some(10));
        assert_eq!(lines.iter().find(|l| l.text == "f").unwrap().new, Some(5));
        assert_eq!(counts(&lines), (3, 3));
    }

    #[test]
    fn describes_the_file_with_notes() {
        let lines = parse("diff --git a/n b/n\nnew file mode 100644\nindex 0000000..e69de29\n--- /dev/null\n+++ b/n\n");
        assert_eq!(kinds(&lines), [Kind::Note]);
        assert_eq!(lines[0].text, "new file mode 100644");

        let lines = parse("diff --git a/a b/b\nsimilarity index 100%\nrename from a\nrename to b\n");
        assert_eq!(
            lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            ["similarity index 100%", "rename from a", "rename to b"]
        );

        let lines = parse("diff --git a/i.png b/i.png\nindex 1..2 100644\nBinary files a/i.png and b/i.png differ\n");
        assert_eq!(lines.len(), 1);
        assert!(lines[0].text.starts_with("Binary files"));

        let lines = parse("@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+b\n");
        assert_eq!(kinds(&lines), [Kind::Hunk, Kind::Removed, Kind::Note, Kind::Added]);
        assert_eq!(lines[2].text, "No newline at end of file");
    }

    #[test]
    fn tidies_tabs_and_line_endings() {
        let lines = parse("@@ -1 +1 @@\r\n-\told\r\n+\tnew\r\n");
        assert_eq!(lines[1].text, "    old");
        assert_eq!(lines[2].text, "    new");
    }

    #[test]
    fn empty_input_has_no_lines() {
        assert!(parse("").is_empty());
        assert_eq!(counts(&[]), (0, 0));
    }

    #[test]
    fn a_conflict_diff_is_readable() {
        let lines = parse(
            "diff --cc f\nindex 1,2..3\n--- a/f\n+++ b/f\n@@@ -1,2 -1,2 +1,6 @@@\n  same\n- ours\n +theirs\n++both\n",
        );
        assert_eq!(kinds(&lines), [Kind::Hunk, Kind::Context, Kind::Removed, Kind::Added, Kind::Added]);
        assert_eq!(lines[1].text, "same");
    }
}
