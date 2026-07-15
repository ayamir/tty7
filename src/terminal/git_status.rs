//! A lightweight git snapshot for a pane's working directory — the current
//! branch and the working-tree diff size — rendered as the sidebar row's third
//! line (`⎇ feat/x  +6 −5`): each session fronted with its branch and change
//! count.
//!
//! Snapshots are shared through [`GitStatusCache`], a process-wide map keyed
//! by work-tree root: every pane whose cwd resolves into the same repo reads
//! the *same* entry, so ten tabs in one repo show one truth, refreshed by
//! whichever pane probed last — not ten drifting copies refreshed on ten
//! different schedules. Probes stay per-trigger (a pane's cwd change, command
//! end, or agent-turn end — see [`crate::terminal::view`]) but are deduped
//! in-flight, so simultaneous triggers from panes in the same directory cost
//! one `git` shell-out, not one per pane.
//!
//! Deliberately shell-out simple: one `git` invocation per field, run on a
//! background thread by the caller so the UI never blocks on a slow repo.
//! Read-only — `GIT_OPTIONAL_LOCKS=0` keeps status polling from ever taking
//! `index.lock` and fighting a real git command the user is running.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A repo's git snapshot: the branch it's on and how much the working tree has
/// changed against `HEAD`. `added`/`removed` sum the per-file line counts from
/// `git diff --numstat HEAD` (tracked staged + unstaged changes); binary files
/// and untracked files don't contribute a line count.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GitStatus {
    /// The branch name (`main`, `feat/x`), or a short commit sha when the HEAD
    /// is detached. Never empty.
    pub branch: String,
    /// Lines added across the working tree vs `HEAD`.
    pub added: u32,
    /// Lines removed across the working tree vs `HEAD`.
    pub removed: u32,
}

/// One raw probe result, before it's folded into the cache: which work tree
/// `cwd` belongs to, plus the fields probed there. `counts` is `None` when the
/// `git diff` invocation itself failed (e.g. it raced a concurrent git write) —
/// distinct from a clean tree's `Some((0, 0))`, so the cache can keep the
/// previous numbers instead of pretending the tree went clean.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RepoSnapshot {
    /// The work tree root (`git rev-parse --show-toplevel`) — the cache key
    /// every pane inside this repo shares.
    pub root: PathBuf,
    pub branch: String,
    pub counts: Option<(u32, u32)>,
}

/// Probe the git snapshot for `cwd`, or `None` when it isn't inside a git work
/// tree (or the path is gone). Blocking — call it on a background executor.
pub fn probe(cwd: &Path) -> Option<RepoSnapshot> {
    if !cwd.exists() {
        return None;
    }
    // Doubles as the "is this a git repo" gate: fails outside a work tree.
    let root = git(cwd, &["rev-parse", "--show-toplevel"])?;
    let root = PathBuf::from(root.trim_end_matches(['\n', '\r']));
    let branch = branch_name(cwd)?;
    Some(RepoSnapshot {
        root,
        branch,
        counts: diff_numstat(cwd),
    })
}

/// The process-wide snapshot store (a gpui [`Global`](gpui::Global)): pane
/// cwds grouped by work-tree root, one [`GitStatus`] per root. Views read
/// through [`status_for`](Self::status_for); the probe loop in
/// [`crate::terminal::view`] brackets each background probe with
/// [`begin_probe`](Self::begin_probe) / [`finish_probe`](Self::finish_probe).
///
/// In-flight dedup is keyed by cwd (the root isn't known until a first probe
/// answers), so two panes at the same directory share one probe; panes in
/// *different* subdirectories of one repo can still race a redundant probe —
/// rare, and both land the same answer.
#[derive(Default)]
pub struct GitStatusCache {
    /// cwd → its work-tree root; `None` = probed and found not to be a repo.
    roots: HashMap<PathBuf, Option<PathBuf>>,
    /// root → the snapshot every pane in that tree shares.
    status: HashMap<PathBuf, GitStatus>,
    /// cwds with a probe currently in flight, so concurrent triggers fold
    /// into one shell-out.
    in_flight: HashSet<PathBuf>,
    /// In-flight cwds re-triggered meanwhile — reprobed once their flight
    /// lands, so the newest trigger's state is never skipped.
    dirty: HashSet<PathBuf>,
}

impl gpui::Global for GitStatusCache {}

impl GitStatusCache {
    /// The snapshot for a pane at `cwd`: resolved through its work-tree root,
    /// so every pane in the same repo answers identically. `None` before the
    /// first probe lands or when `cwd` isn't in a repo.
    pub fn status_for(&self, cwd: &Path) -> Option<GitStatus> {
        let root = self.roots.get(cwd)?.as_ref()?;
        self.status.get(root).cloned()
    }

    /// Claim a probe for `cwd`. `false` means one is already in flight — the
    /// caller must *not* spawn another; the landed flight will reprobe once
    /// (the cwd is marked dirty) so this trigger's state still gets observed.
    pub fn begin_probe(&mut self, cwd: &Path) -> bool {
        if self.in_flight.contains(cwd) {
            self.dirty.insert(cwd.to_path_buf());
            false
        } else {
            self.in_flight.insert(cwd.to_path_buf());
            true
        }
    }

    /// Fold a landed probe for `cwd` into the cache. A failed diff inside a
    /// live repo keeps the root's previous counts (a transient `git` error is
    /// not "the tree went clean"). Returns whether the cwd was re-triggered
    /// while this probe flew — the caller should start one more probe.
    pub fn finish_probe(&mut self, cwd: &Path, snapshot: Option<RepoSnapshot>) -> bool {
        self.in_flight.remove(cwd);
        match snapshot {
            Some(snap) => {
                let (added, removed) = snap.counts.unwrap_or_else(|| {
                    self.status
                        .get(&snap.root)
                        .map(|g| (g.added, g.removed))
                        .unwrap_or((0, 0))
                });
                self.status.insert(
                    snap.root.clone(),
                    GitStatus {
                        branch: snap.branch,
                        added,
                        removed,
                    },
                );
                self.roots.insert(cwd.to_path_buf(), Some(snap.root));
            }
            // Not a repo (or the dir vanished). The root's entry stays for
            // other cwds that still live in it.
            None => {
                self.roots.insert(cwd.to_path_buf(), None);
            }
        }
        self.dirty.remove(cwd)
    }
}

/// The current branch name, or a short sha for a detached HEAD.
fn branch_name(cwd: &Path) -> Option<String> {
    // On a branch — even before the first commit — `symbolic-ref` names it.
    if let Some(out) = git(cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"]) {
        let name = out.trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    // Detached HEAD (or a rebase/bisect): fall back to the short commit sha.
    let sha = git(cwd, &["rev-parse", "--short", "HEAD"])?;
    let sha = sha.trim();
    (!sha.is_empty()).then(|| sha.to_string())
}

/// Sum added/removed lines across the working tree vs `HEAD` from
/// `git diff --numstat HEAD`. Binary files (`-\t-`) contribute nothing.
/// `None` when the invocation itself failed — the caller keeps old counts.
fn diff_numstat(cwd: &Path) -> Option<(u32, u32)> {
    let out = git(cwd, &["diff", "--numstat", "HEAD"])?;
    let mut added = 0u32;
    let mut removed = 0u32;
    for line in out.lines() {
        let mut fields = line.split('\t');
        if let Some(n) = fields.next().and_then(|s| s.parse::<u32>().ok()) {
            added += n;
        }
        if let Some(n) = fields.next().and_then(|s| s.parse::<u32>().ok()) {
            removed += n;
        }
    }
    Some((added, removed))
}

/// Run `git -C <cwd> <args>` and return stdout on success, `None` on a
/// non-zero exit or a missing `git`. `GIT_OPTIONAL_LOCKS=0` makes the read
/// truly read-only; stdin is nulled so a misconfigured git can't block on a
/// prompt.
fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tmp path that is not a git repo yields no snapshot (and never panics).
    #[test]
    fn non_repo_is_none() {
        let dir = std::env::temp_dir().join("tty7-git-status-not-a-repo-xyz");
        let _ = std::fs::create_dir_all(&dir);
        assert_eq!(probe(&dir), None);
    }

    /// A path that doesn't exist is `None`, not a panic.
    #[test]
    fn missing_path_is_none() {
        assert_eq!(probe(Path::new("/no/such/tty7/path/here")), None);
    }

    /// This repo (the crate root is inside the tty7 work tree) reports a branch
    /// and a root, exercising the real `git` probe end-to-end.
    #[test]
    fn own_repo_has_a_branch_and_root() {
        let here = env!("CARGO_MANIFEST_DIR");
        if let Some(snap) = probe(Path::new(here)) {
            assert!(!snap.branch.is_empty());
            assert!(Path::new(here).starts_with(&snap.root));
        }
        // If the crate is built outside a work tree (e.g. a vendored tarball),
        // `None` is the correct answer and the assertions above are skipped.
    }

    fn snap(root: &str, branch: &str, counts: Option<(u32, u32)>) -> RepoSnapshot {
        RepoSnapshot {
            root: PathBuf::from(root),
            branch: branch.into(),
            counts,
        }
    }

    /// Two cwds landing in the same work tree share one entry: a probe from
    /// either updates what both read (the group-by-root contract).
    #[test]
    fn cwds_in_one_repo_share_a_snapshot() {
        let mut cache = GitStatusCache::default();
        let (a, b) = (Path::new("/repo/sub/a"), Path::new("/repo"));
        cache.finish_probe(a, Some(snap("/repo", "main", Some((5, 2)))));
        cache.finish_probe(b, Some(snap("/repo", "main", Some((5, 2)))));
        // A later probe from `a` refreshes the numbers `b` reads too.
        cache.finish_probe(a, Some(snap("/repo", "main", Some((200, 42)))));
        for cwd in [a, b] {
            let got = cache.status_for(cwd).unwrap();
            assert_eq!((got.added, got.removed), (200, 42), "cwd {cwd:?}");
        }
    }

    /// A failed `git diff` (counts `None`) keeps the previous numbers rather
    /// than rendering the tree as suddenly clean; the branch still updates.
    #[test]
    fn failed_diff_keeps_previous_counts() {
        let mut cache = GitStatusCache::default();
        let cwd = Path::new("/repo");
        cache.finish_probe(cwd, Some(snap("/repo", "main", Some((200, 42)))));
        cache.finish_probe(cwd, Some(snap("/repo", "feat/x", None)));
        let got = cache.status_for(cwd).unwrap();
        assert_eq!(got.branch, "feat/x");
        assert_eq!((got.added, got.removed), (200, 42));
    }

    /// In-flight dedup: a second trigger while a probe flies doesn't claim a
    /// new one, but marks the cwd dirty so the landing reports "go again".
    #[test]
    fn concurrent_triggers_fold_into_one_probe_then_rerun() {
        let mut cache = GitStatusCache::default();
        let cwd = Path::new("/repo");
        assert!(cache.begin_probe(cwd));
        assert!(!cache.begin_probe(cwd)); // deduped, marked dirty
        assert!(cache.finish_probe(cwd, Some(snap("/repo", "main", Some((1, 0))))));
        // The rerun claims cleanly and lands with nothing pending.
        assert!(cache.begin_probe(cwd));
        assert!(!cache.finish_probe(cwd, Some(snap("/repo", "main", Some((1, 0))))));
    }

    /// A cwd that leaves the repo (dir deleted / not a work tree) stops
    /// answering, without disturbing the root entry other cwds still use.
    #[test]
    fn non_repo_cwd_clears_only_itself() {
        let mut cache = GitStatusCache::default();
        let (a, b) = (Path::new("/repo/a"), Path::new("/repo/b"));
        cache.finish_probe(a, Some(snap("/repo", "main", Some((3, 1)))));
        cache.finish_probe(b, Some(snap("/repo", "main", Some((3, 1)))));
        cache.finish_probe(a, None);
        assert_eq!(cache.status_for(a), None);
        assert!(cache.status_for(b).is_some());
    }
}
