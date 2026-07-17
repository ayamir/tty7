//! Docked code-editor panel (right side of the terminal body).
//!
//! A lightweight "look at / touch up code without leaving the terminal" editor,
//! modelled on Warp's code panel rather than a full IDE. The text engine is
//! `gpui_component::input::InputState` in CodeEditor mode, which brings rope
//! storage, tree-sitter syntax highlighting, line numbers, indent guides, code
//! folding, auto-indent, undo/redo and an in-buffer search/replace bar. This
//! module owns everything around that engine: the open-file set and tab strip,
//! dirty tracking and save, external-modification reload (via `notify`), and
//! the panel chrome (resizable divider, unsaved-close confirmation).
//!
//! Layout: the panel is a right-hand column of the terminal body (a sibling of
//! the pane tree, like the file tree on the left), so terminals and code stay
//! side by side — the Warp mental model, without generalizing the split-pane
//! tree away from `Entity<TerminalView>`.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

use gpui::prelude::*;
use gpui::{
    AnyElement, Bounds, Context, Entity, Focusable as _, MouseButton, MouseMoveEvent, MouseUpEvent,
    Pixels, PromptLevel, SharedString, Subscription, Window, canvas, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState, TabSize};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex,
};

use crate::ui::app::Tty7App;

/// Width band for the editor column, as a fraction of the window body. The
/// divider drag clamps into this so neither the terminal nor the editor can be
/// squeezed into an unusable sliver.
const MIN_WIDTH_FRAC: f32 = 0.2;
const MAX_WIDTH_FRAC: f32 = 0.8;
const DEFAULT_WIDTH_FRAC: f32 = 0.45;

/// Refuse to open files larger than this: the component's code editor is rated
/// to ~50K lines, and a multi-megabyte blob is almost never what a terminal
/// user meant to open in a side panel.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Debounce for external-change reloads, matching the config hot-reload: a
/// save is often a truncate→write→rename burst that should collapse to one.
const RELOAD_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

/// One open file: the component editor state plus the bookkeeping that turns
/// it into a *file* editor (path, dirty flag, on-disk snapshot identity).
pub(crate) struct OpenFile {
    pub(crate) path: PathBuf,
    pub(crate) input: Entity<InputState>,
    /// The buffer has edits not yet written to `path`.
    pub(crate) dirty: bool,
    /// mtime of the content we last loaded from / saved to disk; used to drop
    /// watcher echoes of our own saves.
    disk_mtime: Option<SystemTime>,
    /// Disk changed under unsaved edits: show the reload/keep banner instead
    /// of silently clobbering either side.
    pub(crate) conflict: bool,
    /// Markdown files can flip the buffer into a rendered preview.
    pub(crate) preview: bool,
    /// Soft-wrap state (mirrored here — the input's own flag isn't readable).
    pub(crate) wrap: bool,
    /// The language server serving this file (spawned per workspace root),
    /// with the LSP `languageId` used for document sync. `None` when no
    /// server is configured/available for the language.
    lsp: Option<(std::rc::Rc<crate::ui::lsp::LspClient>, &'static str)>,
    /// Debounced full-document `didChange`; replaced (cancelling the old
    /// timer) on every keystroke.
    change_task: Option<gpui::Task<()>>,
    _sub: Subscription,
}

impl OpenFile {
    /// Tab label: the file name (the path differentiates in the tooltip).
    fn label(&self) -> SharedString {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.path.display().to_string())
            .into()
    }
}

/// State for the editor panel, held on [`Tty7App`].
pub(crate) struct EditorPanelState {
    pub(crate) open: bool,
    pub(crate) files: Vec<OpenFile>,
    pub(crate) active: usize,
    /// Editor column width as a fraction of the body; shared cell so the
    /// divider drag closure can update it mid-render (same pattern as the
    /// split-pane ratio and the tab sidebar width).
    pub(crate) width_frac: Rc<Cell<f32>>,
    dragging: Rc<Cell<bool>>,
    /// Watches the parent directories of open files for external changes.
    /// Rebuilt whenever the open set changes; `None` while nothing is open.
    watcher: Option<notify::RecommendedWatcher>,
    /// Feeds changed paths from the watcher thread into the UI-side reload
    /// loop spawned in [`EditorPanelState::new`].
    events_tx: smol::channel::Sender<PathBuf>,
    /// Language-server registry (one client per server × workspace root).
    pub(crate) lsp: crate::ui::lsp::LspRegistry,
    /// Find-references results, shown as a drawer under the editor.
    pub(crate) references: Option<Vec<ReferenceItem>>,
}

/// One row in the find-references drawer.
pub(crate) struct ReferenceItem {
    pub path: PathBuf,
    /// 0-based target position.
    pub line: u32,
    pub character: u32,
    /// The referenced line's text, for the row preview.
    pub preview: SharedString,
}

impl EditorPanelState {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Tty7App>) -> Self {
        // The reload loop lives for the app: it debounces watcher pings and
        // routes them to `handle_external_change` on the UI thread.
        let (tx, rx) = smol::channel::unbounded::<PathBuf>();
        cx.spawn_in(window, async move |app, cx| {
            while let Ok(first) = rx.recv().await {
                cx.background_executor().timer(RELOAD_DEBOUNCE).await;
                let mut changed: HashSet<PathBuf> = HashSet::from([first]);
                while let Ok(more) = rx.try_recv() {
                    changed.insert(more);
                }
                let ok = app.update_in(cx, |app, window, cx| {
                    for path in changed {
                        app.editor_handle_external_change(&path, window, cx);
                    }
                });
                if ok.is_err() {
                    break; // app dropped; stop the loop
                }
            }
        })
        .detach();
        Self {
            open: false,
            files: Vec::new(),
            active: 0,
            width_frac: Rc::new(Cell::new(DEFAULT_WIDTH_FRAC)),
            dragging: Rc::new(Cell::new(false)),
            watcher: None,
            events_tx: tx,
            lsp: crate::ui::lsp::LspRegistry::new(window, cx),
            references: None,
        }
    }

    pub(crate) fn active_file(&self) -> Option<&OpenFile> {
        self.files.get(self.active)
    }

    /// Rebuild the external-change watcher over the current open set. Watches
    /// each file's *parent directory* (non-recursively): editors that save via
    /// rename replace the inode, which a direct file watch loses track of.
    fn rebuild_watcher(&mut self) {
        use notify::{RecursiveMode, Watcher};
        self.watcher = None;
        if self.files.is_empty() {
            return;
        }
        let watched: HashSet<PathBuf> = self.files.iter().map(|f| f.path.clone()).collect();
        let dirs: HashSet<PathBuf> = watched
            .iter()
            .filter_map(|p| p.parent().map(Path::to_path_buf))
            .collect();
        let tx = self.events_tx.clone();
        let handler = move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            for p in &event.paths {
                if watched.contains(p) {
                    let _ = tx.try_send(p.clone());
                }
            }
        };
        let mut watcher = match notify::recommended_watcher(handler) {
            Ok(w) => w,
            Err(e) => {
                log::warn!("editor: external-change watcher unavailable: {e}");
                return;
            }
        };
        for dir in dirs {
            if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
                log::warn!("editor: failed to watch {}: {e}", dir.display());
            }
        }
        self.watcher = Some(watcher);
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (tested).
// ---------------------------------------------------------------------------

/// The tree-sitter language name for a path, matching the grammars compiled
/// into gpui-component's `tree-sitter-languages` feature. Falls back to
/// `"text"` (plain, no highlighting) for anything unknown.
pub(crate) fn language_for_path(path: &Path) -> &'static str {
    // Whole-filename matches first (no useful extension).
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        let lowered = name.to_ascii_lowercase();
        match lowered.as_str() {
            "makefile" | "gnumakefile" => return "make",
            "cmakelists.txt" => return "cmake",
            _ => {}
        }
        // Dotfile shell rc's: .zshrc, .bashrc, .profile…
        if lowered.starts_with('.') && (lowered.contains("shrc") || lowered.ends_with("profile")) {
            return "bash";
        }
    }
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return "text";
    };
    match ext.to_ascii_lowercase().as_str() {
        "rs" => "rust",
        "go" => "go",
        "py" | "pyi" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "json" | "jsonc" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "html" | "htm" => "html",
        "css" => "css",
        "md" | "markdown" => "markdown",
        "sh" | "bash" | "zsh" => "bash",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => "cpp",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "lua" => "lua",
        "rb" => "ruby",
        "php" => "php",
        "sql" => "sql",
        "swift" => "swift",
        "scala" => "scala",
        "zig" => "zig",
        "proto" => "proto",
        "diff" | "patch" => "diff",
        "ex" | "exs" => "elixir",
        "erb" => "erb",
        "ejs" => "ejs",
        "svelte" => "svelte",
        "astro" => "astro",
        "graphql" | "gql" => "graphql",
        "cs" => "csharp",
        "cmake" => "cmake",
        _ => "text",
    }
}

/// Quick binary sniff: a NUL byte in the head of the file. Text files never
/// contain NULs; this catches executables/images before `from_utf8` chokes on
/// them with a less helpful error.
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

// ---------------------------------------------------------------------------
// Tty7App: open / save / close / external reload.
// ---------------------------------------------------------------------------

impl Tty7App {
    /// Open `path` in the editor panel (activating an existing tab when the
    /// file is already open) and reveal the panel. Errors surface as window
    /// notifications rather than a half-open tab.
    pub(crate) fn open_file_in_editor(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Some(ix) = self.editor.files.iter().position(|f| f.path == path) {
            self.editor.active = ix;
            self.editor.open = true;
            self.focus_editor(window, cx);
            cx.notify();
            return;
        }
        match std::fs::metadata(&path) {
            Ok(meta) if meta.len() > MAX_FILE_BYTES => {
                window.push_notification(
                    format!(
                        "\"{}\" is too large for the editor ({} MB)",
                        path.display(),
                        meta.len() / (1024 * 1024)
                    ),
                    cx,
                );
                return;
            }
            Err(e) => {
                window.push_notification(format!("Can't open {}: {e}", path.display()), cx);
                return;
            }
            Ok(_) => {}
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                window.push_notification(format!("Can't read {}: {e}", path.display()), cx);
                return;
            }
        };
        if looks_binary(&bytes) {
            window.push_notification(
                format!("\"{}\" looks like a binary file", path.display()),
                cx,
            );
            return;
        }
        let text = match String::from_utf8(bytes) {
            Ok(t) => t,
            Err(_) => {
                window.push_notification(format!("\"{}\" is not valid UTF-8", path.display()), cx);
                return;
            }
        };
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let language = language_for_path(&path);
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor(language)
                .multi_line(true)
                .tab_size(TabSize {
                    tab_size: 4,
                    hard_tabs: false,
                })
                .line_number(true)
                .searchable(true)
                .replaceable(true)
                .folding(true)
                .soft_wrap(false)
                .default_value(text.clone())
        });
        // Language server: spawn (or reuse) the server for this language at
        // the file's workspace root, open the document, and install the
        // completion / hover / definition providers on the input.
        let root = crate::ui::file_tree::repo_root_for(&path)
            .or_else(|| path.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("/"));
        let lsp = self.editor.lsp.client_for(language, &root);
        if let Some((client, language_id)) = &lsp {
            client.did_open(&path, language_id, &text);
            let provider = std::rc::Rc::new(crate::ui::lsp::FileLsp {
                client: client.clone(),
                path: path.clone(),
            });
            input.update(cx, |st, _| {
                st.lsp.completion_provider = Some(provider.clone());
                st.lsp.hover_provider = Some(provider.clone());
                st.lsp.definition_provider = Some(provider);
            });
        }
        // Dirty tracking: `set_value` suppresses events, so every Change here
        // is a real user edit. Each edit also (re)arms the debounced LSP
        // didChange sync.
        let sub = cx.subscribe_in(&input, window, {
            let path = path.clone();
            move |this: &mut Tty7App, _input, ev, window, cx| {
                if matches!(ev, InputEvent::Change) {
                    let path = path.clone();
                    if let Some(f) = this.editor.files.iter_mut().find(|f| f.path == path) {
                        if !f.dirty {
                            f.dirty = true;
                        }
                        if f.lsp.is_some() {
                            f.change_task = Some(cx.spawn_in(window, async move |app, cx| {
                                cx.background_executor()
                                    .timer(std::time::Duration::from_millis(150))
                                    .await;
                                let _ = app.update(cx, |app, cx| {
                                    app.editor_sync_lsp_document(&path, cx);
                                });
                            }));
                        }
                        cx.notify();
                    }
                }
            }
        });
        self.editor.files.push(OpenFile {
            path,
            input,
            dirty: false,
            disk_mtime: mtime,
            conflict: false,
            preview: false,
            wrap: false,
            lsp,
            change_task: None,
            _sub: sub,
        });
        self.editor.active = self.editor.files.len() - 1;
        self.editor.open = true;
        self.editor.rebuild_watcher();
        self.focus_editor(window, cx);
        cx.notify();
    }

    /// Show/hide the editor column. Opening with no files is allowed — the
    /// empty state points at the file tree.
    pub(crate) fn toggle_editor_panel(&mut self, cx: &mut Context<Self>) {
        self.editor.open = !self.editor.open;
        cx.notify();
    }

    /// Focus the active file's text input (e.g. right after opening a file).
    fn focus_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(f) = self.editor.active_file() {
            f.input.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    /// Whether keyboard focus currently sits inside the editor panel. Lets
    /// shared shortcuts (⌘S, ⌘W) route here before their terminal meaning.
    pub(crate) fn editor_has_focus(&self, window: &Window, cx: &Context<Self>) -> bool {
        self.editor.open
            && self.editor.active_file().is_some_and(|f| {
                f.input
                    .read(cx)
                    .focus_handle(cx)
                    .contains_focused(window, cx)
            })
    }

    /// `EditorSave` (⌘S): write the active buffer back to its path.
    pub(crate) fn editor_save_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = self.editor.files.get_mut(self.editor.active) else {
            return;
        };
        let text = f.input.read(cx).text().to_string();
        match std::fs::write(&f.path, &text) {
            Ok(()) => {
                f.dirty = false;
                f.conflict = false;
                f.disk_mtime = std::fs::metadata(&f.path).and_then(|m| m.modified()).ok();
                if let Some((client, _)) = &f.lsp {
                    // Make sure the server saw the final text before the save
                    // notification (the debounced didChange may still be
                    // pending), so on-save diagnostics match the disk state.
                    client.did_change(&f.path, &text);
                    client.did_save(&f.path);
                }
                cx.notify();
            }
            Err(e) => {
                window.push_notification(format!("Save failed: {e}"), cx);
            }
        }
    }

    /// Close the file tab at `ix`. Dirty buffers get a native three-way prompt
    /// (save / discard / cancel) before anything is lost.
    pub(crate) fn editor_close_file(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(f) = self.editor.files.get(ix) else {
            return;
        };
        if !f.dirty {
            self.editor_remove_file(ix, cx);
            return;
        }
        let name = f.label();
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("\"{name}\" has unsaved changes"),
            None,
            &["Save", "Discard", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |app, cx| {
            let Ok(choice) = answer.await else { return };
            let _ = app.update_in(cx, |app, window, cx| match choice {
                0 => {
                    // Save, then close. Save failure keeps the tab open.
                    let prev_active = app.editor.active;
                    app.editor.active = ix;
                    app.editor_save_active(window, cx);
                    app.editor.active = prev_active;
                    if app.editor.files.get(ix).is_some_and(|f| !f.dirty) {
                        app.editor_remove_file(ix, cx);
                    }
                }
                1 => app.editor_remove_file(ix, cx),
                _ => {}
            });
        })
        .detach();
    }

    /// If focus is in the editor, close the active file tab and report `true`
    /// (so ⌘W routes here instead of closing the terminal tab).
    pub(crate) fn editor_close_active_if_focused(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.editor_has_focus(window, cx) {
            return false;
        }
        if self.editor.files.is_empty() {
            self.editor.open = false;
            cx.notify();
            return true;
        }
        self.editor_close_file(self.editor.active, window, cx);
        true
    }

    fn editor_remove_file(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.editor.files.len() {
            return;
        }
        let f = self.editor.files.remove(ix);
        if let Some((client, _)) = &f.lsp {
            client.did_close(&f.path);
        }
        if self.editor.active >= ix && self.editor.active > 0 {
            self.editor.active -= 1;
        }
        self.editor.rebuild_watcher();
        cx.notify();
    }

    /// Push the buffer's current text to the language server (the debounced
    /// tail of a typing burst).
    pub(crate) fn editor_sync_lsp_document(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(f) = self.editor.files.iter().find(|f| f.path == *path) else {
            return;
        };
        if let Some((client, _)) = &f.lsp {
            client.did_change(&f.path, &f.input.read(cx).text().to_string());
        }
    }

    /// Apply `publishDiagnostics` for `path` to its open buffer, if any.
    pub(crate) fn editor_apply_diagnostics(
        &mut self,
        path: &Path,
        diags: Vec<lsp_types::Diagnostic>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(f) = self.editor.files.iter().find(|f| f.path == *path) else {
            return;
        };
        f.input.clone().update(cx, |st, cx| {
            let text = st.text().clone();
            if let Some(set) = st.diagnostics_mut() {
                set.reset(&text);
                set.extend(diags);
                cx.notify();
            }
        });
    }

    /// `EditorGotoDefinition` (F12): resolve the definition at the cursor and
    /// jump — opening the target file first when it lives elsewhere (the
    /// in-buffer ⌘-click path can't cross files; this one can).
    pub(crate) fn editor_goto_definition(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = self.editor.active_file() else {
            return;
        };
        let Some((client, _)) = &f.lsp else { return };
        let st = f.input.read(cx);
        let (text, offset) = (st.text().clone(), st.cursor());
        let Some(params) = crate::ui::lsp::LspClient::position_params(&f.path, &text, offset)
        else {
            return;
        };
        let rx = client.request("textDocument/definition", params);
        cx.spawn_in(window, async move |app, cx| {
            let Ok(v) = rx.recv().await else { return };
            let links = crate::ui::lsp::normalize_definitions(v);
            let Some(link) = links.first() else { return };
            let Some(target) = crate::ui::lsp::path_for_uri(link.target_uri.as_str()) else {
                return;
            };
            let pos = link.target_selection_range.start;
            let _ = app.update_in(cx, |app, window, cx| {
                app.editor_jump_to(&target, pos, window, cx);
            });
        })
        .detach();
    }

    /// `EditorFindReferences` (⇧F12): list every reference to the symbol at
    /// the cursor in a drawer under the editor.
    pub(crate) fn editor_find_references(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = self.editor.active_file() else {
            return;
        };
        let Some((client, _)) = &f.lsp else { return };
        let st = f.input.read(cx);
        let (text, offset) = (st.text().clone(), st.cursor());
        let Some(mut params) = crate::ui::lsp::LspClient::position_params(&f.path, &text, offset)
        else {
            return;
        };
        params["context"] = serde_json::json!({ "includeDeclaration": true });
        let rx = client.request("textDocument/references", params);
        cx.spawn_in(window, async move |app, cx| {
            let Ok(v) = rx.recv().await else { return };
            let locations: Vec<lsp_types::Location> = serde_json::from_value(v).unwrap_or_default();
            // Read each referenced line once per file for the row previews;
            // off the UI thread, since this touches the filesystem.
            let mut items: Vec<ReferenceItem> = Vec::new();
            let mut file_lines: std::collections::HashMap<PathBuf, Vec<String>> =
                std::collections::HashMap::new();
            for loc in locations.into_iter().take(200) {
                let Some(path) = crate::ui::lsp::path_for_uri(loc.uri.as_str()) else {
                    continue;
                };
                let lines = file_lines.entry(path.clone()).or_insert_with(|| {
                    std::fs::read_to_string(&path)
                        .map(|t| t.lines().map(|l| l.to_string()).collect())
                        .unwrap_or_default()
                });
                let line = loc.range.start.line;
                let preview = lines
                    .get(line as usize)
                    .map(|l| l.trim().to_string())
                    .unwrap_or_default();
                items.push(ReferenceItem {
                    path,
                    line,
                    character: loc.range.start.character,
                    preview: preview.into(),
                });
            }
            let _ = app.update(cx, |app, cx| {
                app.editor.references = Some(items);
                cx.notify();
            });
        })
        .detach();
    }

    /// Open `path` (if needed) and place the cursor at an LSP position.
    pub(crate) fn editor_jump_to(
        &mut self,
        path: &Path,
        pos: lsp_types::Position,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_file_in_editor(path, window, cx);
        if let Some(f) = self.editor.active_file()
            && f.path == *path
        {
            f.input.clone().update(cx, |st, cx| {
                st.set_cursor_position(pos, window, cx);
            });
        }
    }

    /// A watched file changed on disk. Clean buffers reload silently; dirty
    /// ones raise the conflict banner and let the user pick a side.
    pub(crate) fn editor_handle_external_change(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.editor.files.iter().position(|f| f.path == *path) else {
            return;
        };
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        {
            let f = &self.editor.files[ix];
            // Our own save's echo: mtime matches what we just wrote.
            if mtime.is_some() && mtime == f.disk_mtime {
                return;
            }
        }
        if self.editor.files[ix].dirty {
            self.editor.files[ix].conflict = true;
            cx.notify();
            return;
        }
        self.editor_reload_from_disk(ix, window, cx);
    }

    /// Replace the buffer with the on-disk content (used by the silent reload
    /// and the conflict banner's "Reload" choice). A vanished file just keeps
    /// the buffer and marks it dirty — saving will recreate it.
    pub(crate) fn editor_reload_from_disk(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(f) = self.editor.files.get_mut(ix) else {
            return;
        };
        let Ok(text) = std::fs::read_to_string(&f.path) else {
            f.dirty = true;
            f.conflict = false;
            cx.notify();
            return;
        };
        f.disk_mtime = std::fs::metadata(&f.path).and_then(|m| m.modified()).ok();
        f.dirty = false;
        f.conflict = false;
        if let Some((client, _)) = &f.lsp {
            client.did_change(&f.path, &text);
        }
        let input = f.input.clone();
        input.update(cx, |input, cx| input.set_value(text, window, cx));
        cx.notify();
    }
}

// ---------------------------------------------------------------------------
// Rendering.
// ---------------------------------------------------------------------------

impl Tty7App {
    /// The editor column (right of the terminal body), or `None` when hidden.
    /// Rendered as `[divider | tabs / banner / editor]`.
    pub(crate) fn render_editor_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.editor.open {
            return None;
        }
        let width = self
            .editor
            .width_frac
            .get()
            .clamp(MIN_WIDTH_FRAC, MAX_WIDTH_FRAC);
        let body = match self.editor.active_file() {
            None => self.render_editor_empty(cx).into_any_element(),
            // Markdown preview replaces the buffer with a rendered view.
            Some(f) if f.preview => {
                let markdown = f.input.read(cx).text().to_string();
                div()
                    .id("editor-md-preview")
                    .size_full()
                    .overflow_y_scroll()
                    .px_4()
                    .py_3()
                    .child(gpui_component::text::TextView::markdown(
                        "editor-md-preview-body",
                        markdown,
                    ))
                    .into_any_element()
            }
            Some(f) => {
                let input = f.input.clone();
                Input::new(&input)
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(cx.theme().mono_font_size)
                    .size_full()
                    .into_any_element()
            }
        };
        let conflict_banner = self
            .editor
            .active_file()
            .filter(|f| f.conflict)
            .map(|_| self.render_editor_conflict_banner(cx));
        let references = self.render_editor_references(cx);

        let panel = v_flex()
            .id("code-editor-panel")
            .flex_none()
            .h_full()
            .w(gpui::relative(width))
            .min_w(px(240.))
            .bg(cx.theme().background)
            .border_l_1()
            .border_color(cx.theme().border)
            .child(self.render_editor_tabs(window, cx))
            .when_some(conflict_banner, |this, b| this.child(b))
            .child(div().flex_1().min_h_0().child(body))
            .when_some(references, |this, drawer| this.child(drawer));

        Some(
            h_flex()
                .flex_none()
                .h_full()
                .child(self.render_editor_divider(cx))
                .child(panel)
                .into_any_element(),
        )
    }

    /// Empty state: the panel is open with nothing loaded.
    fn render_editor_empty(&self, cx: &Context<Self>) -> gpui::Div {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                Icon::new(IconName::File)
                    .large()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Open a file from the file tree"),
            )
    }

    /// The file tab strip along the panel top.
    fn render_editor_tabs(&self, _window: &Window, cx: &mut Context<Self>) -> gpui::Div {
        let active = self.editor.active;
        let tabs = self.editor.files.iter().enumerate().map(|(ix, f)| {
            let is_active = ix == active;
            let title = f.label();
            h_flex()
                .id(("editor-tab", ix))
                .flex_none()
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .rounded(cx.theme().radius)
                .text_sm()
                .cursor_pointer()
                .when(is_active, |d| d.bg(cx.theme().accent))
                .when(!is_active, |d| {
                    d.text_color(cx.theme().muted_foreground)
                        .hover(|s| s.bg(cx.theme().accent.opacity(0.5)))
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        this.editor.active = ix;
                        this.focus_editor(window, cx);
                        cx.notify();
                    }),
                )
                .child(div().child(title))
                .when(f.dirty, |d| {
                    d.child(div().size(px(7.)).rounded_full().bg(cx.theme().warning))
                })
                .child(
                    Button::new(("editor-tab-close", ix))
                        .icon(IconName::Close)
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.editor_close_file(ix, window, cx);
                        })),
                )
        });
        h_flex()
            .flex_none()
            .w_full()
            .items_center()
            .gap_1()
            .px_1()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .overflow_x_hidden()
            .children(tabs)
            .child(div().flex_1())
            // Markdown files get a preview toggle.
            .when_some(
                self.editor
                    .active_file()
                    .filter(|f| language_for_path(&f.path) == "markdown"),
                |this, f| {
                    let preview = f.preview;
                    this.child(
                        Button::new("editor-md-preview-toggle")
                            .label(if preview { "Edit" } else { "Preview" })
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(|this, _, _w, cx| {
                                let ix = this.editor.active;
                                if let Some(f) = this.editor.files.get_mut(ix) {
                                    f.preview = !f.preview;
                                    cx.notify();
                                }
                            })),
                    )
                },
            )
            // Soft-wrap toggle for the active buffer.
            .when(self.editor.active_file().is_some(), |this| {
                this.child(
                    Button::new("editor-wrap-toggle")
                        .label("Wrap")
                        .ghost()
                        .xsmall()
                        .tooltip("Toggle soft wrap")
                        .on_click(cx.listener(|this, _, window, cx| {
                            let ix = this.editor.active;
                            if let Some(f) = this.editor.files.get_mut(ix) {
                                f.wrap = !f.wrap;
                                let wrap = f.wrap;
                                f.input.clone().update(cx, |st, cx| {
                                    st.set_soft_wrap(wrap, window, cx);
                                });
                            }
                        })),
                )
            })
            .child(
                Button::new("editor-panel-close")
                    .icon(IconName::Close)
                    .ghost()
                    .small()
                    .tooltip("Close editor panel")
                    .on_click(cx.listener(|this, _, _w, cx| {
                        this.editor.open = false;
                        cx.notify();
                    })),
            )
    }

    /// The find-references drawer (⇧F12 results) under the editor body.
    fn render_editor_references(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let refs = self.editor.references.as_ref()?;
        let muted = cx.theme().muted_foreground;
        let rows = refs.iter().enumerate().map(|(ix, r)| {
            let name = r
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let (path, line, character) = (r.path.clone(), r.line, r.character);
            h_flex()
                .id(("editor-ref", ix))
                .items_center()
                .gap_2()
                .px_2()
                .py_0p5()
                .text_sm()
                .cursor_pointer()
                .hover(|s| s.bg(cx.theme().accent.opacity(0.5)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        this.editor_jump_to(
                            &path,
                            lsp_types::Position::new(line, character),
                            window,
                            cx,
                        );
                    }),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(muted)
                        .child(format!("{name}:{}", r.line + 1)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_ellipsis()
                        .child(r.preview.clone()),
                )
        });
        Some(
            v_flex()
                .flex_none()
                .max_h(gpui::relative(0.4))
                .border_t_1()
                .border_color(cx.theme().border)
                .child(
                    h_flex()
                        .items_center()
                        .px_2()
                        .py_1()
                        .text_sm()
                        .child(div().flex_1().child(format!("{} references", refs.len())))
                        .child(
                            Button::new("editor-refs-close")
                                .icon(IconName::Close)
                                .ghost()
                                .xsmall()
                                .on_click(cx.listener(|this, _, _w, cx| {
                                    this.editor.references = None;
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    v_flex()
                        .id("editor-refs-list")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .children(rows),
                )
                .into_any_element(),
        )
    }

    /// Banner shown when the file changed on disk while the buffer is dirty.
    fn render_editor_conflict_banner(&self, cx: &mut Context<Self>) -> AnyElement {
        let ix = self.editor.active;
        h_flex()
            .flex_none()
            .w_full()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .bg(cx.theme().warning.opacity(0.15))
            .border_b_1()
            .border_color(cx.theme().border)
            .text_sm()
            .child(div().flex_1().child("File changed on disk"))
            .child(
                Button::new("editor-conflict-reload")
                    .label("Reload")
                    .small()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.editor_reload_from_disk(ix, window, cx);
                    })),
            )
            .child(
                Button::new("editor-conflict-keep")
                    .label("Keep mine")
                    .ghost()
                    .small()
                    .on_click(cx.listener(move |this, _, _w, cx| {
                        if let Some(f) = this.editor.files.get_mut(ix) {
                            f.conflict = false;
                            cx.notify();
                        }
                    })),
            )
            .into_any_element()
    }

    /// The draggable divider between the terminal body and the editor column.
    /// Same canvas + window-mouse-listener pattern as the split-pane divider.
    fn render_editor_divider(&self, cx: &mut Context<Self>) -> AnyElement {
        let width_frac = self.editor.width_frac.clone();
        let dragging = self.editor.dragging.clone();
        let idle = cx.theme().border;
        let active = cx.theme().drag_border;

        // Measured bounds of the whole body row (terminal + editor), captured
        // by the backing canvas so the drag maps pointer → width fraction.
        let container: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::new(Cell::new(None));
        let backing = canvas(
            {
                let container = container.clone();
                move |bounds, _window, _cx| container.set(Some(bounds))
            },
            {
                let container = container.clone();
                let width_frac = width_frac.clone();
                let dragging = dragging.clone();
                move |_bounds, _state, window, _cx| {
                    window.on_mouse_event({
                        let container = container.clone();
                        let width_frac = width_frac.clone();
                        let dragging = dragging.clone();
                        move |ev: &MouseMoveEvent, _phase, window, _cx| {
                            if !dragging.get() {
                                return;
                            }
                            let Some(b) = container.get() else { return };
                            // The canvas only spans the divider itself; use the
                            // window width as the drag reference instead.
                            let total = window.viewport_size().width;
                            if total.as_f32() <= 0.0 {
                                return;
                            }
                            let from_right = (total - ev.position.x).max(px(0.));
                            let frac = (from_right / total).clamp(MIN_WIDTH_FRAC, MAX_WIDTH_FRAC);
                            let _ = b;
                            width_frac.set(frac);
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
            },
        )
        .absolute()
        .size_full();

        let line = if dragging.get() { active } else { idle };
        div()
            .id("editor-divider")
            .relative()
            .flex_none()
            .w(px(5.))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_col_resize()
            .child(backing)
            .child(div().w(px(1.)).h_full().bg(line))
            .on_mouse_down(MouseButton::Left, {
                let dragging = dragging.clone();
                move |_ev, window, _cx| {
                    dragging.set(true);
                    window.refresh();
                }
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_map_covers_common_extensions() {
        for (path, lang) in [
            ("a/b/main.rs", "rust"),
            ("x.tsx", "tsx"),
            ("x.jsx", "javascript"),
            ("x.yml", "yaml"),
            ("Makefile", "make"),
            ("CMakeLists.txt", "cmake"),
            (".zshrc", "bash"),
            ("notes.md", "markdown"),
            ("query.SQL", "sql"),
            ("unknown.xyz", "text"),
            ("no_ext", "text"),
        ] {
            assert_eq!(language_for_path(Path::new(path)), lang, "path {path}");
        }
    }

    #[test]
    fn binary_sniff_flags_nul_bytes_only() {
        assert!(looks_binary(b"\x7fELF\x00\x01"));
        assert!(!looks_binary("plain text\nwith lines".as_bytes()));
        assert!(!looks_binary("中文 UTF-8 内容".as_bytes()));
    }
}
