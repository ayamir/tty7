//! The full working-tree diff behind the sidebar's `+N −N` counts: `git diff
//! HEAD` parsed into files → hunks → lines, for the read-only diff overlay
//! (see [`crate::ui::diff_overlay`]) that covers the terminal when the user
//! clicks a tab row's git line.
//!
//! Same discipline as [`git_status`](crate::terminal::git_status): plain
//! shell-outs run on a background executor by the caller, read-only via
//! `GIT_OPTIONAL_LOCKS=0` (through the shared [`git_status::git`] helper), and
//! never trusted to be fast — the UI shows the previous snapshot (or a loading
//! state) until a probe lands.

use std::path::{Path, PathBuf};

use crate::terminal::git_status;

/// Cap on parsed diff lines per file. A generated lockfile or vendored blob
/// can be tens of thousands of lines; past this the file's hunks stop and the
/// overlay shows a "truncated" notice instead of building a giant element
/// tree. Generous enough that real hand-written changes never hit it.
pub const MAX_LINES_PER_FILE: usize = 2000;

/// A file's added+removed size at which the overlay collapses it by default
/// (GitHub's "Load diff" treatment) — the user can still expand it by click.
pub const AUTO_COLLAPSE_LINES: u32 = 400;

/// One parsed `git diff HEAD` for a repo, plus the untracked files `diff`
/// itself can't see. This is the overlay's whole model.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct DiffSnapshot {
    /// The work-tree root the diff was taken in.
    pub root: PathBuf,
    /// Branch name (or short sha when detached) — the overlay's title.
    pub branch: String,
    /// Changed tracked files, in `git diff` order.
    pub files: Vec<FileDiff>,
    /// Untracked (new, un-added) paths, repo-relative. Listed by name only:
    /// `git diff HEAD` has no blob to diff them against, and agents create
    /// files constantly — hiding them would make the overlay look like it
    /// lost work.
    pub untracked: Vec<String>,
}

impl DiffSnapshot {
    /// Total added/removed line counts across all files — the overlay's
    /// header numbers, matching the sidebar's `+N −N` by construction (both
    /// sum per-file counts of the same `HEAD` diff).
    pub fn totals(&self) -> (u32, u32) {
        self.files
            .iter()
            .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed))
    }
}

/// How a file changed vs `HEAD` — drives the status glyph in its header row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
}

/// One changed file: its header-row facts plus the parsed hunks.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FileDiff {
    /// New path (repo-relative); for a deletion, the old path.
    pub path: String,
    /// The pre-rename path, only when `status == Renamed`.
    pub old_path: Option<String>,
    pub status: FileStatus,
    /// Lines added / removed in this file (counted from the parsed hunks).
    pub added: u32,
    pub removed: u32,
    /// Binary file — no hunks, the header row says "binary" instead.
    pub binary: bool,
    /// Hunk parsing stopped at [`MAX_LINES_PER_FILE`]; the overlay appends a
    /// "truncated" footer under the last hunk.
    pub truncated: bool,
    pub hunks: Vec<Hunk>,
}

/// One `@@` hunk: its header line (kept verbatim, function context and all)
/// and the diff lines under it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Hunk {
    /// The full `@@ -a,b +c,d @@ …` line as git printed it.
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

/// One diff line with the gutter numbers it carries: an added line has only a
/// new number, a removed line only an old one, context both.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    /// The line's text, without the leading `+`/`-`/space marker.
    pub text: String,
}

/// Probe the full diff snapshot for `cwd`, or `None` when it isn't inside a
/// git work tree. Blocking (three `git` shell-outs) — call it on a background
/// executor.
pub fn probe(cwd: &Path) -> Option<DiffSnapshot> {
    if !cwd.exists() {
        return None;
    }
    // Doubles as the "is this a repo" gate, same as the status probe.
    let root = git_status::git(cwd, &["rev-parse", "--show-toplevel"])?;
    let root = PathBuf::from(root.trim_end_matches(['\n', '\r']));
    let branch = git_status::branch_name(cwd)?;
    // `-M` folds a delete+add pair back into one rename entry; `--no-ext-diff`
    // keeps a configured external diff tool from replacing the parseable
    // unified format. A failed diff (e.g. racing a concurrent git write) still
    // yields a snapshot — an empty file list with the branch — rather than
    // hiding the overlay; the next refresh fills it in.
    let files = git_status::git(cwd, &["diff", "--no-color", "--no-ext-diff", "-M", "HEAD"])
        .map(|out| parse_unified(&out))
        .unwrap_or_default();
    // `--full-name` pins paths to the repo root regardless of which
    // subdirectory the pane sits in, matching the diff's path space.
    let untracked = git_status::git(
        cwd,
        &["ls-files", "--others", "--exclude-standard", "--full-name"],
    )
    .map(|out| out.lines().map(str::to_string).collect())
    .unwrap_or_default();
    Some(DiffSnapshot {
        root,
        branch,
        files,
        untracked,
    })
}

/// Parse `git diff` unified output into per-file structures. Tolerant by
/// construction: unrecognized metadata lines between the `diff --git` header
/// and the first hunk (modes, index, similarity) are simply skipped, so a git
/// version printing extra headers degrades to "fewer facts", never a panic.
pub fn parse_unified(out: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    // Line-number counters for the hunk currently being filled.
    let (mut old_no, mut new_no) = (0u32, 0u32);
    // Lines consumed by the current file's hunks, for the per-file cap.
    let mut file_lines = 0usize;

    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let (old_p, new_p) = parse_git_header_paths(rest);
            files.push(FileDiff {
                path: new_p.clone(),
                old_path: (old_p != new_p).then_some(old_p),
                status: FileStatus::Modified,
                added: 0,
                removed: 0,
                binary: false,
                truncated: false,
                hunks: Vec::new(),
            });
            file_lines = 0;
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue; // preamble before any header (shouldn't happen)
        };
        // ── File-level metadata between the header and the first hunk ──────
        if line.starts_with("new file mode") {
            file.status = FileStatus::Added;
            continue;
        }
        if line.starts_with("deleted file mode") {
            file.status = FileStatus::Deleted;
            continue;
        }
        if line.starts_with("rename from ") {
            file.status = FileStatus::Renamed;
            continue;
        }
        if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            file.binary = true;
            continue;
        }
        // `--- a/x` / `+++ b/x` repeat what the header said; `rename to`,
        // `index`, modes and similarity scores add nothing we render. But only
        // skip them *outside* hunk bodies — a removed line legitimately starts
        // with `--- ` inside one.
        if file.hunks.is_empty()
            && (line.starts_with("--- ") || line.starts_with("+++ ") || !is_hunk_line(line))
            && !line.starts_with("@@")
        {
            continue;
        }
        // ── Hunks ───────────────────────────────────────────────────────────
        if line.starts_with("@@") {
            if file.truncated {
                continue; // past the cap: swallow the rest of this file
            }
            let (o, n) = parse_hunk_starts(line).unwrap_or((0, 0));
            old_no = o;
            new_no = n;
            file.hunks.push(Hunk {
                header: line.to_string(),
                lines: Vec::new(),
            });
            continue;
        }
        if file.hunks.is_empty() {
            continue; // stray content outside any hunk
        }
        let (kind, text) = match line.as_bytes().first() {
            Some(b'+') => (LineKind::Added, &line[1..]),
            Some(b'-') => (LineKind::Removed, &line[1..]),
            Some(b' ') => (LineKind::Context, &line[1..]),
            // `\ No newline at end of file` and anything else: not a diff line.
            _ => continue,
        };
        // Count added/removed *before* the truncation gate: the cap is about
        // element volume, but the header numbers must stay honest, so lines
        // past the cap still count even though they're never kept.
        match kind {
            LineKind::Added => file.added += 1,
            LineKind::Removed => file.removed += 1,
            LineKind::Context => {}
        }
        if file.truncated {
            continue;
        }
        file_lines += 1;
        if file_lines > MAX_LINES_PER_FILE {
            file.truncated = true;
            continue;
        }
        let Some(hunk) = file.hunks.last_mut() else {
            continue;
        };
        let (o, n) = match kind {
            LineKind::Added => {
                let n = new_no;
                new_no += 1;
                (None, Some(n))
            }
            LineKind::Removed => {
                let o = old_no;
                old_no += 1;
                (Some(o), None)
            }
            LineKind::Context => {
                let (o, n) = (old_no, new_no);
                old_no += 1;
                new_no += 1;
                (Some(o), Some(n))
            }
        };
        hunk.lines.push(DiffLine {
            kind,
            old_no: o,
            new_no: n,
            text: text.to_string(),
        });
    }
    // A truncated file still counts +/− for its whole diff (the loop above
    // keeps counting past the cap), so totals stay consistent with numstat.
    files
}

/// Whether a line can only belong to a hunk body (`+`/`-`/space/`\` lead).
fn is_hunk_line(line: &str) -> bool {
    matches!(line.as_bytes().first(), Some(b'+' | b'-' | b' ' | b'\\')) || line.is_empty()
}

/// Split the `a/old b/new` tail of a `diff --git` header into the two paths.
///
/// Plain names split on the ` b/` separator; paths with spaces work because
/// git quotes *those* (`"a/x y" "b/x y"`), handled by the quoted branch. A
/// path containing a literal ` b/` unquoted is ambiguous in git's own format —
/// we take the last occurrence, matching git's convention of the `b/` side
/// naming the current file.
fn parse_git_header_paths(rest: &str) -> (String, String) {
    // Quoted form: "a/path with spaces" "b/path with spaces".
    if rest.starts_with('"') {
        let parts: Vec<String> = parse_quoted_pair(rest);
        if parts.len() == 2 {
            return (strip_prefix_ab(&parts[0]), strip_prefix_ab(&parts[1]));
        }
    }
    if let Some(idx) = rest.rfind(" b/") {
        let old = &rest[..idx];
        let new = &rest[idx + 1..];
        return (strip_prefix_ab(old), strip_prefix_ab(new));
    }
    // Unsplittable — show the whole tail rather than nothing.
    (rest.to_string(), rest.to_string())
}

/// Parse up to two double-quoted strings (git's C-style quoting, minus octal
/// escapes — good enough for spaces, the common case).
fn parse_quoted_pair(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut escaped = false;
    for ch in s.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_quote => escaped = true,
            '"' => {
                if in_quote {
                    parts.push(std::mem::take(&mut cur));
                }
                in_quote = !in_quote;
            }
            _ if in_quote => cur.push(ch),
            _ => {}
        }
    }
    parts
}

/// Drop the `a/` / `b/` prefix git puts on header paths.
fn strip_prefix_ab(p: &str) -> String {
    p.strip_prefix("a/")
        .or_else(|| p.strip_prefix("b/"))
        .unwrap_or(p)
        .to_string()
}

/// The old/new start line numbers from a `@@ -a,b +c,d @@` header.
fn parse_hunk_starts(line: &str) -> Option<(u32, u32)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old_part, rest) = rest.split_once(" +")?;
    let (new_part, _) = rest.split_once(" @@")?;
    let old = old_part.split(',').next()?.parse().ok()?;
    let new = new_part.split(',').next()?.parse().ok()?;
    Some((old, new))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,4 +10,5 @@ fn main() {
 let a = 1;
-let b = old();
+let b = new();
+let c = 3;
 done();
diff --git a/docs/new.md b/docs/new.md
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/docs/new.md
@@ -0,0 +1,2 @@
+hello
+world
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 4444444..0000000
--- a/gone.txt
+++ /dev/null
@@ -1,1 +0,0 @@
-bye
diff --git a/img.png b/img.png
index 5555555..6666666 100644
Binary files a/img.png and b/img.png differ
";

    /// The sample covers modify / add / delete / binary; statuses, counts, and
    /// hunk line numbers all land where the unified format says they should.
    #[test]
    fn parses_the_four_file_shapes() {
        let files = parse_unified(SAMPLE);
        assert_eq!(files.len(), 4);

        let m = &files[0];
        assert_eq!(m.path, "src/main.rs");
        assert_eq!(m.status, FileStatus::Modified);
        assert_eq!((m.added, m.removed), (2, 1));
        assert_eq!(m.hunks.len(), 1);
        assert_eq!(m.hunks[0].header, "@@ -10,4 +10,5 @@ fn main() {");
        let lines = &m.hunks[0].lines;
        assert_eq!(lines.len(), 5);
        // Context line carries both numbers, tracking the hunk starts.
        assert_eq!((lines[0].old_no, lines[0].new_no), (Some(10), Some(10)));
        assert_eq!(lines[1].kind, LineKind::Removed);
        assert_eq!(lines[1].old_no, Some(11));
        assert_eq!(lines[1].new_no, None);
        assert_eq!(lines[2].kind, LineKind::Added);
        assert_eq!(lines[2].new_no, Some(11));
        assert_eq!(lines[3].new_no, Some(12));
        assert_eq!(lines[3].text, "let c = 3;");
        // Trailing context resumes both counters.
        assert_eq!((lines[4].old_no, lines[4].new_no), (Some(12), Some(13)));

        let a = &files[1];
        assert_eq!(a.status, FileStatus::Added);
        assert_eq!((a.added, a.removed), (2, 0));

        let d = &files[2];
        assert_eq!(d.status, FileStatus::Deleted);
        assert_eq!((d.added, d.removed), (0, 1));

        let b = &files[3];
        assert!(b.binary);
        assert!(b.hunks.is_empty());
    }

    /// Renames keep both paths and don't show phantom +/− lines.
    #[test]
    fn parses_renames() {
        let out = "\
diff --git a/old/name.rs b/new/name.rs
similarity index 100%
rename from old/name.rs
rename to new/name.rs
";
        let files = parse_unified(out);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, FileStatus::Renamed);
        assert_eq!(files[0].path, "new/name.rs");
        assert_eq!(files[0].old_path.as_deref(), Some("old/name.rs"));
        assert_eq!((files[0].added, files[0].removed), (0, 0));
    }

    /// Quoted headers (paths with spaces) resolve to the unquoted paths.
    #[test]
    fn parses_quoted_paths() {
        let out = "diff --git \"a/has space.txt\" \"b/has space.txt\"\n";
        let files = parse_unified(out);
        assert_eq!(files[0].path, "has space.txt");
        assert_eq!(files[0].old_path, None);
    }

    /// A `--- ` *content* line inside a hunk is a removed line, not metadata.
    #[test]
    fn triple_dash_content_line_is_kept() {
        let out = "\
diff --git a/x.md b/x.md
index 1111111..2222222 100644
--- a/x.md
+++ b/x.md
@@ -1,2 +1,1 @@
 keep
---- a heading rule
";
        let files = parse_unified(out);
        let lines = &files[0].hunks[0].lines;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].kind, LineKind::Removed);
        // Raw `---- a heading rule` = marker `-` + content `--- a heading rule`:
        // content that *itself* starts with `--- ` must not be eaten as metadata.
        assert_eq!(lines[1].text, "--- a heading rule");
    }

    /// Past the per-file cap the hunks stop growing and the file is flagged,
    /// but the +/− counts keep counting so the header stays honest.
    #[test]
    fn caps_lines_per_file_but_keeps_counting() {
        let mut out = String::from(
            "diff --git a/big.txt b/big.txt\nindex 1..2 100644\n--- a/big.txt\n+++ b/big.txt\n@@ -0,0 +1,3000 @@\n",
        );
        for i in 0..3000 {
            out.push_str(&format!("+line {i}\n"));
        }
        let files = parse_unified(&out);
        assert!(files[0].truncated);
        assert_eq!(files[0].added, 3000);
        let kept: usize = files[0].hunks.iter().map(|h| h.lines.len()).sum();
        assert_eq!(kept, MAX_LINES_PER_FILE);
    }

    /// `\ No newline at end of file` markers are skipped, not rendered.
    #[test]
    fn skips_no_newline_marker() {
        let out = "\
diff --git a/x b/x
index 1..2 100644
--- a/x
+++ b/x
@@ -1,1 +1,1 @@
-old
\\ No newline at end of file
+new
\\ No newline at end of file
";
        let files = parse_unified(out);
        assert_eq!(files[0].hunks[0].lines.len(), 2);
        assert_eq!((files[0].added, files[0].removed), (1, 1));
    }

    /// Totals sum per-file counts.
    #[test]
    fn snapshot_totals() {
        let snap = DiffSnapshot {
            files: parse_unified(SAMPLE),
            ..Default::default()
        };
        assert_eq!(snap.totals(), (4, 2));
    }
}
