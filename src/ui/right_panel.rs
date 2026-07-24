//! The right detail panel: a docked column showing what the active pane *is*,
//! rather than what it's printing — session facts, its working-tree diff, and
//! its file tree.
//!
//! It splits across two hosts on purpose. The **tab row lives in the title bar**
//! (built in [`tab_strip`](crate::ui::tab_strip)), so the panel's controls sit on
//! the same line as the window's own chrome instead of stacking a second 40px bar
//! under it; the **body** is this module's column inside `body_area`. The two are
//! kept in register by both measuring from `Config::right_panel_width`, so the
//! tabs sit exactly over the content they switch.
//!
//! No new source of truth: Info reads the same `TerminalView`/`Tab` accessors the
//! sidebar row does, Changes probes the same `git_diff` the diff overlay does, and
//! Files renders the same rows as the code panel's tree.

use gpui::{AnyElement, Context, Window, div, prelude::*, px};
use gpui_component::button::Button;
use gpui_component::input::Input;
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use std::path::PathBuf;

use crate::core::config::{Config, RightPanelTab};
use crate::daemon::protocol::PaneProcs;
use crate::terminal::git_diff::{self, DiffSnapshot};
use crate::ui::app::{CONTENT_INSET, Tty7App};

/// Bounds for the panel's width, mirroring the rail's: a floor so the tree never
/// becomes an ellipsis parade, and a ceiling as a fraction of the window so a
/// persisted value can't swallow the terminal.
pub(crate) const MIN_WIDTH: f32 = 200.;
pub(crate) const MAX_WIDTH_RATIO: f32 = 0.5;

/// Width (px) of the resize handle's invisible hit-area, centered on the panel's
/// left border — same geometry as the tab rail's.
const RESIZE_HANDLE_WIDTH: f32 = 8.;

/// Panel state that isn't a user preference (those live in `Config`): the cached
/// diff for the Changes tab and the body's scroll position.
#[derive(Default)]
pub(crate) struct RightPanelState {
    /// The cwd `diff` was probed from — compared against the active pane's cwd to
    /// decide whether the cached snapshot is still about the right repository.
    pub(crate) diff_cwd: Option<PathBuf>,
    /// Last completed probe. `Some(None)` and `None` are different answers:
    /// "probed, not a work tree" versus "never probed".
    pub(crate) diff: Option<Option<DiffSnapshot>>,
    /// A probe is in flight; keeps the render path from spawning a second one.
    pub(crate) diff_loading: bool,
    /// The pane `procs` describes, so a pane switch invalidates it rather than
    /// showing the previous pane's processes under the new pane's name.
    pub(crate) procs_pane: Option<u64>,
    /// Last completed process/port query for `procs_pane`.
    pub(crate) procs: Option<PaneProcs>,
    /// A query is in flight. Also the poll loop's own guard: the loop reschedules
    /// itself from the completion handler, so this being set means "a tick is
    /// already on the way" and a re-render must not start a second chain.
    pub(crate) procs_loading: bool,
}

/// How often the Info tab re-queries processes and ports while it's open. Fast
/// enough that starting a dev server shows up as you tab over, slow enough that
/// the process-table walk stays off the profile.
const PROCS_POLL: std::time::Duration = std::time::Duration::from_millis(2000);

impl Tty7App {
    /// Whether the right panel is docked open. The title bar's tab row, the body
    /// column and the code overlay's right inset all derive from this.
    pub(crate) fn right_panel_open(&self, cx: &gpui::App) -> bool {
        cx.global::<Config>().right_panel_visible && !self.tabs.is_empty()
    }

    /// The panel's live width, re-clamped to the window the same way the rail's
    /// is, so a persisted value from a larger display can't take over.
    /// Named `_px` rather than `_width` because the field it reads is
    /// `right_panel_width`; a method of the same name would shadow it awkwardly
    /// at every call site.
    pub(crate) fn right_panel_px(&self, window: &Window, _cx: &gpui::App) -> f32 {
        let max = (window.viewport_size().width.as_f32() * MAX_WIDTH_RATIO).max(MIN_WIDTH);
        // The live cell, not the config: a drag in progress writes only here, and
        // persists to the config on release.
        self.right_panel_width.get().clamp(MIN_WIDTH, max)
    }

    /// `ToggleRightPanel` (⌘J).
    pub(crate) fn toggle_right_panel(&mut self, cx: &mut Context<Self>) {
        let next = !cx.global::<Config>().right_panel_visible;
        self.update_config(cx, |cfg| cfg.right_panel_visible = next);
    }

    /// Select a tab. Opens the panel if it was closed, so the title bar's tab
    /// tiles double as "show me this" rather than being inert while hidden.
    pub(crate) fn set_right_panel_tab(&mut self, tab: RightPanelTab, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| {
            cfg.right_panel_tab = tab;
            cfg.right_panel_visible = true;
        });
    }

    /// The docked column, or `None` while the panel is closed.
    pub(crate) fn render_right_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.right_panel_open(cx) {
            return None;
        }
        let width = self.right_panel_px(window, cx);
        let tab = cx.global::<Config>().right_panel_tab;

        let body = match tab {
            RightPanelTab::Info => self.render_panel_info(window, cx),
            RightPanelTab::Outline => self.render_panel_outline(window, cx),
            RightPanelTab::Changes => self.render_panel_changes(window, cx),
            RightPanelTab::Files => self.render_panel_files(window, cx),
        };
        let (backing, handle) = self.right_panel_resize(cx);

        Some(
            v_flex()
                .id("right-panel")
                .relative()
                .flex_none()
                .w(px(width))
                .h_full()
                .child(backing)
                // The sunk sidebar surface, like the tab rail: both are chrome
                // around the terminal, so they read as the same material.
                .bg(cx.theme().sidebar)
                .border_l_1()
                .border_color(cx.theme().sidebar_border)
                // A title-bar-height top zone of its own, exactly like the rail's.
                // This is what makes the panel read as one column instead of a box
                // bolted under the title bar: its surface runs the full height of
                // the window, and the tab row sits *on* it rather than on the
                // terminal's bar above a seam.
                .child(
                    h_flex()
                        .flex_none()
                        .h(px(crate::ui::app::TITLE_BAR_HEIGHT))
                        .items_center()
                        .gap(px(2.))
                        .pl(px(CONTENT_INSET - crate::ui::app::TILE_PAD))
                        .children(self.right_panel_tabs(cx))
                        .child(div().flex_1())
                        // The panel is what reaches the window's right edge while
                        // it's open, so it carries the corner chrome.
                        .child(self.window_chrome(window, cx)),
                )
                .child(body)
                .child(handle)
                .into_any_element(),
        )
    }

    /// The panel's resize drag: a measuring canvas that installs window-level
    /// mouse listeners while held, plus the handle itself. Mirrors the tab rail's
    /// (`tab_sidebar.rs`) with the axis flipped — this panel is anchored to the
    /// window's right edge, so width grows as the pointer moves *left*, measured
    /// from the panel's own right edge rather than its origin.
    fn right_panel_resize(&self, cx: &mut Context<Self>) -> (AnyElement, AnyElement) {
        use gpui::{Bounds, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, canvas};
        use std::cell::Cell as StdCell;
        use std::rc::Rc;

        let container: Rc<StdCell<Option<Bounds<Pixels>>>> = Rc::new(StdCell::new(None));
        let backing = canvas(
            {
                let container = container.clone();
                move |bounds, _window, _cx| container.set(Some(bounds))
            },
            {
                let container = container.clone();
                let width_cell = self.right_panel_width.clone();
                let dragging = self.right_panel_dragging.clone();
                move |_bounds, _state, window, _cx| {
                    window.on_mouse_event({
                        let container = container.clone();
                        let width_cell = width_cell.clone();
                        let dragging = dragging.clone();
                        move |ev: &MouseMoveEvent, _phase, window, _cx| {
                            if !dragging.get() {
                                return;
                            }
                            let Some(b) = container.get() else {
                                return;
                            };
                            let right = b.origin.x + b.size.width;
                            let raw = (right - ev.position.x).as_f32();
                            let max = (window.viewport_size().width.as_f32() * MAX_WIDTH_RATIO)
                                .max(MIN_WIDTH);
                            width_cell.set(raw.clamp(MIN_WIDTH, max));
                            window.refresh();
                        }
                    });
                    window.on_mouse_event({
                        let width_cell = width_cell.clone();
                        let dragging = dragging.clone();
                        move |_ev: &MouseUpEvent, _phase, window, cx| {
                            if !dragging.get() {
                                return;
                            }
                            dragging.set(false);
                            let w = width_cell.get();
                            let cfg = cx.global_mut::<Config>();
                            if cfg.right_panel_width != w {
                                cfg.right_panel_width = w;
                                cfg.save();
                            }
                            window.refresh();
                        }
                    });
                }
            },
        )
        .absolute()
        .size_full()
        .into_any_element();

        let active = self.right_panel_dragging.get();
        let handle = div()
            .group("right-panel-resize")
            .absolute()
            .top_0()
            .left(px(-(RESIZE_HANDLE_WIDTH / 2.)))
            .w(px(RESIZE_HANDLE_WIDTH))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_col_resize()
            .child(
                div()
                    .w(px(1.))
                    .h_full()
                    .when(active, |d| d.bg(cx.theme().drag_border))
                    .group_hover("right-panel-resize", |s| s.bg(cx.theme().drag_border)),
            )
            .on_mouse_down(MouseButton::Left, {
                let dragging = self.right_panel_dragging.clone();
                move |_ev, window, _cx| {
                    dragging.set(true);
                    window.refresh();
                }
            })
            .into_any_element();

        (backing, handle)
    }

    /// A section label inside the panel body — the small caps line that names
    /// what the icon-only tab row can't. `trailing` carries a tab's own controls
    /// where it has any, so they sit on the label's line rather than earning a
    /// second header row.
    fn panel_title(
        &self,
        text: &str,
        trailing: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .flex_none()
            .h(px(28.))
            .items_center()
            .justify_between()
            .pl(px(CONTENT_INSET))
            // Trailing tiles align on the glyph like every other control in the
            // window; a label-only header just takes the plain inset.
            .pr(px(if trailing.is_some() {
                CONTENT_INSET - crate::ui::app::TILE_PAD
            } else {
                CONTENT_INSET
            }))
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(cx.theme().muted_foreground)
                    .child(text.to_uppercase()),
            )
            .when_some(trailing, |this, t| this.child(t))
            .into_any_element()
    }

    /// The Files header's one control. No refresh button: the tree runs a
    /// recursive filesystem watcher over its roots and invalidates its own caches,
    /// so a manual refresh is a button that does what already happened.
    fn files_controls(&self, cx: &mut Context<Self>) -> AnyElement {
        let show_hidden = self.file_tree.show_hidden;
        crate::ui::tab_strip::chrome_tile(
            Button::new("panel-tree-hidden").icon(Icon::new(IconName::Eye).size(px(13.))),
            show_hidden,
            cx,
        )
        .xsmall()
        .w(px(24.))
        .h(px(24.))
        .rounded_md()
        .tooltip(if show_hidden {
            "Hide dotfiles"
        } else {
            "Show dotfiles"
        })
        .on_click(cx.listener(|this, _, _w, cx| {
            this.file_tree.show_hidden = !this.file_tree.show_hidden;
            cx.notify();
        }))
        .into_any_element()
    }

    /// The Files tab's filter box — the same borderless magnifier + input the tab
    /// rail uses, so the two panels search the same way. Sits under the header
    /// rather than in it: it's a full-width control, not a trailing tile.
    fn files_search(&self, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .h(px(30.))
            .px(px(CONTENT_INSET))
            .child(
                Icon::new(IconName::Search)
                    .small()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.file_search).appearance(false).xsmall()),
            )
            .into_any_element()
    }

    /// The body's scrolling area, so every tab shares one scroll container and
    /// one content inset.
    fn panel_scroll(&self, inner: AnyElement, title: AnyElement) -> AnyElement {
        v_flex()
            .flex_1()
            .min_h_0()
            .child(title)
            .child(
                div()
                    .id("right-panel-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(inner),
            )
            .into_any_element()
    }

    /// A quiet "nothing to show" line, used wherever a tab has no data yet.
    fn panel_empty(&self, text: &str, cx: &mut Context<Self>) -> AnyElement {
        div()
            .px(px(CONTENT_INSET))
            .py(px(4.))
            .text_size(px(12.))
            .text_color(cx.theme().muted_foreground)
            .child(text.to_string())
            .into_any_element()
    }

    // ── Info ────────────────────────────────────────────────────────────────

    /// Session facts for the active pane, as a two-column key/value list. Every
    /// row comes from an accessor the sidebar already uses, so the panel can
    /// never disagree with the row that spawned it.
    fn render_panel_info(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let title = self.panel_title("Info", None, cx);
        let mut rows: Vec<(&'static str, String)> = Vec::new();
        // Held aside from `rows` because they're not key/value lines: the actions
        // hang off the cwd, and the two lists get their own sub-headers below.
        let mut cwd_for_actions: Option<PathBuf> = None;
        let mut pane_id: Option<u64> = None;

        if let Some(tab) = self.tabs.get(self.active) {
            if let Some(leaf) = tab.detail_pane(window, cx) {
                let view = leaf.read(cx);
                pane_id = Some(view.pane_id);
                if let Some(cwd) = view
                    .git_status_cwd()
                    .map(|p| p.to_path_buf())
                    .or_else(|| view.cwd())
                {
                    rows.push(("cwd", compact_path(&cwd)));
                    cwd_for_actions = Some(cwd);
                }
                let shell = view.shell_spec().map(|s| s.program.clone());
                rows.push((
                    "shell",
                    crate::core::shells::default_shell_name(shell.as_deref()),
                ));
                if let Some(ssh) = view.ssh_spec() {
                    rows.push(("ssh", ssh.host.clone()));
                }
            }
            if let Some(git) = tab.git_status(Some(window), cx) {
                rows.push(("branch", git.branch.clone()));
                rows.push(("changes", format!("+{} −{}", git.added, git.removed)));
            }
            if let Some(agent) = tab.agent(cx) {
                let name = agent.display_name();
                let status = match tab.agent_status(cx) {
                    Some(s) => format!("{name} · {}", agent_status_label(s)),
                    None => name.to_string(),
                };
                rows.push(("agent", status));
            }
        }

        if rows.is_empty() {
            return self.panel_scroll(self.panel_empty("No active session.", cx), title);
        }

        // Keep the process/port query pointed at the pane on screen, and keep it
        // ticking while this tab is the one being looked at.
        self.sync_procs(pane_id, cx);

        let mut list = v_flex().px(px(CONTENT_INSET)).py(px(2.)).gap(px(5.));
        for (k, v) in rows {
            list = list.child(
                h_flex()
                    .items_baseline()
                    .gap(px(8.))
                    .text_size(px(11.5))
                    .child(
                        div()
                            .flex_none()
                            .w(px(52.))
                            .text_color(cx.theme().muted_foreground)
                            .child(k),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(cx.theme().foreground)
                            .child(v),
                    ),
            );
        }

        let inner = v_flex()
            .child(list)
            .when_some(cwd_for_actions, |this, cwd| {
                this.child(self.cwd_actions(cwd, cx))
            })
            .children(self.procs_section(cx))
            .children(self.ports_section(cx))
            .into_any_element();
        self.panel_scroll(inner, title)
    }

    /// The "open this cwd in…" row under the Info list. Deliberately only the
    /// destinations that need no configuration — a system reveal and the
    /// clipboard. An "open in $EDITOR" button would need a picker, a stored
    /// choice and a settings page to change it; that's a feature, not a row.
    fn cwd_actions(&self, cwd: PathBuf, cx: &mut Context<Self>) -> AnyElement {
        let reveal_label = if cfg!(target_os = "macos") {
            "Reveal in Finder"
        } else {
            "Open Folder"
        };
        h_flex()
            .gap(px(2.))
            .px(px(CONTENT_INSET - crate::ui::app::TILE_PAD))
            .pt(px(6.))
            .child(
                crate::ui::tab_strip::chrome_tile(
                    Button::new("panel-info-reveal")
                        .icon(Icon::new(IconName::FolderOpen).size(px(13.))),
                    false,
                    cx,
                )
                .xsmall()
                .w(px(24.))
                .h(px(24.))
                .rounded_md()
                .tooltip(reveal_label)
                .on_click({
                    let cwd = cwd.clone();
                    move |_, _window, cx| cx.reveal_path(&cwd)
                }),
            )
            .child(
                crate::ui::tab_strip::chrome_tile(
                    Button::new("panel-info-copy-path")
                        .icon(Icon::new(IconName::Copy).size(px(13.))),
                    false,
                    cx,
                )
                .xsmall()
                .w(px(24.))
                .h(px(24.))
                .rounded_md()
                .tooltip("Copy Path")
                .on_click(move |_, _window, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                        cwd.display().to_string(),
                    ));
                }),
            )
            .into_any_element()
    }

    /// A small caps divider inside a tab's body, for the sub-lists that hang off
    /// the Info tab. Lighter than [`panel_title`], which is the tab's own header.
    fn panel_subtitle(&self, text: &str, cx: &mut Context<Self>) -> AnyElement {
        div()
            .px(px(CONTENT_INSET))
            .pt(px(12.))
            .pb(px(3.))
            .text_size(px(10.))
            .text_color(cx.theme().muted_foreground)
            .child(text.to_uppercase())
            .into_any_element()
    }

    /// The pane's process tree, indented by depth. Returns nothing at all when
    /// the pane is just a shell sitting at its prompt: a one-row "processes"
    /// section that always says `zsh` is a header earning its keep zero times.
    fn procs_section(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let procs = &self.procs()?.procs;
        if procs.len() < 2 {
            return None;
        }
        let mut list = v_flex().px(px(CONTENT_INSET)).gap(px(1.));
        for p in procs {
            list = list.child(
                h_flex()
                    .items_baseline()
                    .gap(px(6.))
                    .text_size(px(11.5))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            // Indent by depth so the tree reads without drawing
                            // connector glyphs into a 260px column.
                            .pl(px(f32::from(p.depth) * 10.))
                            .text_color(if p.foreground {
                                cx.theme().foreground
                            } else {
                                cx.theme().muted_foreground
                            })
                            .child(p.name.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.5))
                            .text_color(cx.theme().muted_foreground)
                            .child(p.pid.to_string()),
                    ),
            );
        }
        Some(
            v_flex()
                .child(self.panel_subtitle("Processes", cx))
                .child(list)
                .into_any_element(),
        )
    }

    /// TCP ports the pane's processes are listening on — the answer to "what
    /// port did that dev server pick?", next to the pane that started it.
    fn ports_section(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ports = &self.procs()?.ports;
        if ports.is_empty() {
            return None;
        }
        let mut list = v_flex().px(px(CONTENT_INSET)).gap(px(1.));
        for p in ports {
            list = list.child(
                h_flex()
                    .items_baseline()
                    .gap(px(8.))
                    .text_size(px(11.5))
                    .child(
                        div()
                            .flex_none()
                            .w(px(52.))
                            .text_color(cx.theme().foreground)
                            .child(p.port.to_string()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(cx.theme().muted_foreground)
                            .child(p.name.clone()),
                    ),
            );
        }
        Some(
            v_flex()
                .child(self.panel_subtitle("Ports", cx))
                .child(list)
                .into_any_element(),
        )
    }

    /// The cached query, but only when it describes the pane currently on screen.
    fn procs(&self) -> Option<&PaneProcs> {
        self.right_panel.procs.as_ref()
    }

    /// Point the process query at `pane_id` and make sure the poll is running.
    /// Called from the Info tab's render, so the loop starts when the tab is
    /// looked at and dies when it isn't — see [`spawn_procs_query`].
    fn sync_procs(&mut self, pane_id: Option<u64>, cx: &mut Context<Self>) {
        let Some(pane_id) = pane_id else { return };
        if self.right_panel.procs_pane != Some(pane_id) {
            self.right_panel.procs_pane = Some(pane_id);
            // Drop the previous pane's answer rather than showing it under the new
            // pane's heading until the first tick lands.
            self.right_panel.procs = None;
        }
        if !self.right_panel.procs_loading {
            self.spawn_procs_query(pane_id, cx);
        }
    }

    /// One query, then reschedule — the poll loop. It reschedules only while the
    /// panel is open on Info, so the loop is self-terminating: close the panel or
    /// switch tabs and the next completion simply doesn't queue another.
    fn spawn_procs_query(&mut self, pane_id: u64, cx: &mut Context<Self>) {
        self.right_panel.procs_loading = true;
        cx.spawn(async move |this, cx| {
            let procs = cx
                .background_executor()
                .spawn(async move { crate::terminal::RemoteTerminal::query_procs(pane_id) })
                .await;
            let keep_polling = this
                .update(cx, |app, cx| {
                    app.right_panel.procs_loading = false;
                    // A pane switch while we flew makes this answer stale; drop it
                    // and let the new pane's own query land.
                    if app.right_panel.procs_pane != Some(pane_id) {
                        return false;
                    }
                    app.right_panel.procs = Some(procs);
                    cx.notify();
                    let cfg = cx.global::<Config>();
                    cfg.right_panel_visible && cfg.right_panel_tab == RightPanelTab::Info
                })
                .unwrap_or(false);
            if !keep_polling {
                return;
            }
            cx.background_executor().timer(PROCS_POLL).await;
            let _ = this.update(cx, |app, cx| {
                // Re-check rather than trusting the pre-sleep decision: two seconds
                // is plenty of time to close the panel.
                let cfg = cx.global::<Config>();
                let wanted = cfg.right_panel_visible && cfg.right_panel_tab == RightPanelTab::Info;
                if wanted && app.right_panel.procs_pane == Some(pane_id) {
                    app.spawn_procs_query(pane_id, cx);
                }
            });
        })
        .detach();
    }

    // ── Outline ─────────────────────────────────────────────────────────────

    /// The pane's commands, newest first, each scrolling the terminal back to
    /// where it ran. Positions come from the OSC 133 marks the reader thread
    /// records — see [`crate::terminal::marks`].
    ///
    /// Newest first because that's the end you came from: you scrolled past the
    /// thing you want, and the list should start where your attention is.
    fn render_panel_outline(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let title = self.panel_title("Outline", None, cx);
        let Some(leaf) = self
            .tabs
            .get(self.active)
            .and_then(|t| t.detail_pane(window, cx))
        else {
            return self.panel_scroll(self.panel_empty("No active session.", cx), title);
        };
        let marks = leaf.read(cx).command_marks();
        if marks.is_empty() {
            // Two very different causes, one honest sentence: nothing has run
            // yet, or this shell never reported OSC 133 (no integration, a bare
            // `sh`, a nested PTY that eats the marks).
            return self.panel_scroll(
                self.panel_empty("No commands recorded for this pane.", cx),
                title,
            );
        }

        let mut list = v_flex().px(px(CONTENT_INSET - 4.)).py(px(2.)).gap(px(1.));
        for mark in marks.iter().rev() {
            let row = mark.row;
            let leaf = leaf.clone();
            // A command that failed is the one you're most often looking for, so
            // it gets the only color in the list.
            let failed = mark.exit.is_some_and(|c| c != 0);
            list = list.child(
                h_flex()
                    .id(gpui::SharedString::from(format!("panel-mark-{row}")))
                    .items_baseline()
                    .gap(px(6.))
                    .px(px(4.))
                    .py(px(2.))
                    .rounded(px(4.))
                    .text_size(px(11.5))
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().sidebar_accent.opacity(0.55)))
                    .on_click(cx.listener(move |_this, _, _window, cx| {
                        leaf.update(cx, |view, cx| {
                            view.scroll_to_mark(row, cx);
                        });
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(if failed {
                                cx.theme().danger
                            } else {
                                cx.theme().foreground
                            })
                            // Commands wrap in the shell but must not here: one
                            // row per command is what makes the list scannable.
                            .child(one_line(&mark.text)),
                    )
                    // Only nonzero exits earn a badge. Annotating every success
                    // with a `0` would make the failures harder to spot, not
                    // easier — the whole point of the column.
                    .when_some(mark.exit.filter(|c| *c != 0), |this, code| {
                        this.child(
                            div()
                                .flex_none()
                                .text_size(px(10.5))
                                .text_color(cx.theme().danger)
                                .child(code.to_string()),
                        )
                    })
                    // A command still running is worth marking: it's why the
                    // pane is busy.
                    .when(!mark.done, |this| {
                        this.child(
                            div()
                                .flex_none()
                                .text_size(px(10.5))
                                .text_color(cx.theme().muted_foreground)
                                .child("…"),
                        )
                    }),
            );
        }
        self.panel_scroll(list.into_any_element(), title)
    }

    // ── Changes ─────────────────────────────────────────────────────────────

    /// The working-tree diff as a compact file list — path plus `+N −M` — not the
    /// diff overlay's hunk cards, which need far more than 260px to be readable.
    /// Clicking a row opens the full overlay on that repo.
    fn render_panel_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let title = self.panel_title("Changes", None, cx);
        let cwd = self
            .tabs
            .get(self.active)
            .and_then(|t| t.detail_pane(window, cx))
            .and_then(|leaf| {
                let v = leaf.read(cx);
                v.git_status_cwd()
                    .map(|p| p.to_path_buf())
                    .or_else(|| v.cwd())
            });

        let Some(cwd) = cwd else {
            return self.panel_scroll(self.panel_empty("No working directory.", cx), title);
        };
        // Probe on first paint for this cwd, and whenever the pane moves to a
        // different repository. Refreshes ride the same git-status observer the
        // sidebar counts do, which clears the cache (see `right_panel_invalidate`).
        if self.right_panel.diff_cwd.as_ref() != Some(&cwd) {
            self.right_panel.diff_cwd = Some(cwd.clone());
            self.right_panel.diff = None;
            self.spawn_right_panel_diff(cwd.clone(), cx);
        }

        let inner = match &self.right_panel.diff {
            None => self.panel_empty("Loading…", cx),
            Some(None) => self.panel_empty("Not a git work tree.", cx),
            Some(Some(snap)) if snap.files.is_empty() && snap.untracked.is_empty() => {
                self.panel_empty("No changes.", cx)
            }
            Some(Some(snap)) => {
                let files: Vec<(String, u32, u32)> = snap
                    .files
                    .iter()
                    .map(|f| (f.path.clone(), f.added, f.removed))
                    .collect();
                let untracked = snap.untracked.clone();
                let focused = self.diff_overlay_focus(&cwd).map(str::to_string);
                // Rows inset themselves rather than the list, so the hover and
                // selected capsules bleed a little past the text into the same
                // 12px gutter the tab rail's rows use.
                let mut list = v_flex().px(px(CONTENT_INSET - 4.)).py(px(2.)).gap(px(1.));
                for (path, added, removed) in files {
                    let selected = focused.as_deref() == Some(path.as_str());
                    list = list.child(
                        h_flex()
                            .id(gpui::SharedString::from(format!("panel-change-{path}")))
                            .items_baseline()
                            .gap(px(8.))
                            .px(px(4.))
                            .py(px(2.))
                            .rounded(px(4.))
                            .text_size(px(11.5))
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().sidebar_accent.opacity(0.55)))
                            .when(selected, |s| s.bg(cx.theme().sidebar_accent))
                            .on_click({
                                let cwd = cwd.clone();
                                let path = path.clone();
                                cx.listener(move |this, _, window, cx| {
                                    // Toggling on the same row closes the overlay,
                                    // so a row is a switch for "show me this diff",
                                    // not a one-way door.
                                    this.toggle_diff_overlay_at(
                                        cwd.clone(),
                                        Some(path.clone()),
                                        window,
                                        cx,
                                    );
                                })
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(cx.theme().foreground)
                                    .child(path),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(cx.theme().success)
                                    .child(format!("+{added}")),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(cx.theme().danger)
                                    .child(format!("−{removed}")),
                            ),
                    );
                }
                if !untracked.is_empty() {
                    list = list.child(
                        div()
                            .pt(px(4.))
                            .px(px(4.))
                            .text_size(px(11.))
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{} untracked", untracked.len())),
                    );
                }
                list.into_any_element()
            }
        };
        self.panel_scroll(inner, title)
    }

    /// Off-thread `git diff` for the panel, mirroring the diff overlay's probe.
    fn spawn_right_panel_diff(&mut self, cwd: PathBuf, cx: &mut Context<Self>) {
        if self.right_panel.diff_loading {
            return;
        }
        self.right_panel.diff_loading = true;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let cwd = cwd.clone();
                    async move { git_diff::probe(&cwd) }
                })
                .await;
            let _ = this.update(cx, |app, cx| {
                app.right_panel.diff_loading = false;
                // Drop the result if the panel moved on to another repo while we
                // flew — otherwise a slow probe would overwrite a newer one.
                if app.right_panel.diff_cwd.as_ref() == Some(&cwd) {
                    app.right_panel.diff = Some(result);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Drop the cached diff so the next paint re-probes. Called from the same
    /// git-status observer that refreshes the sidebar's `+N −M`.
    pub(crate) fn right_panel_invalidate(&mut self) {
        self.right_panel.diff_cwd = None;
    }

    // ── Files ───────────────────────────────────────────────────────────────

    /// The project tree, reusing the code panel's rows verbatim — same expand
    /// state, same click-to-open, so the panel and the editor overlay are two
    /// views of one tree rather than two trees.
    fn render_panel_files(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let controls = self.files_controls(cx);
        let title = self.panel_title("Files", Some(controls), cx);
        let search = self.files_search(cx);
        let rows = self.render_file_tree_rows(window, cx);
        v_flex()
            .flex_1()
            .min_h_0()
            .child(title)
            .child(search)
            .child(rows)
            .into_any_element()
    }
}

/// The one-word status the Info row shows next to the agent's name.
fn agent_status_label(status: crate::core::cli_agent::AgentStatus) -> &'static str {
    use crate::core::cli_agent::AgentStatus::*;
    match status {
        Idle => "idle",
        Working => "working",
        Waiting => "waiting",
        Done => "done",
    }
}

/// Flatten a possibly-multiline command to one row: newlines and tabs become
/// spaces, runs of whitespace collapse. A heredoc or a `for` loop typed across
/// lines is still recognizable, and the list keeps one row per command.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `~`-shorten a path for the Info list, which has ~180px to play with.
fn compact_path(path: &std::path::Path) -> String {
    let s = path.to_string_lossy().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && s.starts_with(&home) => s.replacen(&home, "~", 1),
        _ => s,
    }
}
