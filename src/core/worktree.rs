//! Git-worktree support for the tab context menu's "New Worktree Tab": derive
//! the repo from a pane's cwd, propose an unused two-word name (editable in the
//! sheet, see `ui::worktree_prompt`), and run `git worktree add -b` under the
//! repository's own `.tty7/worktrees/` (kept out of `git status` by an
//! auto-written self-ignoring `.tty7/.gitignore`) — so a coding agent gets an
//! isolated checkout on its own branch, physically next to the code it forks.
//! Blocking (spawns `git`); callers run it on the background executor, except
//! [`is_inside_repo`], which is a pure filesystem probe cheap enough for
//! menu-open time.

use std::path::{Path, PathBuf};

/// Word pools for generated branch names (`quiet-otter`). Short, lowercase,
/// branch-safe; two pools of 24 give 576 combinations before the numeric
/// fallback in [`defaults`] kicks in.
const ADJECTIVES: [&str; 24] = [
    "quiet", "amber", "bold", "calm", "cedar", "coral", "dusky", "early", "fable", "gold", "hazel",
    "ivory", "jade", "keen", "lunar", "mossy", "noble", "ochre", "pale", "rapid", "sunny", "tidal",
    "vivid", "wild",
];
const NOUNS: [&str; 24] = [
    "otter", "heron", "lynx", "wren", "fox", "elk", "crane", "finch", "gecko", "ibis", "koala",
    "llama", "marten", "newt", "osprey", "puffin", "quail", "raven", "seal", "tern", "urchin",
    "vole", "walrus", "yak",
];

/// A freshly created worktree: where it lives and the branch checked out in it.
#[derive(Debug)]
pub struct NewWorktree {
    pub path: PathBuf,
    pub branch: String,
}

/// What to create, as confirmed (or edited) in the sheet: the checkout's
/// directory name under the managed root, the new branch's name, and the
/// commit-ish it starts from.
#[derive(Debug, Clone)]
pub struct WorktreeRequest {
    pub name: String,
    pub branch: String,
    pub base: String,
}

/// Pre-filled values for the sheet: an unused two-word candidate (offered as
/// both directory name and branch), the branch currently checked out (the
/// natural start point; `"HEAD"` when detached), and the directory the new
/// checkout would land in, for the live path preview.
#[derive(Debug, Clone)]
pub struct WorktreeDefaults {
    pub name: String,
    pub base: String,
    pub dir: PathBuf,
}

/// Whether `cwd` sits inside a git repository — an upward scan for `.git`
/// (a directory in a primary checkout, a file in a linked worktree or
/// submodule). No subprocess: the tab context menu calls this while opening.
pub fn is_inside_repo(cwd: &Path) -> bool {
    cwd.ancestors().any(|d| d.join(".git").exists())
}

/// Run `git -C <dir> <args>`, returning trimmed stdout on success and trimmed
/// stderr as the error otherwise.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Whether `name` already exists as a local branch in the repo at `repo_root`.
/// A failed probe (`--verify --quiet` exits non-zero) means it's free.
fn branch_exists(repo_root: &Path, name: &str) -> bool {
    git(
        repo_root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ],
    )
    .is_ok()
}

/// A tiny xorshift over a time+pid seed — enough randomness to spread branch
/// names without pulling in a `rand` dependency.
fn seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    nanos ^ ((std::process::id() as u64) << 32) | 1
}

fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// One `adjective-noun` candidate from the pools.
fn candidate(state: &mut u64) -> String {
    let a = ADJECTIVES[(next(state) % ADJECTIVES.len() as u64) as usize];
    let n = NOUNS[(next(state) % NOUNS.len() as u64) as usize];
    format!("{a}-{n}")
}

/// A tty7-managed worktree a closing tab sat in, resolved for the
/// close-time cleanup offer: where it is, its branch, the repository it
/// belongs to, and whether it holds uncommitted changes.
#[derive(Debug, Clone)]
pub struct ManagedWorktree {
    pub path: PathBuf,
    pub branch: String,
    pub main_root: PathBuf,
    pub dirty: bool,
}

/// Resolve `cwd` to the tty7-managed worktree containing it, or `None` when it
/// sits anywhere else. Only checkouts under the main repository's
/// `.tty7/worktrees/` count — a user's own linked worktrees are never offered
/// for removal. Blocking (spawns `git`).
pub fn managed(cwd: &Path) -> Option<ManagedWorktree> {
    // Canonicalize before the component test: git reports resolved physical
    // paths (`/private/var/…` on macOS), while `cwd` may arrive through
    // symlinks — a textual comparison would then never match. The `.tty7/
    // worktrees` ancestor check is a cheap pure-filesystem pre-filter, so the
    // common case (every ordinary tab close) never spawns git.
    let cwd = std::fs::canonicalize(cwd).ok()?;
    if !cwd
        .ancestors()
        .any(|a| a.ends_with(Path::new(".tty7").join("worktrees")))
    {
        return None;
    }
    let path = PathBuf::from(git(&cwd, &["rev-parse", "--show-toplevel"]).ok()?);
    let main_root = git(
        &path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()
    .map(PathBuf::from)?
    .parent()?
    .to_path_buf();
    // The checkout must really sit in *this* repository's managed directory —
    // both paths come from git, so they compare on equal (physical) footing.
    if !path.starts_with(main_root.join(".tty7").join("worktrees")) {
        return None;
    }
    let branch = git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]).ok()?;
    let dirty = !git(&path, &["status", "--porcelain"]).ok()?.is_empty();
    Some(ManagedWorktree {
        path,
        branch,
        main_root,
        dirty,
    })
}

/// Remove a managed worktree (`git worktree remove`, `--force` to discard
/// uncommitted changes), then best-effort delete its branch with `-d` — so a
/// branch carrying unmerged commits survives the cleanup.
pub fn remove(wt: &ManagedWorktree, force: bool) -> Result<(), String> {
    let path = wt.path.to_str().ok_or("worktree path is not valid UTF-8")?;
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(path);
    git(&wt.main_root, &args)?;
    let _ = git(&wt.main_root, &["branch", "-d", &wt.branch]);
    Ok(())
}

/// Locate the repository containing `cwd` and the directory its managed
/// worktrees live in: `(repo_root, <main-root>/.tty7/worktrees)`. Anchored on
/// the *main* repository even when `cwd` is itself inside a linked worktree
/// (a worktree tab spawning another worktree), so checkouts never nest. The
/// common git-dir is `<main>/.git`, whose parent is the main root.
fn repo_dir(cwd: &Path) -> Result<(PathBuf, PathBuf), String> {
    let repo_root = git(cwd, &["rev-parse", "--show-toplevel"])
        .map_err(|_| "not inside a git repository".to_string())?;
    let repo_root = PathBuf::from(repo_root);
    let main_root = git(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()
    .map(PathBuf::from)
    .and_then(|d| d.parent().map(Path::to_path_buf))
    .unwrap_or_else(|| repo_root.clone());
    let dir = main_root.join(".tty7").join("worktrees");
    Ok((repo_root, dir))
}

/// Compute the sheet's pre-filled values: a generated `adjective-noun` name
/// (retried until both the branch and the directory are unused, with a
/// numeric-suffix fallback so a saturated pool still terminates) and the
/// currently checked-out branch as the start point.
pub fn defaults(cwd: &Path) -> Result<WorktreeDefaults, String> {
    let (repo_root, dir) = repo_dir(cwd)?;

    let mut state = seed();
    let mut name = candidate(&mut state);
    for attempt in 0..64 {
        // Both the ref and the directory must be free — a stale directory from a
        // hand-removed worktree would make `git worktree add` fail either way.
        if !branch_exists(&repo_root, &name) && !dir.join(&name).exists() {
            break;
        }
        name = if attempt < 32 {
            candidate(&mut state)
        } else {
            format!("{}-{}", candidate(&mut state), next(&mut state) % 1000)
        };
    }

    // Detached HEAD (or an unborn branch) has no abbrev-ref; start from HEAD.
    let base = git(&repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "HEAD".to_string());
    Ok(WorktreeDefaults { name, base, dir })
}

/// Create the requested worktree for the repository containing `cwd`, at
/// `<main-root>/.tty7/worktrees/<name>`, on new branch `branch` starting from
/// `base`. Branch and base validity is git's to judge; the directory name only
/// has to stay a single path component so it can't escape the managed root.
pub fn create(cwd: &Path, req: &WorktreeRequest) -> Result<NewWorktree, String> {
    if req.name.is_empty() || req.name == "." || req.name == ".." || req.name.contains(['/', '\\'])
    {
        return Err(format!("invalid worktree name \"{}\"", req.name));
    }
    let (repo_root, dir) = repo_dir(cwd)?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    // A `*` gitignore inside `.tty7/` keeps the whole tree (checkouts included,
    // the ignore file itself too) out of the repository's `git status`, without
    // ever editing the repo's own .gitignore. Best-effort: a failed write only
    // costs status noise, never the worktree.
    let ignore = dir
        .parent()
        .expect(".tty7/worktrees has a parent")
        .join(".gitignore");
    if !ignore.exists() {
        let _ = std::fs::write(&ignore, "*\n");
    }

    let path = dir.join(&req.name);
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    git(
        &repo_root,
        &[
            "worktree",
            "add",
            "-b",
            &req.branch,
            path.to_str().ok_or("worktree path is not valid UTF-8")?,
            &req.base,
        ],
    )?;
    Ok(NewWorktree {
        path,
        branch: req.branch.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh scratch dir under the system temp location, unique per test —
    /// the same std-only pattern the config tests use (no tempfile dep).
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tty7-wt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sh(dir: &Path, args: &[&str]) {
        assert!(
            std::process::Command::new(args[0])
                .args(&args[1..])
                .current_dir(dir)
                .output()
                .unwrap()
                .status
                .success(),
            "command failed: {args:?}"
        );
    }

    /// A throwaway repo with one commit, so `worktree add` has a HEAD to branch
    /// from.
    fn temp_repo(name: &str) -> PathBuf {
        let dir = scratch(name);
        sh(&dir, &["git", "init", "-q"]);
        sh(&dir, &["git", "config", "user.email", "t@t"]);
        sh(&dir, &["git", "config", "user.name", "t"]);
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        sh(&dir, &["git", "add", "."]);
        sh(&dir, &["git", "commit", "-q", "-m", "init"]);
        dir
    }

    /// The simplest sensible request: directory and branch share `name`,
    /// starting from HEAD — what the sheet submits when nothing is edited.
    fn req(name: &str) -> WorktreeRequest {
        WorktreeRequest {
            name: name.into(),
            branch: name.into(),
            base: "HEAD".into(),
        }
    }

    #[test]
    fn candidate_is_two_pool_words() {
        let mut state = seed();
        let name = candidate(&mut state);
        let (a, n) = name.split_once('-').unwrap();
        assert!(ADJECTIVES.contains(&a));
        assert!(NOUNS.contains(&n));
    }

    #[test]
    fn is_inside_repo_scans_upward_for_dot_git() {
        let repo = temp_repo("probe");
        let sub = repo.join("deep/nested");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(is_inside_repo(&sub));
        let plain = scratch("probe-plain");
        assert!(!is_inside_repo(&plain));
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&plain);
    }

    #[test]
    fn defaults_proposes_fresh_name_current_branch_and_target_dir() {
        let repo = temp_repo("dflt");
        let d = defaults(&repo).unwrap();
        assert!(!branch_exists(&repo, &d.name));
        let head = git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        assert_eq!(d.base, head);
        // The target dir is the repo's own `.tty7/worktrees` (git reports the
        // canonical root: /var → /private/var on macOS).
        let canon = std::fs::canonicalize(&repo).unwrap();
        assert_eq!(d.dir, canon.join(".tty7").join("worktrees"));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn create_makes_worktree_on_new_branch_inside_the_repo() {
        let repo = temp_repo("repo");
        let wt = create(&repo, &req("quiet-otter")).unwrap();
        assert!(wt.path.join("a.txt").exists());
        assert!(branch_exists(&repo, &wt.branch));
        // The worktree lands under `<repo>/.tty7/worktrees/<name>`…
        let canon = std::fs::canonicalize(&repo).unwrap();
        assert_eq!(wt.path, canon.join(".tty7/worktrees/quiet-otter"));
        // …on the new branch…
        let head = git(&wt.path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        assert_eq!(head, wt.branch);
        // …and the auto-written `.tty7/.gitignore` keeps the main repo's
        // status clean despite the checkout living inside it.
        assert_eq!(
            std::fs::read_to_string(canon.join(".tty7/.gitignore")).unwrap(),
            "*\n"
        );
        assert_eq!(git(&repo, &["status", "--porcelain"]).unwrap(), "");
        // A second request colliding on the directory is refused up front.
        assert!(
            create(&repo, &req("quiet-otter"))
                .unwrap_err()
                .contains("already exists")
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn create_honors_custom_branch_and_base() {
        let repo = temp_repo("base");
        // A `stable` branch one commit behind the default branch's HEAD.
        sh(&repo, &["git", "branch", "stable"]);
        std::fs::write(repo.join("b.txt"), "b").unwrap();
        sh(&repo, &["git", "add", "."]);
        sh(&repo, &["git", "commit", "-q", "-m", "second"]);
        let wt = create(
            &repo,
            &WorktreeRequest {
                name: "my-dir".into(),
                branch: "feat/my-branch".into(),
                base: "stable".into(),
            },
        )
        .unwrap();
        // Directory and branch names diverge as requested…
        assert_eq!(wt.path.file_name().unwrap().to_str().unwrap(), "my-dir");
        let head = git(&wt.path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        assert_eq!(head, "feat/my-branch");
        // …and the checkout starts from `stable` (no b.txt yet).
        assert!(wt.path.join("a.txt").exists());
        assert!(!wt.path.join("b.txt").exists());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn create_rejects_escaping_names() {
        let repo = temp_repo("names");
        for bad in ["", ".", "..", "a/b", "a\\b"] {
            let mut r = req("x");
            r.name = bad.into();
            assert!(
                create(&repo, &r)
                    .unwrap_err()
                    .contains("invalid worktree name"),
                "{bad:?} should be rejected"
            );
        }
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn create_from_a_linked_worktree_lands_in_the_main_repo() {
        let repo = temp_repo("nest");
        let first = create(&repo, &req("first-wt")).unwrap();
        // Spawn the second worktree from *inside* the first: it must land in
        // the main repo's `.tty7/worktrees`, not nest inside the first checkout.
        let second = create(&first.path, &req("second-wt")).unwrap();
        assert_eq!(second.path.parent().unwrap(), first.path.parent().unwrap());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn managed_resolves_managed_checkouts_and_remove_cleans_up() {
        let repo = temp_repo("mg");
        let wt = create(&repo, &req("mg-wt")).unwrap();
        // The repo root itself is never "managed"…
        assert!(managed(&repo).is_none());
        // …nor is a linked worktree the user made outside `.tty7/worktrees`.
        let own = scratch("mg-own");
        let _ = std::fs::remove_dir_all(&own);
        sh(
            &repo,
            &[
                "git",
                "worktree",
                "add",
                "-b",
                "own-branch",
                own.to_str().unwrap(),
            ],
        );
        assert!(managed(&own).is_none());
        // Any path inside the managed checkout resolves to it, initially clean.
        let sub = wt.path.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let m = managed(&sub).unwrap();
        assert_eq!(m.branch, wt.branch);
        assert_eq!(m.path, wt.path);
        assert!(!m.dirty);
        // Uncommitted changes flip `dirty` and block a plain remove; --force
        // discards them. The branch (no unique commits) is deleted with it.
        std::fs::write(wt.path.join("b.txt"), "b").unwrap();
        let m = managed(&wt.path).unwrap();
        assert!(m.dirty);
        assert!(remove(&m, false).is_err());
        remove(&m, true).unwrap();
        assert!(!wt.path.exists());
        assert!(!branch_exists(&repo, &wt.branch));
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&own);
    }

    #[test]
    fn create_outside_a_repo_errors() {
        let plain = scratch("plain");
        let err = create(&plain, &req("x")).unwrap_err();
        assert_eq!(err, "not inside a git repository");
        assert_eq!(defaults(&plain).unwrap_err(), "not inside a git repository");
        let _ = std::fs::remove_dir_all(&plain);
    }
}
