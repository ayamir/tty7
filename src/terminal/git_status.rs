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
use std::time::{Duration, Instant};

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
    /// every pane inside this work tree shares. For a linked worktree this is
    /// the worktree's own directory, not the main checkout's.
    pub root: PathBuf,
    /// The *repository* the work tree belongs to: the main checkout's root
    /// when `root` is a linked worktree, otherwise `root` itself. The
    /// sidebar's grouping key — every worktree of one repo shares it, while
    /// branch/diff state stays per work tree under `root`.
    pub home: PathBuf,
    pub branch: String,
    pub counts: Option<(u32, u32)>,
}

/// Probe the git snapshot for `cwd`, or `None` when it isn't inside a git work
/// tree (or the path is gone). Blocking — call it on a background executor.
pub fn probe(cwd: &Path) -> Option<RepoSnapshot> {
    if !cwd.exists() {
        return None;
    }
    // One `rev-parse` answers every path question at once: the work-tree root
    // (which doubles as the "is this a git repo" gate — it fails outside a
    // work tree) plus the git-dir/common-dir pair that tells a linked worktree
    // from a main checkout. Asking separately cost two process spawns per
    // probe, which mattered once probes stopped being rare: they now also fire
    // on window activation and on an agent's tool calls, across every pane.
    let paths = git(
        cwd,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
        ],
    )?;
    let mut lines = paths.lines().map(|l| l.trim_end_matches(['\n', '\r']));
    let root = PathBuf::from(lines.next()?);
    // A git old enough to reject `--path-format` fails the whole invocation
    // above, so reaching here means the two dirs are present — but degrade to
    // "main checkout" rather than trusting that, same as the old code did.
    let home = repo_home(&root, lines.next(), lines.next());
    let branch = branch_name(cwd)?;
    Some(RepoSnapshot {
        home,
        root,
        branch,
        counts: diff_numstat(cwd),
    })
}

/// The repository "home" every checkout of one repo shares, from the work-tree
/// `root` and the `--git-dir` / `--git-common-dir` pair: for a linked worktree
/// (its git dir differs from the common git dir) the main work tree's root —
/// the parent of `<main>/.git`; for the main checkout itself, a submodule, or
/// any failure to tell, the work-tree root unchanged. A bare common dir (no
/// trailing `.git` component, the bare-repo-plus-worktrees layout) anchors on
/// the bare directory itself — still one shared key.
fn repo_home(root: &Path, git_dir: Option<&str>, common_dir: Option<&str>) -> PathBuf {
    let (Some(git_dir), Some(common)) = (git_dir, common_dir) else {
        return root.to_path_buf();
    };
    if git_dir == common {
        return root.to_path_buf();
    }
    let common = Path::new(common);
    match (common.file_name(), common.parent()) {
        (Some(name), Some(parent)) if name == ".git" => parent.to_path_buf(),
        _ => common.to_path_buf(),
    }
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
    /// work-tree root → the repository home it belongs to (see
    /// [`RepoSnapshot::home`]). Identity for a plain checkout; the main
    /// root for a linked worktree, so the sidebar groups them together.
    homes: HashMap<PathBuf, PathBuf>,
    /// root → the snapshot every pane in that tree shares.
    status: HashMap<PathBuf, GitStatus>,
    /// cwds with a probe currently in flight, so concurrent triggers fold
    /// into one shell-out.
    in_flight: HashSet<PathBuf>,
    /// In-flight cwds re-triggered meanwhile — reprobed once their flight
    /// lands, so the newest trigger's state is never skipped.
    dirty: HashSet<PathBuf>,
    /// When each cwd's last probe *landed*, for the throttle that opportunistic
    /// triggers go through ([`begin_probe_throttled`](Self::begin_probe_throttled)).
    last_probe: HashMap<PathBuf, Instant>,
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

    /// What the cache *knows* about the repository `cwd` belongs to,
    /// three-valued for the sidebar's repo grouping: `None` = no probe has
    /// answered yet (the caller should keep whatever grouping it had, not
    /// reshuffle on a guess); `Some(None)` = probed and confirmed outside any
    /// work tree; `Some(Some(home))` = probed and inside the repo at `home`.
    /// `home` is the repository home, not the work-tree root — a linked
    /// worktree answers with the main checkout's root, so every worktree of
    /// one repo lands in one sidebar group.
    pub fn known_repo_for(&self, cwd: &Path) -> Option<Option<PathBuf>> {
        let root = self.roots.get(cwd)?;
        Some(root.as_ref().map(|root| {
            self.homes
                .get(root)
                .cloned()
                .unwrap_or_else(|| root.clone())
        }))
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

    /// Claim an *opportunistic* probe for `cwd`: one triggered by a cheap,
    /// frequent signal — the window regaining focus, an agent finishing a tool
    /// call — rather than by a rare edge like a command ending.
    ///
    /// Unlike [`begin_probe`](Self::begin_probe) this declines instead of
    /// queueing: a probe already in flight, or one against a repo probed less
    /// than `min_interval` ago, drops the trigger entirely (no dirty mark, no
    /// rerun). That's the whole point of the two entry points — the rare edges
    /// must never be missed, while these signals repeat on their own, so a
    /// count that's a second stale beats a `git` storm across every pane of a
    /// repo the moment the user alt-tabs back.
    ///
    /// The throttle counts per *repo*, not per cwd (see
    /// [`throttle_key`](Self::throttle_key)), and the claim stamps the clock
    /// rather than waiting for the landing: without that, a dozen panes
    /// scattered over one repo's subdirectories would all claim in the same
    /// instant — each of them passing a throttle no probe had answered yet —
    /// and produce a dozen identical full-repo diffs.
    pub fn begin_probe_throttled(&mut self, cwd: &Path, min_interval: Duration) -> bool {
        if self.in_flight.contains(cwd) {
            return false;
        }
        let key = self.throttle_key(cwd).to_path_buf();
        if self
            .last_probe
            .get(&key)
            .is_some_and(|at| at.elapsed() < min_interval)
        {
            return false;
        }
        self.last_probe.insert(key, Instant::now());
        self.in_flight.insert(cwd.to_path_buf());
        true
    }

    /// What the opportunistic throttle counts against: the work-tree root once
    /// some probe has answered for `cwd`, and `cwd` itself before that.
    ///
    /// The counts a probe produces are repo-wide — `git diff --numstat HEAD`
    /// ignores which subdirectory it ran in — so panes at `repo/`, `repo/src`
    /// and `repo/docs` are three ways of asking one question, and want one
    /// shared clock rather than one each. In-flight dedup stays keyed by cwd:
    /// it brackets a specific spawn, and [`finish_probe`](Self::finish_probe)
    /// has to be able to release exactly what was claimed.
    ///
    /// Before any probe has landed the root is simply unknown, so the first
    /// sweep over a repo still costs one probe per distinct cwd; every sweep
    /// after that collapses to one.
    fn throttle_key<'a>(&'a self, cwd: &'a Path) -> &'a Path {
        match self.roots.get(cwd) {
            Some(Some(root)) => root,
            _ => cwd,
        }
    }

    /// Fold a landed probe for `cwd` into the cache. A failed diff inside a
    /// live repo keeps the root's previous counts (a transient `git` error is
    /// not "the tree went clean"). Returns whether the cwd was re-triggered
    /// while this probe flew — the caller should start one more probe.
    pub fn finish_probe(&mut self, cwd: &Path, snapshot: Option<RepoSnapshot>) -> bool {
        self.in_flight.remove(cwd);
        // Re-stamp on landing so the gap is measured from fresh counts, and
        // under the root this probe just resolved — which is how a cwd first
        // learns to share its repo's clock (at claim time it had none).
        let key = match &snapshot {
            Some(snap) => snap.root.clone(),
            None => self.throttle_key(cwd).to_path_buf(),
        };
        self.last_probe.insert(key, Instant::now());
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
                self.homes.insert(snap.root.clone(), snap.home);
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

/// The current branch name, or a short sha for a detached HEAD. Shared with
/// [`git_diff`](crate::terminal::git_diff), which fronts its overlay with the
/// same branch label the sidebar row shows.
pub(crate) fn branch_name(cwd: &Path) -> Option<String> {
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
/// prompt; `hide_console` keeps this GUI process from flashing a console window
/// on Windows for every probe. Shared with [`git_diff`](crate::terminal::git_diff)
/// so every git read in the app goes through the same lock-free, prompt-proof
/// invocation.
pub(crate) fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    let out = crate::core::proc::hide_console(&mut cmd).output().ok()?;
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
            home: PathBuf::from(root),
            branch: branch.into(),
            counts,
        }
    }

    /// A snapshot for a linked worktree: its own root, a shared repo home.
    fn wt_snap(root: &str, home: &str, branch: &str) -> RepoSnapshot {
        RepoSnapshot {
            root: PathBuf::from(root),
            home: PathBuf::from(home),
            branch: branch.into(),
            counts: Some((0, 0)),
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

    /// The three-valued `known_repo_for` the sidebar's repo grouping reads:
    /// unprobed → `None`, probed-and-in-a-repo → `Some(Some(home))`,
    /// probed-and-not-a-repo → `Some(None)`. The three cases are what let a
    /// sticky group key hold across an in-flight cd instead of flickering.
    #[test]
    fn known_repo_for_is_three_valued() {
        let mut cache = GitStatusCache::default();
        let (repo, plain, unseen) = (
            Path::new("/repo/a"),
            Path::new("/tmp/x"),
            Path::new("/never"),
        );
        cache.finish_probe(repo, Some(snap("/repo", "main", Some((1, 0)))));
        cache.finish_probe(plain, None);

        // Inside a work tree: the resolved repo home, wrapped twice.
        assert_eq!(
            cache.known_repo_for(repo),
            Some(Some(PathBuf::from("/repo")))
        );
        // Probed and confirmed outside any repo: a definite "not a repo".
        assert_eq!(cache.known_repo_for(plain), Some(None));
        // Never probed: no answer yet — the caller keeps its sticky key.
        assert_eq!(cache.known_repo_for(unseen), None);
    }

    /// Linked worktrees of one repository share a *group* (`known_repo_for`
    /// answers the main root for both) while their *status* stays per work
    /// tree — different branches never clobber each other.
    #[test]
    fn worktrees_share_a_repo_but_not_a_status() {
        let mut cache = GitStatusCache::default();
        let (main, wt) = (Path::new("/repo"), Path::new("/repo/.wt/feat"));
        cache.finish_probe(main, Some(wt_snap("/repo", "/repo", "main")));
        cache.finish_probe(wt, Some(wt_snap("/repo/.wt/feat", "/repo", "feat/x")));

        // One sidebar group…
        assert_eq!(
            cache.known_repo_for(main),
            Some(Some(PathBuf::from("/repo")))
        );
        assert_eq!(cache.known_repo_for(wt), Some(Some(PathBuf::from("/repo"))));
        // …two independent branch lines.
        assert_eq!(cache.status_for(main).unwrap().branch, "main");
        assert_eq!(cache.status_for(wt).unwrap().branch, "feat/x");
    }

    /// The four shapes `repo_home` has to tell apart, straight from the
    /// `--git-dir` / `--git-common-dir` pair the merged `rev-parse` returns.
    #[test]
    fn repo_home_resolves_worktree_layouts() {
        let root = Path::new("/repo/.wt/feat");

        // A main checkout: the two dirs agree, so the work tree is its own home.
        assert_eq!(
            repo_home(Path::new("/repo"), Some("/repo/.git"), Some("/repo/.git")),
            PathBuf::from("/repo")
        );
        // A linked worktree: the common dir is the main checkout's `.git`, so
        // the home is that `.git`'s parent — the main work tree.
        assert_eq!(
            repo_home(root, Some("/repo/.git/worktrees/feat"), Some("/repo/.git")),
            PathBuf::from("/repo")
        );
        // A bare repo with worktrees hanging off it: no `.git` component to
        // strip, so the bare dir itself is the shared key.
        assert_eq!(
            repo_home(root, Some("/bare.git/worktrees/feat"), Some("/bare.git")),
            PathBuf::from("/bare.git")
        );
        // A git too old (or too odd) to answer both: degrade to the work tree
        // rather than guessing a grouping key.
        assert_eq!(
            repo_home(root, Some("/repo/.git"), None),
            root.to_path_buf()
        );
        assert_eq!(repo_home(root, None, None), root.to_path_buf());
    }

    /// The opportunistic path declines where the edge path queues: an in-flight
    /// probe drops the trigger (and leaves nothing dirty, so no rerun), and a
    /// probe that just landed rate-limits the next one.
    #[test]
    fn throttled_probes_decline_instead_of_queueing() {
        let mut cache = GitStatusCache::default();
        let cwd = Path::new("/repo");
        let gap = Duration::from_secs(60);

        assert!(cache.begin_probe_throttled(cwd, gap));
        // In flight: declined, and unlike `begin_probe` it doesn't mark dirty —
        // the landing reports "nothing pending" rather than asking for a rerun.
        assert!(!cache.begin_probe_throttled(cwd, gap));
        assert!(!cache.finish_probe(cwd, Some(snap("/repo", "main", Some((1, 0))))));

        // Landed just now: still inside the gap, so the next trigger is dropped.
        assert!(!cache.begin_probe_throttled(cwd, gap));
        // …but a zero gap always lets one through, and edge triggers never
        // consult the throttle at all.
        assert!(cache.begin_probe_throttled(cwd, Duration::ZERO));
        assert!(!cache.finish_probe(cwd, Some(snap("/repo", "main", Some((1, 0))))));
        assert!(cache.begin_probe(cwd));
    }

    /// The throttle is per repo, not per cwd: panes sitting in different
    /// subdirectories ask one question (the counts are repo-wide), so once the
    /// cache knows where they live, a window activation costs one probe for
    /// the repo rather than one per pane.
    #[test]
    fn throttle_collapses_subdirectories_of_one_repo() {
        let mut cache = GitStatusCache::default();
        let (top, src, docs) = (
            Path::new("/repo"),
            Path::new("/repo/src"),
            Path::new("/repo/docs"),
        );
        let gap = Duration::from_secs(60);

        // Nothing known yet, so each cwd is its own key and each gets a probe.
        for cwd in [top, src, docs] {
            assert!(cache.begin_probe_throttled(cwd, gap));
            assert!(!cache.finish_probe(cwd, Some(snap("/repo", "main", Some((3, 1))))));
        }

        // Now all three resolve to `/repo`, so the next sweep collapses: the
        // first pane to ask spends the probe and the rest ride on it.
        assert!(!cache.begin_probe_throttled(top, gap));
        assert!(!cache.begin_probe_throttled(src, gap));

        // …and the claim itself is what stops the stampede — with the clock
        // wound back far enough to let one through, the *others* still decline
        // while it is in flight, even though nothing has landed yet.
        assert!(cache.begin_probe_throttled(docs, Duration::ZERO));
        assert!(!cache.begin_probe_throttled(top, gap));
        assert!(!cache.begin_probe_throttled(src, gap));

        // A pane elsewhere is untouched by any of it.
        let other = Path::new("/other");
        assert!(cache.begin_probe_throttled(other, gap));
    }
}
