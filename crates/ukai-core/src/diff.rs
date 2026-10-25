//! A checkout's changes as `git diff` prints them, read into files and the
//! lines of their hunks, for the diff pane to show and comment on.
//!
//! Paths come from the `---` / `+++` and `rename` lines, which say each side
//! on its own, rather than from the `diff --git` line, which cannot be split
//! where a path has a space; that line only names a file with no hunks (a
//! binary one, or a mode change), when both of its halves are the same.

use crate::review::{self, DiffLine, Hunk, LineKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Deleted,
    Modified,
    /// Moved from `from`, maybe changed too.
    Renamed {
        from: String,
    },
}

impl ChangeKind {
    /// One letter, as `git status --short` puts it.
    pub fn letter(&self) -> &'static str {
        match self {
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Modified => "M",
            Self::Renamed { .. } => "R",
        }
    }
}

/// One file's change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Relative to the checkout's root, `/`-separated; the old path of a
    /// deleted file.
    pub path: String,
    pub kind: ChangeKind,
    /// Git says the contents differ but shows no lines.
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

impl FileChange {
    /// Lines added and removed.
    pub fn stats(&self) -> (usize, usize) {
        review::stats(&self.hunks)
    }
}

/// Files changed, lines added, and lines removed across `files`.
pub fn totals(files: &[FileChange]) -> (usize, usize, usize) {
    files.iter().fold((0, 0, 0), |(n, added, removed), file| {
        let (a, r) = file.stats();
        (n + 1, added + a, removed + r)
    })
}

/// The files of `patch`, in its order. A patch cut short ends with what was
/// read of its last file.
pub fn parse(patch: &str) -> Vec<FileChange> {
    let mut files = Vec::new();
    let mut current: Option<Building> = None;
    for line in patch.lines() {
        if let Some(header) = line.strip_prefix("diff --git ") {
            files.extend(current.take().map(Building::finish));
            current = Some(Building::new(header));
            continue;
        }
        let Some(file) = current.as_mut() else {
            continue;
        };
        if let Some(range) = line.strip_prefix("@@ ") {
            file.start_hunk(range);
        } else if file.in_hunk {
            file.push_line(line);
        } else {
            file.header_line(line);
        }
    }
    files.extend(current.map(Building::finish));
    files
}

/// A file being read.
struct Building {
    /// The `diff --git` line's path, when both of its halves agree.
    header_path: Option<String>,
    old: Option<String>,
    new: Option<String>,
    rename_from: Option<String>,
    rename_to: Option<String>,
    added: bool,
    deleted: bool,
    binary: bool,
    hunks: Vec<Hunk>,
    in_hunk: bool,
    /// The next line's number on each side.
    old_line: usize,
    new_line: usize,
}

impl Building {
    fn new(header: &str) -> Self {
        Self {
            header_path: same_halves(header),
            old: None,
            new: None,
            rename_from: None,
            rename_to: None,
            added: false,
            deleted: false,
            binary: false,
            hunks: Vec::new(),
            in_hunk: false,
            old_line: 0,
            new_line: 0,
        }
    }

    fn header_line(&mut self, line: &str) {
        if line.starts_with("new file mode") {
            self.added = true;
        } else if line.starts_with("deleted file mode") {
            self.deleted = true;
        } else if let Some(path) = line.strip_prefix("rename from ") {
            self.rename_from = Some(unquote(path));
        } else if let Some(path) = line.strip_prefix("rename to ") {
            self.rename_to = Some(unquote(path));
        } else if let Some(path) = line.strip_prefix("--- ") {
            self.old = side(path, "a/");
        } else if let Some(path) = line.strip_prefix("+++ ") {
            self.new = side(path, "b/");
        } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
            self.binary = true;
        }
    }

    /// `-12,3 +12,4 @@ fn context`
    fn start_hunk(&mut self, range: &str) {
        let start = |sign: char| {
            range
                .split_whitespace()
                .find_map(|part| part.strip_prefix(sign))
                .and_then(|part| part.split(',').next())
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(0)
        };
        // A side with no lines starts "at" 0; its next line would be 1.
        self.old_line = start('-').max(1);
        self.new_line = start('+').max(1);
        self.hunks.push(Vec::new());
        self.in_hunk = true;
    }

    fn push_line(&mut self, line: &str) {
        let (kind, text) = match line.chars().next() {
            Some('+') => (LineKind::Added, &line[1..]),
            Some('-') => (LineKind::Removed, &line[1..]),
            Some(' ') => (LineKind::Context, &line[1..]),
            // Some tools strip the space off a blank context line.
            None => (LineKind::Context, ""),
            // `\ No newline at end of file`, and anything else.
            Some(_) => return,
        };
        let (old, new) = match kind {
            LineKind::Added => (None, Some(self.new_line)),
            LineKind::Removed => (Some(self.old_line), None),
            LineKind::Context => (Some(self.old_line), Some(self.new_line)),
        };
        if old.is_some() {
            self.old_line += 1;
        }
        if new.is_some() {
            self.new_line += 1;
        }
        if let Some(hunk) = self.hunks.last_mut() {
            hunk.push(DiffLine {
                kind,
                old,
                new,
                text: text.trim_end_matches('\r').to_owned(),
            });
        }
    }

    fn finish(self) -> FileChange {
        let deleted = self.deleted || (self.new.is_none() && self.old.is_some());
        let added = self.added || (self.old.is_none() && self.new.is_some());
        let path = self
            .rename_to
            .clone()
            .or_else(|| if deleted { self.old.clone() } else { None })
            .or(self.new)
            .or(self.old)
            .or(self.header_path)
            .unwrap_or_default();
        let kind = match self.rename_from {
            Some(from) => ChangeKind::Renamed { from },
            None if deleted => ChangeKind::Deleted,
            None if added => ChangeKind::Added,
            None => ChangeKind::Modified,
        };
        FileChange {
            path,
            kind,
            binary: self.binary,
            hunks: self.hunks,
        }
    }
}

/// The path of a `---` / `+++` line; `None` for `/dev/null`.
fn side(path: &str, prefix: &str) -> Option<String> {
    // A timestamp may follow a tab, as with `--no-index`.
    let path = path.split('\t').next().unwrap_or(path);
    if path == "/dev/null" {
        return None;
    }
    let path = unquote(path);
    Some(path.strip_prefix(prefix).map(str::to_owned).unwrap_or(path))
}

/// `a/x b/x` → `x`; `None` when the halves differ or cannot be told apart.
fn same_halves(header: &str) -> Option<String> {
    let rest = header.strip_prefix("a/")?;
    // `<p> b/<p>`: the path is half of what is left, less ` b/`.
    let len = rest.len().checked_sub(3)? / 2;
    let (path, other) = (rest.get(..len)?, rest.get(len..)?);
    (other.strip_prefix(" b/")? == path).then(|| path.to_owned())
}

/// A path as git prints it, without the quotes and escapes it adds to one
/// with unusual characters.
fn unquote(path: &str) -> String {
    let Some(inner) = path
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return path.to_owned();
    };
    let mut bytes = Vec::with_capacity(inner.len());
    let mut chars = inner.bytes().peekable();
    while let Some(byte) = chars.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        match chars.next() {
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(digit @ b'0'..=b'7') => {
                // Three octal digits: one byte of a UTF-8 sequence.
                let mut value = u32::from(digit - b'0');
                for _ in 0..2 {
                    if let Some(next @ b'0'..=b'7') = chars.peek().copied() {
                        value = value * 8 + u32::from(next - b'0');
                        chars.next();
                    }
                }
                bytes.push(u8::try_from(value).unwrap_or(b'?'));
            }
            Some(other) => bytes.push(other),
            None => {}
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn line(kind: LineKind, old: Option<usize>, new: Option<usize>, text: &str) -> DiffLine {
        DiffLine {
            kind,
            old,
            new,
            text: text.into(),
        }
    }

    const MODIFIED: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@ mod a;
 fn a() {}
-fn b() {}
+fn b() -> u8 { 1 }
+fn c() {}

@@ -10,2 +11,2 @@
-old
+new
 end
\\ No newline at end of file
";

    #[test]
    fn numbers_the_lines_of_each_hunk_on_both_sides() {
        let files = parse(MODIFIED);
        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert_eq!(file.path, "src/lib.rs");
        assert_eq!(file.kind, ChangeKind::Modified);
        assert_eq!(
            file.hunks,
            [
                vec![
                    line(LineKind::Context, Some(1), Some(1), "fn a() {}"),
                    line(LineKind::Removed, Some(2), None, "fn b() {}"),
                    line(LineKind::Added, None, Some(2), "fn b() -> u8 { 1 }"),
                    line(LineKind::Added, None, Some(3), "fn c() {}"),
                    // A blank context line with its space stripped.
                    line(LineKind::Context, Some(3), Some(4), ""),
                ],
                vec![
                    line(LineKind::Removed, Some(10), None, "old"),
                    line(LineKind::Added, None, Some(11), "new"),
                    line(LineKind::Context, Some(11), Some(12), "end"),
                ],
            ]
        );
        assert_eq!(file.stats(), (3, 2));
        assert_eq!(totals(&files), (1, 3, 2));
    }

    #[test]
    fn tells_added_deleted_renamed_and_binary_files() {
        let patch = "\
diff --git a/new.txt b/new.txt
new file mode 100644
index 0000000..3b18e51
--- /dev/null
+++ b/new.txt
@@ -0,0 +1 @@
+hello
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 3b18e51..0000000
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
diff --git a/old name.md b/docs/new name.md
similarity index 90%
rename from old name.md
rename to docs/new name.md
index 1111111..2222222 100644
--- a/old name.md
+++ b/docs/new name.md
@@ -1 +1 @@
-a
+b
diff --git a/moved.rs b/elsewhere/moved.rs
similarity index 100%
rename from moved.rs
rename to elsewhere/moved.rs
diff --git a/logo.png b/logo.png
index 1111111..2222222 100644
Binary files a/logo.png and b/logo.png differ
";
        let files = parse(patch);
        let summary: Vec<(&str, &ChangeKind, bool, (usize, usize))> = files
            .iter()
            .map(|f| (f.path.as_str(), &f.kind, f.binary, f.stats()))
            .collect();
        assert_eq!(
            summary,
            [
                ("new.txt", &ChangeKind::Added, false, (1, 0)),
                ("gone.txt", &ChangeKind::Deleted, false, (0, 1)),
                (
                    "docs/new name.md",
                    &ChangeKind::Renamed {
                        from: "old name.md".into()
                    },
                    false,
                    (1, 1)
                ),
                (
                    "elsewhere/moved.rs",
                    &ChangeKind::Renamed {
                        from: "moved.rs".into()
                    },
                    false,
                    (0, 0)
                ),
                ("logo.png", &ChangeKind::Modified, true, (0, 0)),
            ]
        );
        assert_eq!(files[0].hunks[0][0].new, Some(1));
        assert_eq!(files[1].hunks[0][0].old, Some(1));
    }

    #[test]
    fn reads_an_untracked_file_diffed_against_nothing() {
        // `git diff --no-index -- /dev/null notes.txt`
        let patch = "\
diff --git a/notes.txt b/notes.txt
new file mode 100644
index 0000000..e69de29
--- /dev/null
+++ b/notes.txt
@@ -0,0 +1,2 @@
+one
+two
";
        let files = parse(patch);
        assert_eq!(files[0].path, "notes.txt");
        assert_eq!(files[0].kind, ChangeKind::Added);
        assert_eq!(files[0].stats(), (2, 0));
    }

    #[test]
    fn keeps_what_was_read_of_a_patch_cut_short() {
        let cut = &MODIFIED[..MODIFIED.find("+fn c").unwrap_or(MODIFIED.len())];
        let files = parse(cut);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].stats(), (1, 1));
    }

    #[rstest]
    #[case::empty("", 0)]
    #[case::not_a_diff("hello\nworld\n", 0)]
    #[case::header_only("diff --git a/a b/a\nold mode 100644\nnew mode 100755\n", 1)]
    fn reads_what_is_there(#[case] patch: &str, #[case] count: usize) {
        assert_eq!(parse(patch).len(), count);
    }

    #[test]
    fn names_a_mode_change_from_its_header() {
        let files = parse("diff --git a/run me.sh b/run me.sh\nold mode 100644\nnew mode 100755\n");
        assert_eq!(files[0].path, "run me.sh");
        assert_eq!(files[0].kind, ChangeKind::Modified);
    }

    #[rstest]
    #[case::plain("a/b.rs", "a/b.rs")]
    #[case::quoted_tab(r#""a/tab\there""#, "a/tab\there")]
    #[case::quoted_quote(r#""a/say \"hi\"""#, "a/say \"hi\"")]
    #[case::octal_utf8(r#""a/\346\227\245.md""#, "a/日.md")]
    fn unquotes_paths(#[case] quoted: &str, #[case] path: &str) {
        assert_eq!(unquote(quoted), path);
    }

    #[test]
    fn letters_say_the_kind() {
        assert_eq!(ChangeKind::Added.letter(), "A");
        assert_eq!(ChangeKind::Deleted.letter(), "D");
        assert_eq!(ChangeKind::Modified.letter(), "M");
        assert_eq!(
            ChangeKind::Renamed {
                from: String::new()
            }
            .letter(),
            "R"
        );
    }
}
