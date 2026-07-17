//! Local project file tree (the code overlay's left column — see
//! `code_editor::render_code_overlay` for the panel that hosts it).
//!
//! Modelled on Warp's Project Explorer: lazily-loaded directories, gitignore
//! awareness (ignored entries render dimmed, not hidden), a filesystem watcher
//! that keeps listings fresh, keyboard navigation, inline new-file / rename
//! editing, and a per-row context menu (open / cd / reveal / copy path /
//! delete / attach to a coding agent). Roots come from the active tab's panes:
//! each pane's cwd resolves to its repository root (walk up to `.git`), so a
//! tab whose panes sit in two repos shows both as top-level roots.
//!
//! The panel is a plain lazy tree over `std::fs` — no daemon round-trips (the
//! SFTP panel covers the remote case). Listings are cached per directory and
//! invalidated by `notify` events, so a huge repo only ever pays for the
//! directories actually expanded.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, Entity, ExternalPaths, FocusHandle, KeyDownEvent, MouseButton,
    MouseMoveEvent, MouseUpEvent, PromptLevel, SharedString, Subscription, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use ignore::gitignore::Gitignore;

use crate::ui::app::Tty7App;

/// Width band (px) for the tree column.
const MIN_WIDTH: f32 = 160.0;
const MAX_WIDTH: f32 = 480.0;
const DEFAULT_WIDTH: f32 = 240.0;

/// Per-level indent (px) for nested rows.
const INDENT: f32 = 14.0;

/// Debounce for watcher-driven refreshes (same rationale as the config
/// hot-reload: coalesce a save burst into one reload).
const REFRESH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

/// One directory entry in a cached listing.
#[derive(Clone)]
pub(crate) struct TreeEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    /// Matched by the gitignore chain (or is `.git` itself): rendered dimmed.
    pub ignored: bool,
}

/// A flattened visible row: what the list renders and what keyboard
/// navigation walks. Roots are rows too (depth 0, always expanded-looking).
pub(crate) struct TreeRow {
    pub entry: TreeEntry,
    pub depth: usize,
    pub is_root: bool,
    pub expanded: bool,
}

/// One in-progress inline edit (new file / new folder / rename).
pub(crate) enum TreeEdit {
    NewFile {
        dir: PathBuf,
        input: Entity<InputState>,
    },
    NewFolder {
        dir: PathBuf,
        input: Entity<InputState>,
    },
    Rename {
        path: PathBuf,
        input: Entity<InputState>,
    },
}

impl TreeEdit {
    fn input(&self) -> &Entity<InputState> {
        match self {
            TreeEdit::NewFile { input, .. }
            | TreeEdit::NewFolder { input, .. }
            | TreeEdit::Rename { input, .. } => input,
        }
    }

    /// The directory whose listing hosts the edit row.
    fn host_dir(&self) -> &Path {
        match self {
            TreeEdit::NewFile { dir, .. } | TreeEdit::NewFolder { dir, .. } => dir,
            TreeEdit::Rename { path, .. } => path.parent().unwrap_or(path),
        }
    }
}

/// App-global file-tree infrastructure, held on [`Tty7App`]. The per-tab view
/// state (roots, expansion, selection) lives in
/// [`TabCode`](crate::ui::code_editor::TabCode); everything here is path-keyed
/// cache or chrome shared by every tab's panel — one panel shows at a time.
pub(crate) struct FileTreeState {
    /// Lazily-loaded listing per directory; invalidated by watcher events.
    children: HashMap<PathBuf, Vec<TreeEntry>>,
    /// Compiled `.gitignore` per directory (`None` = the dir has none).
    /// Invalidated when a `.gitignore` changes.
    gitignore: HashMap<PathBuf, Option<Rc<Gitignore>>>,
    pub(crate) show_hidden: bool,
    pub(crate) width: Rc<Cell<f32>>,
    dragging: Rc<Cell<bool>>,
    pub(crate) editing: Option<TreeEdit>,
    editing_subs: Vec<Subscription>,
    /// One recursive watcher over the union of every tab's roots; rebuilt
    /// when any root set changes. Events invalidate the path-keyed caches
    /// above, which are tab-agnostic.
    watcher: Option<notify::RecommendedWatcher>,
    events_tx: smol::channel::Sender<PathBuf>,
    pub(crate) focus_handle: FocusHandle,
}

impl FileTreeState {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Tty7App>) -> Self {
        let (tx, rx) = smol::channel::unbounded::<PathBuf>();
        cx.spawn_in(window, async move |app, cx| {
            while let Ok(first) = rx.recv().await {
                cx.background_executor().timer(REFRESH_DEBOUNCE).await;
                let mut changed: HashSet<PathBuf> = HashSet::from([first]);
                while let Ok(more) = rx.try_recv() {
                    changed.insert(more);
                }
                let ok = app.update(cx, |app, cx| {
                    app.file_tree_apply_fs_events(&changed, cx);
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();
        Self {
            children: HashMap::new(),
            gitignore: HashMap::new(),
            show_hidden: false,
            width: Rc::new(Cell::new(DEFAULT_WIDTH)),
            dragging: Rc::new(Cell::new(false)),
            editing: None,
            editing_subs: Vec::new(),
            watcher: None,
            events_tx: tx,
            focus_handle: cx.focus_handle(),
        }
    }

    /// (Re)attach the recursive watcher to `roots` (the union across tabs).
    fn rebuild_watcher(&mut self, roots: &HashSet<PathBuf>) {
        use notify::{RecursiveMode, Watcher};
        self.watcher = None;
        if roots.is_empty() {
            return;
        }
        let tx = self.events_tx.clone();
        let handler = move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            for p in event.paths {
                let _ = tx.try_send(p);
            }
        };
        let mut watcher = match notify::recommended_watcher(handler) {
            Ok(w) => w,
            Err(e) => {
                log::warn!("file tree: watcher unavailable: {e}");
                return;
            }
        };
        for root in roots {
            if let Err(e) = watcher.watch(root, RecursiveMode::Recursive) {
                log::warn!("file tree: failed to watch {}: {e}", root.display());
            }
        }
        self.watcher = Some(watcher);
    }

    /// Load any expanded directory whose listing isn't cached yet. Called once
    /// per render pass so `visible_rows` can stay `&self`.
    fn ensure_loaded(&mut self, roots: &[PathBuf], expanded: &HashSet<PathBuf>) {
        // Roots always list; expanded dirs list on demand. Collect first: the
        // borrow checker won't let us mutate `children` while iterating it.
        let mut todo: Vec<(PathBuf, PathBuf)> = Vec::new(); // (dir, its root)
        for root in roots {
            if !self.children.contains_key(root) {
                todo.push((root.clone(), root.clone()));
            }
            for dir in expanded {
                if dir.starts_with(root) && !self.children.contains_key(dir) {
                    todo.push((dir.clone(), root.clone()));
                }
            }
        }
        for (dir, root) in todo {
            let listing = self.list_dir(&dir, &root);
            self.children.insert(dir, listing);
        }
    }

    /// Read one directory into sorted entries, tagging gitignored ones.
    fn list_dir(&mut self, dir: &Path, root: &Path) -> Vec<TreeEntry> {
        let Ok(read) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut out: Vec<TreeEntry> = Vec::new();
        for e in read.flatten() {
            let path = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let ignored = name == ".git" || self.is_gitignored(&path, is_dir, root);
            out.push(TreeEntry {
                name,
                path,
                is_dir,
                ignored,
            });
        }
        sort_entries(&mut out);
        out
    }

    /// Walk the `.gitignore` chain from `root` down to the entry's directory;
    /// the deepest match wins (whitelist `!patterns` un-ignore).
    fn is_gitignored(&mut self, path: &Path, is_dir: bool, root: &Path) -> bool {
        let Some(parent) = path.parent() else {
            return false;
        };
        let mut state = false;
        // Ancestor chain root → parent, in order.
        let mut chain: Vec<&Path> = parent
            .ancestors()
            .take_while(|a| a.starts_with(root))
            .collect();
        chain.reverse();
        for dir in chain {
            let gi = self
                .gitignore
                .entry(dir.to_path_buf())
                .or_insert_with(|| {
                    let file = dir.join(".gitignore");
                    file.is_file().then(|| {
                        let (gi, _err) = Gitignore::new(&file);
                        Rc::new(gi)
                    })
                })
                .clone();
            let Some(gi) = gi else { continue };
            let Ok(rel) = path.strip_prefix(dir) else {
                continue;
            };
            match gi.matched(rel, is_dir) {
                ignore::Match::Ignore(_) => state = true,
                ignore::Match::Whitelist(_) => state = false,
                ignore::Match::None => {}
            }
        }
        state
    }

    /// Flatten `roots` + `expanded` directories into display order (both come
    /// from the active tab's panel state).
    pub(crate) fn visible_rows(&self, roots: &[PathBuf], expanded: &HashSet<PathBuf>) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        for root in roots {
            let name = root
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| root.display().to_string());
            rows.push(TreeRow {
                entry: TreeEntry {
                    name,
                    path: root.clone(),
                    is_dir: true,
                    ignored: false,
                },
                depth: 0,
                is_root: true,
                expanded: true,
            });
            self.flatten_dir(root, 1, expanded, &mut rows);
        }
        rows
    }

    fn flatten_dir(
        &self,
        dir: &Path,
        depth: usize,
        expanded: &HashSet<PathBuf>,
        out: &mut Vec<TreeRow>,
    ) {
        let Some(entries) = self.children.get(dir) else {
            return;
        };
        for e in entries {
            if !self.show_hidden && e.name.starts_with('.') {
                continue;
            }
            let is_expanded = e.is_dir && expanded.contains(&e.path);
            out.push(TreeRow {
                entry: e.clone(),
                depth,
                is_root: false,
                expanded: is_expanded,
            });
            if is_expanded {
                self.flatten_dir(&e.path, depth + 1, expanded, out);
            }
        }
    }

    /// Drop cached listings after a filesystem change under `dir`.
    fn invalidate_dir(&mut self, dir: &Path) {
        self.children.remove(dir);
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (tested).
// ---------------------------------------------------------------------------

/// Directories first, then case-insensitive by name (dotfiles keep their
/// leading-dot position in that ordering — they sort before letters).
pub(crate) fn sort_entries(entries: &mut [TreeEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// The repository root for a path: the nearest ancestor containing `.git`
/// (dir or worktree file), or `None` outside any repo.
pub(crate) fn repo_root_for(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|p| p.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Single-quote a path for the shell; embedded `'` becomes `'\''`.
pub(crate) fn shell_quote(path: &Path) -> String {
    let s = path.to_string_lossy();
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || "/.-_~+".contains(c))
    {
        return s.into_owned();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

// ---------------------------------------------------------------------------
// Tty7App: toggling, fs events, row operations.
// ---------------------------------------------------------------------------

impl Tty7App {
    /// Derive the root set from the active tab's panes: each pane cwd maps to
    /// its repo root (or itself outside a repo); home as the last resort.
    pub(crate) fn file_tree_refresh_roots(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Some(tab) = self.tabs.get(self.active) {
            for leaf in tab.pane.leaves() {
                let Some(cwd) = leaf.read(cx).cwd() else {
                    continue;
                };
                let root = repo_root_for(&cwd).unwrap_or(cwd);
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }
        }
        if roots.is_empty()
            && let Some(home) = std::env::var_os("HOME")
        {
            roots.push(PathBuf::from(home));
        }
        let _ = window;
        let Some(code) = self.tab_code_mut() else {
            return;
        };
        if roots != code.roots {
            code.roots = roots;
        }
        // Refresh listings but keep expansion state; the caches are shared
        // (path-keyed), so a stale entry only costs a relist.
        self.file_tree.children.clear();
        self.file_tree.gitignore.clear();
        // One watcher over every tab's roots.
        let union: HashSet<PathBuf> = self
            .tabs
            .iter()
            .filter_map(|t| t.code.as_deref())
            .flat_map(|c| c.roots.iter().cloned())
            .collect();
        self.file_tree.rebuild_watcher(&union);
        cx.notify();
    }

    /// Watcher callback (debounced): drop affected listing caches so the next
    /// render relists. A `.gitignore` change resets ignore state wholesale —
    /// its patterns can affect any depth below it.
    pub(crate) fn file_tree_apply_fs_events(
        &mut self,
        paths: &HashSet<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        // The caches are shared across tabs, so invalidate unconditionally —
        // a hidden tab's stale listing would otherwise survive until reopened.
        let gitignore_touched = paths
            .iter()
            .any(|p| p.file_name().is_some_and(|n| n == ".gitignore"));
        if gitignore_touched {
            self.file_tree.gitignore.clear();
            self.file_tree.children.clear();
        } else {
            for p in paths {
                if let Some(parent) = p.parent() {
                    self.file_tree.invalidate_dir(parent);
                }
                // A changed dir itself (e.g. a mkdir) also invalidates its own
                // listing if cached.
                self.file_tree.invalidate_dir(p);
            }
        }
        cx.notify();
    }

    fn file_tree_toggle_expand(&mut self, dir: &Path, cx: &mut Context<Self>) {
        let Some(code) = self.tab_code_mut() else {
            return;
        };
        if !code.expanded.remove(dir) {
            code.expanded.insert(dir.to_path_buf());
        }
        cx.notify();
    }

    /// Row activation (click / Enter): directories toggle, files open in the
    /// editor panel.
    fn file_tree_activate(
        &mut self,
        row_path: &Path,
        is_dir: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(code) = self.tab_code_mut() {
            code.selected = Some(row_path.to_path_buf());
        }
        if is_dir {
            self.file_tree_toggle_expand(row_path, cx);
        } else {
            self.open_file_in_editor(row_path, window, cx);
        }
        cx.notify();
    }

    /// Keyboard navigation over the flattened rows.
    fn file_tree_key_down(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(code) = self.tab_code() else {
            return;
        };
        let rows = self
            .file_tree
            .visible_rows(&code.roots, &code.expanded);
        if rows.is_empty() {
            return;
        }
        let sel_ix = code
            .selected
            .as_ref()
            .and_then(|s| rows.iter().position(|r| r.entry.path == *s));
        let key = ev.keystroke.key.as_str();
        match key {
            "up" | "down" => {
                let next = match (sel_ix, key) {
                    (None, _) => 0,
                    (Some(i), "up") => i.saturating_sub(1),
                    (Some(i), _) => (i + 1).min(rows.len() - 1),
                };
                let path = rows[next].entry.path.clone();
                if let Some(code) = self.tab_code_mut() {
                    code.selected = Some(path);
                }
                cx.notify();
            }
            "left" => {
                let Some(i) = sel_ix else { return };
                let row = &rows[i];
                let (path, is_dir, expanded, is_root) = (
                    row.entry.path.clone(),
                    row.entry.is_dir,
                    row.expanded,
                    row.is_root,
                );
                let parent_in_rows = path
                    .parent()
                    .is_some_and(|p| rows.iter().any(|r| r.entry.path == p));
                if let Some(code) = self.tab_code_mut() {
                    if is_dir && expanded && !is_root {
                        code.expanded.remove(&path);
                    } else if parent_in_rows
                        && let Some(parent) = path.parent()
                    {
                        // Jump to the parent row (stay put at a root).
                        code.selected = Some(parent.to_path_buf());
                    }
                }
                cx.notify();
            }
            "right" => {
                let Some(i) = sel_ix else { return };
                let row = &rows[i];
                if row.entry.is_dir && !row.expanded && !row.is_root {
                    let path = row.entry.path.clone();
                    if let Some(code) = self.tab_code_mut() {
                        code.expanded.insert(path);
                    }
                    cx.notify();
                }
            }
            "enter" => {
                let Some(i) = sel_ix else { return };
                let (path, is_dir) = (rows[i].entry.path.clone(), rows[i].entry.is_dir);
                self.file_tree_activate(&path, is_dir, window, cx);
            }
            _ => {}
        }
    }

    // ----- Inline edits (new file / new folder / rename) --------------------

    fn file_tree_begin_edit(
        &mut self,
        edit_for: TreeEditKind,
        target: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let initial = match edit_for {
            TreeEditKind::Rename => target
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            _ => String::new(),
        };
        let input = cx.new(|cx| {
            let mut st = InputState::new(window, cx).placeholder(match edit_for {
                TreeEditKind::NewFile => "file name",
                TreeEditKind::NewFolder => "folder name",
                TreeEditKind::Rename => "new name",
            });
            st.set_value(initial, window, cx);
            st
        });
        input.update(cx, |st, cx| st.focus(window, cx));
        let sub = cx.subscribe_in(
            &input,
            window,
            |this: &mut Tty7App, _input, ev, window, cx| match ev {
                InputEvent::PressEnter { .. } => this.file_tree_commit_edit(window, cx),
                InputEvent::Blur => this.file_tree_cancel_edit(cx),
                _ => {}
            },
        );
        self.file_tree.editing_subs = vec![sub];
        // New entries land in the target dir (or the file's parent), which
        // must be expanded for the inline input row to show.
        let host_dir = if target.is_dir() {
            target.to_path_buf()
        } else {
            target.parent().unwrap_or(target).to_path_buf()
        };
        if !matches!(edit_for, TreeEditKind::Rename)
            && let Some(code) = self.tab_code_mut()
        {
            code.expanded.insert(host_dir.clone());
        }
        self.file_tree.editing = Some(match edit_for {
            TreeEditKind::NewFile => TreeEdit::NewFile {
                dir: host_dir,
                input,
            },
            TreeEditKind::NewFolder => TreeEdit::NewFolder {
                dir: host_dir,
                input,
            },
            TreeEditKind::Rename => TreeEdit::Rename {
                path: target.to_path_buf(),
                input,
            },
        });
        cx.notify();
    }

    fn file_tree_cancel_edit(&mut self, cx: &mut Context<Self>) {
        self.file_tree.editing = None;
        self.file_tree.editing_subs.clear();
        cx.notify();
    }

    fn file_tree_commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.file_tree.editing.take() else {
            return;
        };
        self.file_tree.editing_subs.clear();
        let name = edit.input().read(cx).value().trim().to_string();
        if name.is_empty() || name.contains('/') {
            cx.notify();
            return;
        }
        let result: std::io::Result<PathBuf> = match &edit {
            TreeEdit::NewFile { dir, .. } => {
                let path = dir.join(&name);
                std::fs::File::create_new(&path).map(|_| path)
            }
            TreeEdit::NewFolder { dir, .. } => {
                let path = dir.join(&name);
                std::fs::create_dir(&path).map(|_| path)
            }
            TreeEdit::Rename { path, .. } => {
                let to = path.with_file_name(&name);
                if to.exists() {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "target exists",
                    ))
                } else {
                    std::fs::rename(path, &to).map(|_| to)
                }
            }
        };
        match result {
            Ok(new_path) => {
                self.file_tree.invalidate_dir(edit.host_dir());
                if let Some(code) = self.tab_code_mut() {
                    code.selected = Some(new_path.clone());
                }
                // A freshly created file opens straight into the editor.
                if matches!(edit, TreeEdit::NewFile { .. }) {
                    self.open_file_in_editor(&new_path, window, cx);
                }
            }
            Err(e) => {
                use gpui_component::WindowExt as _;
                window.push_notification(format!("{e}"), cx);
            }
        }
        cx.notify();
    }

    /// Context-menu delete, with a native confirm (recursive for dirs).
    fn file_tree_delete(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        let is_dir = path.is_dir();
        let detail = if is_dir {
            "The folder and everything inside it will be deleted."
        } else {
            "The file will be deleted."
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete \"{name}\"?"),
            Some(detail),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |app, cx| {
            let Ok(0) = answer.await else { return };
            let _ = app.update_in(cx, |app, window, cx| {
                let result = if is_dir {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                match result {
                    Ok(()) => {
                        if let Some(parent) = path.parent() {
                            app.file_tree.invalidate_dir(parent);
                        }
                        if let Some(code) = app.tab_code_mut()
                            && code.selected.as_deref() == Some(&path)
                        {
                            code.selected = None;
                        }
                        cx.notify();
                    }
                    Err(e) => {
                        use gpui_component::WindowExt as _;
                        window.push_notification(format!("Delete failed: {e}"), cx);
                    }
                }
            });
        })
        .detach();
    }

    /// "cd here": type `cd <dir>` + Enter into the focused pane's PTY.
    fn file_tree_cd(&mut self, dir: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some(leaf) = self
            .tabs
            .get(self.active)
            .and_then(|t| t.pane.focused_or_first(window, cx))
        else {
            return;
        };
        leaf.read(cx)
            .run_command_line(&format!("cd {}", shell_quote(dir)));
        self.focus_active(window, cx);
    }

    /// "Attach to agent": paste an `@path` reference into the pane running a
    /// coding agent (unsubmitted, so the user can keep typing the prompt).
    fn file_tree_attach_to_agent(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(target) = self.agent_target_leaf(cx) else {
            crate::terminal::notify_desktop(
                Some("tty7"),
                "No running coding agent found — start one (claude, codex, …) in a pane first.",
            );
            return;
        };
        // Prefer a repo-relative path (what agents resolve best) when the file
        // sits under one of the tree's roots.
        let rel = self
            .tab_code()
            .into_iter()
            .flat_map(|c| c.roots.iter())
            .find_map(|r| path.strip_prefix(r).ok())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| path.to_path_buf());
        target.update(cx, |view, cx| {
            view.paste(format!("@{} ", rel.display()), cx);
        });
    }
}

/// Which inline edit a context-menu entry starts.
#[derive(Clone, Copy)]
enum TreeEditKind {
    NewFile,
    NewFolder,
    Rename,
}

// ---------------------------------------------------------------------------
// Rendering.
// ---------------------------------------------------------------------------

impl Tty7App {
    /// The file-tree column: the code overlay's left side (the overlay
    /// renders the divider and the editor to its right).
    pub(crate) fn render_file_tree_column(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (roots, expanded) = match self.tab_code() {
            Some(code) => (code.roots.clone(), code.expanded.clone()),
            None => (Vec::new(), std::collections::HashSet::new()),
        };
        self.file_tree.ensure_loaded(&roots, &expanded);
        let width = self.file_tree.width.get().clamp(MIN_WIDTH, MAX_WIDTH);
        let rows = self.file_tree.visible_rows(&roots, &expanded);

        let list = v_flex()
            .id("file-tree-rows")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_1()
            .py_1()
            .children(
                rows.iter()
                    .flat_map(|row| self.render_tree_row(row, window, cx)),
            );

        v_flex()
            .id("file-tree-panel")
            .flex_none()
            .h_full()
            .w(px(width))
            .bg(cx.theme().background)
            .track_focus(&self.file_tree.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                this.file_tree_key_down(ev, window, cx);
            }))
            .child(self.render_tree_header(cx))
            .child(list)
            .into_any_element()
    }

    /// Panel header: title + refresh / new-file / hidden-files toggle.
    fn render_tree_header(&self, cx: &mut Context<Self>) -> gpui::Div {
        let show_hidden = self.file_tree.show_hidden;
        h_flex()
            .flex_none()
            .items_center()
            .gap_0p5()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .flex_1()
                    .text_sm()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child("Files"),
            )
            .child(
                Button::new("tree-refresh")
                    .icon(IconName::LoaderCircle)
                    .ghost()
                    .xsmall()
                    .tooltip("Refresh")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.file_tree_refresh_roots(window, cx);
                    })),
            )
            .child(
                Button::new("tree-toggle-hidden")
                    .icon(IconName::Eye)
                    .ghost()
                    .xsmall()
                    .tooltip(if show_hidden {
                        "Hide dotfiles"
                    } else {
                        "Show dotfiles"
                    })
                    .on_click(cx.listener(|this, _, _w, cx| {
                        this.file_tree.show_hidden = !this.file_tree.show_hidden;
                        cx.notify();
                    })),
            )
    }

    /// One row (plus, when an inline edit targets it, the edit input row).
    fn render_tree_row(
        &self,
        row: &TreeRow,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let path = row.entry.path.clone();
        let is_dir = row.entry.is_dir;
        let selected = self.tab_code().and_then(|c| c.selected.as_deref()) == Some(&*path);
        let muted = cx.theme().muted_foreground;

        // Inline rename replaces the row's label with an input.
        let renaming = matches!(
            &self.file_tree.editing,
            Some(TreeEdit::Rename { path: p, .. }) if *p == path
        );

        let icon = if row.is_root {
            IconName::FolderOpen
        } else if is_dir {
            if row.expanded {
                IconName::FolderOpen
            } else {
                IconName::Folder
            }
        } else {
            IconName::File
        };

        let label: AnyElement = if renaming {
            let input = self.file_tree.editing.as_ref().unwrap().input().clone();
            Input::new(&input).xsmall().into_any_element()
        } else {
            div()
                .flex_1()
                .min_w_0()
                .text_ellipsis()
                .text_sm()
                .when(row.entry.ignored, |d| {
                    d.italic().text_color(muted.opacity(0.7))
                })
                .when(row.is_root, |d| d.font_weight(gpui::FontWeight::MEDIUM))
                .child(SharedString::from(row.entry.name.clone()))
                .into_any_element()
        };

        let row_el = h_flex()
            .id(SharedString::from(format!("tree-{}", path.display())))
            .items_center()
            .gap_1()
            .pl(px(6.0 + row.depth as f32 * INDENT))
            .pr_1()
            .py_0p5()
            .rounded(cx.theme().radius)
            .cursor_pointer()
            .when(selected, |d| d.bg(cx.theme().accent))
            .when(!selected, |d| {
                d.hover(|s| s.bg(cx.theme().accent.opacity(0.5)))
            })
            .child(Icon::new(icon).xsmall().text_color(if is_dir {
                cx.theme().primary
            } else {
                muted
            }))
            .child(label)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener({
                    let path = path.clone();
                    move |this, _, window, cx| {
                        this.file_tree.focus_handle.focus(window, cx);
                        this.file_tree_activate(&path, is_dir, window, cx);
                    }
                }),
            )
            // Drag the row as external paths — the terminal's existing drop
            // handler shell-escapes and inserts them.
            .on_drag(ExternalPaths(vec![path.clone()].into()), {
                let name = row.entry.name.clone();
                move |_, _, _, cx| {
                    let name = name.clone();
                    cx.new(|_| DragGhost { name })
                }
            })
            .context_menu({
                let app = cx.entity().downgrade();
                let path = path.clone();
                let is_root = row.is_root;
                move |menu, _window, cx| {
                    let danger = cx.theme().danger;
                    Self::tree_row_context_menu(menu, &path, is_dir, is_root, danger, &app)
                }
            });

        let mut out: Vec<AnyElement> = vec![row_el.into_any_element()];

        // New-file/new-folder edit input renders as a pseudo-child row of its
        // host directory (right after the dir's own row).
        if let Some(edit) = &self.file_tree.editing {
            let host_matches = match edit {
                TreeEdit::NewFile { dir, .. } | TreeEdit::NewFolder { dir, .. } => *dir == path,
                TreeEdit::Rename { .. } => false,
            };
            if host_matches {
                let input = edit.input().clone();
                out.push(
                    h_flex()
                        .items_center()
                        .gap_1()
                        .pl(px(6.0 + (row.depth + 1) as f32 * INDENT))
                        .pr_1()
                        .py_0p5()
                        .child(Input::new(&input).xsmall())
                        .into_any_element(),
                );
            }
        }
        out
    }

    /// The per-row right-click menu, mirroring Warp's Project Explorer set.
    fn tree_row_context_menu(
        menu: PopupMenu,
        path: &Path,
        is_dir: bool,
        is_root: bool,
        danger: gpui::Hsla,
        app: &gpui::WeakEntity<Self>,
    ) -> PopupMenu {
        let mut menu = menu.min_w(px(200.));
        let p = path.to_path_buf();

        if !is_dir {
            menu = menu.item(PopupMenuItem::new("Open").on_click({
                let app = app.clone();
                let p = p.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| this.open_file_in_editor(&p, window, cx));
                }
            }));
        }
        if is_dir {
            menu = menu.item(PopupMenuItem::new("cd Here").on_click({
                let app = app.clone();
                let p = p.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| this.file_tree_cd(&p, window, cx));
                }
            }));
        }
        menu = menu
            .item(PopupMenuItem::new("Insert Path in Terminal").on_click({
                let app = app.clone();
                let p = p.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| {
                        if let Some(leaf) = this
                            .tabs
                            .get(this.active)
                            .and_then(|t| t.pane.focused_or_first(window, cx))
                        {
                            leaf.update(cx, |view, cx| view.paste(shell_quote(&p), cx));
                        }
                    });
                }
            }))
            .item(PopupMenuItem::new("Attach to Agent").on_click({
                let app = app.clone();
                let p = p.clone();
                move |_, _window, cx| {
                    let _ = app.update(cx, |this, cx| this.file_tree_attach_to_agent(&p, cx));
                }
            }))
            .separator()
            .item(PopupMenuItem::new("New File").on_click({
                let app = app.clone();
                let p = p.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| {
                        this.file_tree_begin_edit(TreeEditKind::NewFile, &p, window, cx)
                    });
                }
            }))
            .item(PopupMenuItem::new("New Folder").on_click({
                let app = app.clone();
                let p = p.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| {
                        this.file_tree_begin_edit(TreeEditKind::NewFolder, &p, window, cx)
                    });
                }
            }));

        if !is_root {
            menu = menu.item(PopupMenuItem::new("Rename").on_click({
                let app = app.clone();
                let p = p.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| {
                        this.file_tree_begin_edit(TreeEditKind::Rename, &p, window, cx)
                    });
                }
            }));
        }

        menu = menu
            .separator()
            .item(PopupMenuItem::new("Copy Path").on_click({
                let p = p.clone();
                move |_, _window, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(p.display().to_string()));
                }
            }))
            .item(PopupMenuItem::new("Reveal in Finder").on_click({
                let p = p.clone();
                move |_, _window, cx| {
                    cx.reveal_path(&p);
                }
            }));

        if !is_root {
            menu = menu.separator().item(
                PopupMenuItem::element(move |_window, _cx| {
                    div().text_color(danger).child("Delete")
                })
                .on_click({
                    let app = app.clone();
                    move |_, window, cx| {
                        let p = p.clone();
                        let _ = app.update(cx, |this, cx| this.file_tree_delete(p, window, cx));
                    }
                }),
            );
        }
        menu
    }

    /// The draggable divider on the tree's right edge.
    pub(crate) fn render_tree_divider(&self, cx: &mut Context<Self>) -> AnyElement {
        let width = self.file_tree.width.clone();
        let dragging = self.file_tree.dragging.clone();
        let idle = cx.theme().border;
        let active = cx.theme().drag_border;
        let line = if dragging.get() { active } else { idle };

        div()
            .id("file-tree-divider")
            .relative()
            .flex_none()
            .w(px(5.))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_col_resize()
            .child(
                gpui::canvas(|_, _, _| (), {
                    let width = width.clone();
                    let dragging = dragging.clone();
                    move |bounds, _, window, _cx| {
                        let divider_x = bounds.origin.x;
                        window.on_mouse_event({
                            let width = width.clone();
                            let dragging = dragging.clone();
                            move |ev: &MouseMoveEvent, _phase, window, _cx| {
                                if !dragging.get() {
                                    return;
                                }
                                // The tree's left edge = divider left minus
                                // the current width; new width follows the
                                // pointer from that fixed edge.
                                let left = divider_x - px(width.get());
                                let w = (ev.position.x - left).max(px(0.));
                                width.set(w.as_f32().clamp(MIN_WIDTH, MAX_WIDTH));
                                window.refresh();
                            }
                        });
                        window.on_mouse_event({
                            let dragging = dragging.clone();
                            move |_ev: &MouseUpEvent, _phase, window, _cx| {
                                if dragging.get() {
                                    dragging.set(false);
                                    window.refresh();
                                }
                            }
                        });
                    }
                })
                .absolute()
                .size_full(),
            )
            .child(div().w(px(1.)).h_full().bg(line))
            .on_mouse_down(MouseButton::Left, {
                move |_ev, window, _cx| {
                    dragging.set(true);
                    window.refresh();
                }
            })
            .into_any_element()
    }
}

/// The little drag ghost shown while a row is dragged toward a terminal.
struct DragGhost {
    name: String,
}

impl gpui::Render for DragGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .rounded(cx.theme().radius)
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .text_sm()
            .child(Icon::new(IconName::File).xsmall())
            .child(SharedString::from(self.name.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool) -> TreeEntry {
        TreeEntry {
            name: name.to_string(),
            path: PathBuf::from(format!("/x/{name}")),
            is_dir,
            ignored: false,
        }
    }

    #[test]
    fn sort_puts_dirs_first_then_case_insensitive_names() {
        let mut v = vec![
            entry("zeta.rs", false),
            entry("Alpha", true),
            entry("beta", true),
            entry("Apple.rs", false),
        ];
        sort_entries(&mut v);
        let names: Vec<&str> = v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Alpha", "beta", "Apple.rs", "zeta.rs"]);
    }

    #[test]
    fn shell_quote_leaves_safe_paths_and_quotes_the_rest() {
        assert_eq!(shell_quote(Path::new("/a/b.txt")), "/a/b.txt");
        assert_eq!(shell_quote(Path::new("/a dir/f")), "'/a dir/f'");
        assert_eq!(shell_quote(Path::new("/a'b")), r"'/a'\''b'");
    }

    #[test]
    fn repo_root_walks_up_to_git() {
        let tmp = std::env::temp_dir().join(format!("tty7-tree-test-{}", std::process::id()));
        let nested = tmp.join("repo/src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(tmp.join("repo/.git")).unwrap();
        assert_eq!(repo_root_for(&nested), Some(tmp.join("repo")));
        assert_eq!(repo_root_for(&tmp), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
