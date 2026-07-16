//! The tab strip rendered into the title bar: one chip per tab (context icon,
//! label, close affordance), inline rename, drag-to-reorder, and the "+"
//! new-tab button. Split out of `app.rs` as an `impl Tty7App` block (the same
//! pattern `settings` uses) so the window-shell file stays focused on tab/pane
//! orchestration rather than chrome rendering.

use gpui::{
    App, Axis, Context, FontWeight, MouseButton, MouseDownEvent, SharedString, Window, div,
    prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::Input;
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex};

use crate::core::actions::{OpenSettings, TogglePalette};
use crate::core::config::Config;
use crate::daemon::protocol::ShellSpec;
use crate::ui::app::{Tab, Tty7App};
use crate::ui::hints::tab_badge_label;

/// How many trailing path components a deep tab label keeps, mirroring
/// ghostty's zsh integration title `%(4~|…/%3~|%~)`: a path deeper than this
/// collapses to `…/` plus its last three components; a shallower one shows in
/// full. The home directory abbreviates to `~`.
const KEEP_SEGMENTS: usize = 3;

/// Abbreviate a leading `$HOME` to `~` (an integrated shell usually already
/// does this, but absolute paths from other shells won't be). Borrows when
/// there's nothing to rewrite.
pub(crate) fn abbreviate_home(path: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if path.starts_with('~') {
        return Cow::Borrowed(path);
    }
    let Some(home) = std::env::var_os("HOME") else {
        return Cow::Borrowed(path);
    };
    let home = home.to_string_lossy();
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return Cow::Borrowed(path);
    }
    if path == home {
        return Cow::Owned("~".to_string());
    }
    match path.strip_prefix(home) {
        Some(rest) if rest.starts_with('/') => Cow::Owned(format!("~{rest}")),
        _ => Cow::Borrowed(path),
    }
}

/// Derive a short tab label from a terminal's raw title.
///
/// Shells emit OSC titles like `user@host:~/projects/app`; we show the tail the
/// way ghostty does — the working directory abbreviated with `~`, trimmed to
/// its last few components (`…/repo/025/tty7`) once it runs deep. We strip any
/// `user@host:` prefix first; a non-path title (a running command) passes
/// through unchanged.
fn short_title(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    // Drop a leading `user@host:` if present (only when it precedes a path).
    let after_host = match raw.split_once(':') {
        Some((head, tail)) if head.contains('@') => tail,
        _ => raw,
    };
    let after_host = after_host.trim();
    if after_host.is_empty() {
        return String::new();
    }
    let abbreviated = abbreviate_home(after_host);
    let path: &str = abbreviated.as_ref();

    // Classify the leading marker so it can be counted toward depth (like `~` in
    // zsh's `%N~`) but dropped when the path is truncated.
    enum Kind {
        Home,
        Absolute,
        Relative,
    }
    let (kind, body) = if let Some(rest) = path.strip_prefix("~/") {
        (Kind::Home, rest)
    } else if path == "~" {
        return "~".to_string();
    } else if let Some(rest) = path.strip_prefix('/') {
        (Kind::Absolute, rest)
    } else {
        (Kind::Relative, path)
    };

    let segments: Vec<&str> = body.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return match kind {
            Kind::Home => "~",
            Kind::Absolute => "/",
            Kind::Relative => "",
        }
        .to_string();
    }

    // `~` counts as one component in ghostty's depth test (`%(4~|…|…)`).
    let depth = segments.len() + usize::from(matches!(kind, Kind::Home));
    let mut label = if depth > KEEP_SEGMENTS {
        // Deep path: ellipsis plus the trailing components, leading marker dropped.
        let tail = &segments[segments.len() - KEEP_SEGMENTS..];
        format!("…/{}", tail.join("/"))
    } else {
        match kind {
            Kind::Home => format!("~/{}", segments.join("/")),
            Kind::Absolute => format!("/{}", segments.join("/")),
            Kind::Relative => segments.join("/"),
        }
    };
    // Final safety clamp for an unusually long single component.
    if label.chars().count() > 40 {
        label = format!("{}…", label.chars().take(40).collect::<String>());
    }
    label
}

/// Drag payload for reordering tabs. Carries the source index and a label so the
/// drag preview can show the tab being moved. `pub(crate)` so the vertical
/// [`tab_sidebar`](crate::ui::tab_sidebar) reuses the same payload (and could one
/// day support strip ↔ sidebar cross-drops via the shared `move_tab`).
#[derive(Clone)]
pub(crate) struct DragTab {
    pub(crate) index: usize,
    pub(crate) label: SharedString,
}

impl Render for DragTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_1()
            .rounded_lg()
            .bg(cx.theme().secondary)
            .border_1()
            .border_color(cx.theme().border)
            .text_sm()
            .text_color(cx.theme().foreground)
            .child(self.label.clone())
    }
}

impl Tty7App {
    /// The status dot pinned to a tab avatar's bottom-right corner (an agent's
    /// live status, or an SSH pane's connection phase): a solid
    /// `rgb` disc with a surface-colored separator ring so it reads as sitting
    /// on the badge. `unread` is how many panes hold a finished turn you
    /// haven't looked at: when nonzero, the dot swells into a count badge —
    /// the same disc grown just enough to speak its number — so read↔unread
    /// stays one element opening its mouth, not a second indicator appearing.
    /// `size` is the avatar edge.
    fn status_dot(rgb: u32, unread: usize, size: f32, cx: &App) -> gpui::AnyElement {
        let d = (size * 0.42).max(7.);
        let bg = cx.theme().background;
        if unread > 0 {
            // The count badge: sized to seat a digit legibly, centred on the
            // read dot's centre (same corner point) so the swell reads as the
            // dot growing in place rather than a new element popping up.
            let nd = (size * 0.72).max(13.0);
            // Panes per tab are single-digit in practice; clamp so an absurd
            // split can never overflow the disc.
            let label = unread.min(9).to_string();
            div()
                .absolute()
                .right(px(-(nd - d) / 2.0 - d * 0.22))
                .bottom(px(-(nd - d) / 2.0 - d * 0.22))
                .size(px(nd))
                .rounded_full()
                .border_1()
                .border_color(bg)
                .bg(gpui::rgb(rgb))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px((nd * 0.62).round()))
                .font_weight(FontWeight::BOLD)
                .text_color(gpui::white())
                .child(label)
                .into_any_element()
        } else {
            div()
                .absolute()
                .right(px(-(d * 0.22)))
                .bottom(px(-(d * 0.22)))
                .size(px(d))
                .rounded_full()
                .border_2()
                .border_color(bg)
                .bg(gpui::rgb(rgb))
                .into_any_element()
        }
    }

    /// The leading avatar for a tab row/chip: a rounded badge that brands the
    /// tab by what's running in it — each session fronted with an icon. A
    /// recognized coding agent gets its brand mark — a white silhouette
    /// (gpui tints SVGs as an alpha mask) on the vendor accent; a plain shell
    /// gets a neutral terminal glyph. Live status rides the corner as a
    /// [`status_dot`](Self::status_dot) — the agent's working/waiting/done, or
    /// an SSH pane's connection phase (`ssh`) — one corner-dot language for
    /// the whole avatar column. `size` is the badge's edge in px.
    pub(crate) fn tab_avatar(
        &self,
        agent: Option<crate::core::cli_agent::CLIAgent>,
        status: Option<crate::core::cli_agent::AgentStatus>,
        unread: usize,
        ssh: Option<u32>,
        size: f32,
        cx: &App,
    ) -> gpui::AnyElement {
        let base = div()
            .flex_shrink_0()
            .size(px(size))
            .flex()
            .items_center()
            .justify_center();
        // A circle for every kind — the brand mark / glyph sits
        // small and centred with generous padding rather than filling the badge.
        match agent {
            Some(agent) => {
                // The agent's live status as a small dot pinned to the badge's
                // bottom-right corner (blue working / amber waiting / green
                // done), ringed in the surface color so it reads as sitting on
                // the badge rather than clipped by it. Idle (or unknown) draws
                // no dot — a resting agent is just its brand mark. *Unread*
                // finished turns swell the dot into a count badge ("2 results
                // waiting") that shrinks back to a plain dot once you view the
                // panes, without ever hiding the done state.
                let dot = status
                    .and_then(|s| s.dot_rgb())
                    .map(|rgb| Self::status_dot(rgb, unread, size, cx));
                base.relative()
                    .rounded_full()
                    .bg(gpui::rgb(agent.accent_rgb()))
                    .child(
                        gpui::svg()
                            .path(agent.icon_path())
                            .size(px(size * 0.54))
                            .text_color(gpui::white()),
                    )
                    .when_some(dot, |b, dot| b.child(dot))
                    .into_any_element()
            }
            None => base
                .relative()
                .rounded_full()
                // A clearly-visible neutral disc (a neutral grey shell badge), not a
                // near-transparent tint — so the avatar column reads as a column.
                .bg(cx.theme().muted)
                .child(
                    // A flush `>_` prompt (not the boxed `square-terminal`) so it
                    // fills the badge at the same visual weight as a brand mark.
                    gpui::svg()
                        .path("icons/terminal.svg")
                        .size(px(size * 0.56))
                        .text_color(cx.theme().foreground.opacity(0.65)),
                )
                // SSH connection phase as a corner status dot — the same
                // element as an agent's, not a border ring around the badge
                // (a ring read as a second, differently-shaped avatar style).
                .when_some(ssh, |b, rgb| b.child(Self::status_dot(rgb, 0, size, cx)))
                .into_any_element(),
        }
    }

    /// The display label for a tab: the user-set name if present, otherwise the
    /// focused terminal's title (shortened), falling back to
    /// "Session N" when there's no title yet. Pass `window` so the label tracks
    /// the focused pane in a split; `None` (no window available) uses the first
    /// leaf.
    pub(crate) fn tab_label(
        &self,
        tab: &Tab,
        index: usize,
        window: Option<&Window>,
        cx: &App,
    ) -> String {
        if let Some(name) = tab.name.as_ref() {
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        let raw = tab.leaf_title(window, cx);
        let label = short_title(&raw);
        if label.trim().is_empty() {
            format!("Session {}", index + 1)
        } else {
            label
        }
    }

    /// Attach the "new tab" shell picker to a button: the configured default
    /// shell leads the menu (tagged `default`), followed by every shell detected
    /// on this machine; clicking one opens a tab on that shell. Extracted so the
    /// title-bar strip's "+" and the vertical [`tab_sidebar`] share one menu
    /// definition rather than duplicating the shell iteration.
    ///
    /// [`tab_sidebar`]: crate::ui::tab_sidebar
    pub(crate) fn attach_new_tab_menu(
        &self,
        button: Button,
        cx: &Context<Self>,
    ) -> impl IntoElement + use<> {
        let shells = self.detected_shells.clone();
        let default_name = crate::core::shells::default_shell_name(
            cx.global::<Config>()
                .shell
                .as_ref()
                .map(|s| s.program.as_str()),
        );
        let app = cx.entity().downgrade();
        button.dropdown_menu(move |menu, _window, _cx| {
            let mut menu = menu.min_w(px(220.));
            // One row per detected shell. There's no separate "New Tab (…)"
            // entry — it only duplicated the default shell's row, and ⌘T already
            // opens a default tab in one press. The configured default is tagged
            // instead so the menu still says which shell a bare new tab would use.
            for shell in &shells {
                let spec = ShellSpec {
                    program: shell.program.clone(),
                    args: shell.args.clone(),
                };
                let open = app.clone();
                let item = if shell.label == default_name {
                    let label: SharedString = shell.label.clone().into();
                    PopupMenuItem::element(move |_window, cx| {
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(label.clone())
                            .child(
                                div()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("default"),
                            )
                    })
                } else {
                    PopupMenuItem::new(shell.label.clone())
                };
                menu = menu.item(item.on_click(move |_, window, cx| {
                    if let Some(app) = open.upgrade() {
                        app.update(cx, |this, cx| {
                            this.new_tab_with_shell(Some(spec.clone()), window, cx);
                        });
                    }
                }));
            }
            // Before shell detection lands (or if it finds nothing), keep a
            // single default entry so the menu is never empty.
            if shells.is_empty() {
                let open_default = app.clone();
                menu = menu.item(
                    PopupMenuItem::new("New Tab").on_click(move |_, window, cx| {
                        if let Some(app) = open_default.upgrade() {
                            app.update(cx, |this, cx| this.new_tab(window, cx));
                        }
                    }),
                );
            }
            menu
        })
    }

    /// Build the per-tab right-click menu, shared by the strip's chips and the
    /// sidebar's rows (which passes `below_wording` so the trailing close reads
    /// "Close Tabs Below" in the vertical list). Live state — tab count, the
    /// tab's cwd — is read at open time through the weak `app` handle, so the
    /// render loop never pays a per-frame cwd syscall and the enablement can't
    /// go stale between render and click.
    pub(crate) fn tab_context_menu(
        menu: PopupMenu,
        index: usize,
        below_wording: bool,
        app: &gpui::WeakEntity<Self>,
        window: &Window,
        cx: &App,
    ) -> PopupMenu {
        let Some(entity) = app.upgrade() else {
            return menu;
        };
        let this = entity.read(cx);
        let tab_count = this.tabs.len();
        let cwd = this.tab_cwd(index, window, cx);
        let has_cwd = cwd.is_some();
        let mut menu = menu.min_w(px(200.));

        // Rename — the same inline edit a label double-click starts, given a
        // discoverable entry point.
        menu = menu.item(PopupMenuItem::new("Rename Tab").on_click({
            let app = app.clone();
            move |_, window, cx| {
                let _ = app.update(cx, |this, cx| this.start_rename(index, window, cx));
            }
        }));

        // Worktree: an isolated checkout of this tab's repo on a fresh branch,
        // opened as a new tab — parallel-agent fuel. Only offered when the
        // tab's cwd actually sits in a git repository (a filesystem-only
        // probe, cheap enough at open time); outside one the entry would be
        // pure noise.
        let in_repo = cwd
            .as_deref()
            .is_some_and(crate::core::worktree::is_inside_repo);
        if in_repo {
            menu = menu
                .separator()
                .item(PopupMenuItem::new("New Worktree Tab").on_click({
                    let app = app.clone();
                    move |_, window, cx| {
                        let _ = app.update(cx, |this, cx| this.new_worktree_tab(index, window, cx));
                    }
                }));
        }

        // Splits act on the right-clicked tab: activate it first (a no-op when
        // it already is), then split its focused pane — one code path with the
        // keyboard actions.
        menu = menu
            .separator()
            .item(PopupMenuItem::new("Split Right").on_click({
                let app = app.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| {
                        this.activate(index, window, cx);
                        this.split(Axis::Horizontal, window, cx);
                    });
                }
            }))
            .item(PopupMenuItem::new("Split Down").on_click({
                let app = app.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| {
                        this.activate(index, window, cx);
                        this.split(Axis::Vertical, window, cx);
                    });
                }
            }));

        menu = menu.separator().item(
            PopupMenuItem::new("Copy Working Directory")
                .disabled(!has_cwd)
                .on_click(move |_, _window, cx| {
                    if let Some(cwd) = cwd.as_ref() {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                            cwd.display().to_string(),
                        ));
                    }
                }),
        );

        menu.separator()
            .item(PopupMenuItem::new("Close Tab").on_click({
                let app = app.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| this.close_tab(index, window, cx));
                }
            }))
            .item(
                PopupMenuItem::new("Close Other Tabs")
                    .disabled(tab_count <= 1)
                    .on_click({
                        let app = app.clone();
                        move |_, window, cx| {
                            let _ =
                                app.update(cx, |this, cx| this.close_other_tabs(index, window, cx));
                        }
                    }),
            )
            .item(
                PopupMenuItem::new(if below_wording {
                    "Close Tabs Below"
                } else {
                    "Close Tabs to the Right"
                })
                .disabled(index + 1 >= tab_count)
                .on_click({
                    let app = app.clone();
                    move |_, window, cx| {
                        let _ =
                            app.update(cx, |this, cx| this.close_tabs_right_of(index, window, cx));
                    }
                }),
            )
    }

    /// The horizontal tab strip rendered into the title bar. `show_chips` draws
    /// the per-tab chip row; passing `false` (the vertical-sidebar mode, where
    /// the sidebar owns the tab list) keeps only the "+" and "⋯" chrome so the
    /// title bar isn't left empty.
    pub(crate) fn tab_strip(
        &self,
        show_chips: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let active = self.active;
        // While the bare ⌘/Ctrl hold is armed (see `ui::hints`), each of the
        // first nine chips swaps its close affordance for a ⌘N badge — same
        // slot, so nothing shifts when the hints appear.
        let show_badges = self.mod_hint_badges;
        // Explicit viewport-derived strip width, NOT `w_full`: the title bar sizes
        // its content by intrinsic width, so `w_full` doesn't track the window and
        // the strip's right edge (where the "⋯" is pinned) lags behind the
        // shrinking window — the button drifts right into the corner. Deriving the
        // width from the live viewport makes the right edge track the window at
        // every size. macOS reserves 80px on the *left* for the traffic lights, so
        // `viewport - 80` reaches the true right edge and the strip's own `pr`
        // sets the "⋯" inset; other platforms put the window controls on the
        // *right*, so keep the strip narrower to clear them.
        //
        // The non-macOS reserve must cover everything the TitleBar lays out
        // *beside* the strip, or the strip overruns the bar and shoves the native
        // close button off the corner: 12px of `TitleBar` left padding + the three
        // 34px window-control tiles (─ ▢ ✕ = 102px) = 114px. Undershooting it (the
        // old 100px) left the strip ~14px too wide, clipping the "✕".
        let strip_w = if cfg!(target_os = "macos") {
            (window.viewport_size().width - px(80.)).max(px(160.))
        } else {
            (window.viewport_size().width - px(114.)).max(px(140.))
        };
        // The "+" and the right-edge overflow "⋯" (30px each), their surrounding
        // gaps, and the strip's own left/right padding all live *outside* the
        // clipped chip row — reserve that whole footprint here so the fixed chrome
        // never overflows the strip box (which would eat the "⋯"'s right inset and
        // shove it into the window corner) and cap the chip row at the remainder.
        let chips_avail = (strip_w - px(100.)).max(px(80.));
        // Only the chip row clips; a crowded row shrinks its chips (down to their
        // `min_w`) and truncates their labels rather than pushing the "+" away.
        let mut chips = h_flex()
            .items_center()
            .gap_1p5()
            .min_w_0()
            .max_w(chips_avail)
            .overflow_hidden();

        for (i, tab) in self.tabs.iter().enumerate() {
            // In sidebar mode the vertical rail carries the tab list; the strip
            // keeps only its "+"/"⋯" chrome, so skip the chip row entirely.
            if !show_chips {
                break;
            }
            let is_active = i == active;
            let label = self.tab_label(tab, i, Some(window), cx);
            // SSH status dot (PRD FR-E2): coloured by the pane's connection phase.
            let ssh_dot = self.tab_ssh_dot(tab, cx);
            // A coding agent running in this tab (Claude Code, Codex, …) fronts
            // its chip with the vendor brand mark so it's recognizable at a
            // glance across a crowded strip.
            let agent = tab.agent(cx);
            let agent_status = tab.agent_status(cx);
            let agent_unread = tab.agent_unread_count(cx);

            // Inline rename input for this tab, if it's the one being renamed.
            let rename_input = self
                .renaming
                .as_ref()
                .filter(|r| r.index == i)
                .map(|r| r.input.clone());
            // Clean label (no pane-count suffix) for the rename prefill / drag preview.
            let drag_label: SharedString = label.clone().into();

            // Either the editable input (while renaming) or the clickable,
            // draggable label.
            let label_region = match rename_input {
                Some(input) => div()
                    .id(("tab-rename", i))
                    .flex_1()
                    .min_w_0()
                    // Swallow mouse-downs (incl. double-click word-select inside
                    // the input) so they never reach the enclosing TitleBar and
                    // zoom/maximize the window.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(Input::new(&input).appearance(false))
                    .into_any_element(),
                None => div()
                    .id(("tab-label", i))
                    .flex_1()
                    .min_w_0()
                    // Ellipsis-truncate the label so a shrunken chip degrades
                    // gracefully instead of hard-clipping mid-glyph.
                    .truncate()
                    .text_sm()
                    // Active tab carries a hair more weight so hierarchy reads
                    // from the type, not from colour alone.
                    .when(is_active, |d| d.font_weight(FontWeight::MEDIUM))
                    .child(label)
                    // Single click activates; double click starts a rename.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                            // Swallow the event so it never reaches the enclosing
                            // TitleBar, whose double-click handler would otherwise
                            // zoom/maximize the window on a rename double-click.
                            cx.stop_propagation();
                            if ev.click_count >= 2 {
                                this.start_rename(i, window, cx);
                            } else {
                                this.activate(i, window, cx);
                            }
                        }),
                    )
                    // Drag the tab by its label to reorder it.
                    .on_drag(
                        DragTab {
                            index: i,
                            label: drag_label.clone(),
                        },
                        |drag, _, _, cx| {
                            cx.stop_propagation();
                            cx.new(|_| drag.clone())
                        },
                    )
                    .into_any_element(),
            };

            let chip = h_flex()
                .id(("tab-chip", i))
                // The strip lives inside gpui-component's `TitleBar`, which marks
                // its whole area as `WindowControlArea::Drag`. On Windows that maps
                // to `HTCAPTION`, so unless an element on top registers a
                // mouse-blocking hitbox, the OS swallows clicks as window-drags and
                // our `on_mouse_down` never fires. `occlude()` makes the chip a
                // `BlockMouse` hitbox so hit-testing stops here (its label/close
                // children paint above it, so they still click through). No-op on
                // macOS, where titlebar dragging doesn't gate child hit-testing.
                .occlude()
                // A group so this chip's close affordance can reveal on hover
                // (progressive disclosure) without affecting sibling tabs.
                .group(SharedString::from(format!("tab-chip-{i}")))
                .items_center()
                .justify_between()
                .gap_1p5()
                .h(px(30.))
                // Content-adaptive width with a readable floor. The three inputs
                // that should decide a chip's width all flow through flexbox: its
                // own label length (the flex basis is the content), the other
                // tabs' lengths, and the window. A short label ("~") sits at the
                // floor; a longer one grows to fit — with no pixel cap on top, so
                // a wide window with few tabs shows labels in full (the only upper
                // bound is `short_title`'s 40-char clamp). When the row overflows,
                // `flex_shrink` trims every chip in proportion to its basis (the
                // longest give up the most) down to `min_w`, which is the shrink
                // floor too — kept modest so a crowded strip stays readable rather
                // than collapsing to slivers, and so plenty of tabs fit first.
                .min_w(px(100.))
                .flex_shrink(1.)
                .pl_3()
                .pr_1p5()
                .rounded_lg()
                // Active tab: a soft lifted fill, no border — reads native
                // (Safari/Arc) rather than as a hard-edged box. Inactive: quiet
                // muted text with a barely-there fill on hover for feedback.
                .when(is_active, |s| {
                    s.bg(cx.theme().secondary).text_color(cx.theme().foreground)
                })
                .when(!is_active, |s| {
                    s.text_color(cx.theme().muted_foreground)
                        .hover(|s| s.bg(cx.theme().muted))
                })
                // Drop target: dropping a dragged tab here moves it to this slot.
                .drag_over::<DragTab>(|s, _, _, cx| s.bg(cx.theme().drag_border.opacity(0.2)))
                .on_drop(cx.listener(move |this, drag: &DragTab, _window, cx| {
                    this.move_tab(drag.index, i, cx);
                }))
                // A click anywhere on the chip activates the tab. Clicks on the
                // label or close button are handled by those children (which stop
                // propagation), so this fires for the rest — icon, padding, the
                // bare chip — making the whole tab a switch target, not just text.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.activate(i, window, cx);
                    }),
                )
                // Leading SSH status dot when this tab hosts an SSH session.
                .when_some(ssh_dot, |c, rgb| {
                    c.child(
                        div()
                            .flex_shrink_0()
                            .size(px(6.))
                            .rounded_full()
                            .bg(gpui::rgb(rgb)),
                    )
                })
                // Leading agent brand avatar, when a coding agent runs in this
                // tab — the vendor mark on its accent. Only agents get an avatar
                // here: ordinary shells stay text-only so the strip reads as
                // tabs, not icon-per-chip busy.
                .when_some(agent, |chip, agent| {
                    chip.child(self.tab_avatar(
                        Some(agent),
                        agent_status,
                        agent_unread,
                        None,
                        18.,
                        cx,
                    ))
                })
                // Clickable / editable label region.
                .child(label_region)
                // Trailing slot: normally the close affordance — kept out of the
                // way (opacity 0) on every chip, active or not, and fades in on
                // chip hover, so a row of tabs reads clean instead of
                // three-icons-per-chip busy. Space is reserved either way, so
                // nothing shifts on hover. While the shortcut hints are armed,
                // the same slot shows the tab's ⌘N badge instead.
                .child(if show_badges && i < 9 {
                    // Bare digit, no keycap box — the hint blends into the chip
                    // rather than reading as another button. Sized to the exact
                    // 20px square of the close button it stands in for, so the
                    // swap can never change the chip's width (an ellipsized
                    // label would otherwise reflow and the strip would jitter).
                    div()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(20.))
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(if is_active {
                            cx.theme().foreground
                        } else {
                            cx.theme().muted_foreground
                        })
                        .child(tab_badge_label(i))
                        .into_any_element()
                } else {
                    div()
                        .flex_shrink_0()
                        .opacity(0.)
                        .group_hover(SharedString::from(format!("tab-chip-{i}")), |s| {
                            s.opacity(1.)
                        })
                        .child(
                            Button::new(("tab-close", i))
                                .icon(IconName::Close)
                                .ghost()
                                .xsmall()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.close_tab(i, window, cx);
                                })),
                        )
                        .into_any_element()
                });

            // Per-tab right-click menu (rename / worktree / split / copy cwd /
            // close group) — the same builder the sidebar rows use.
            let menu_app = cx.entity().downgrade();
            chips = chips.child(chip.context_menu(move |menu, window, cx| {
                Self::tab_context_menu(menu, i, false, &menu_app, window, cx)
            }));
        }

        // "+" new-tab button — click opens the shell picker. The default shell
        // leads the menu (so the common case is two quick clicks on the same
        // spot; ⌘T still opens a default tab in one), followed by every shell
        // discovered on this machine (`detected_shells`, probed at startup).
        // Built on gpui-component's `DropdownMenu`, which is only implemented
        // for `Button` — hence a ghost Button restyled to the title bar's 30px
        // tile rhythm (30px box, 15px glyph, soft corners) rather than the
        // hand-rolled tile the "+" used to be.
        let add_button =
            // Same Windows titlebar note as the chips above: `occlude()` gives
            // the trigger a BlockMouse hitbox so the TitleBar's HTCAPTION drag
            // area doesn't swallow the click.
            div().occlude().flex_shrink_0().child(
                self.attach_new_tab_menu(
                    Button::new("tab-add")
                        .icon(Icon::new(IconName::Plus).size(px(15.)))
                        .ghost()
                        .xsmall()
                        .w(px(30.))
                        .h(px(30.))
                        .rounded_lg(),
                    cx,
                ),
            );

        // Right-edge overflow menu: the low-frequency *global* entries (command
        // palette, settings) that until now had no on-screen affordance at all —
        // only keyboard shortcuts. Same ghost 30px tile as the "+", but anchored
        // to the title bar's otherwise-empty right edge and opening from its
        // top-right corner so the popup never spills off-screen.
        //
        // `.menu(label, Action)` dispatches the real action, so a click and the
        // shortcut travel one path and the row auto-renders the shortcut hint; it
        // needs an `action_context` inside the app's element tree to land on the
        // root `on_action` handlers, so we hand it the focused pane (falling back
        // to the home page's handle when no tab is open).
        // (Settings is a full-window overlay now, so it simply covers this menu
        // while open — no need to conditionally hide it.)
        let action_ctx = self
            .tabs
            .get(active)
            .and_then(|t| t.pane.focused_or_first(window, cx))
            .map(|leaf| leaf.read(cx).focus_handle.clone())
            .unwrap_or_else(|| self.home_focus.clone());
        let menu_button = div().occlude().flex_shrink_0().child(
            Button::new("titlebar-menu")
                .icon(Icon::new(IconName::Ellipsis).size(px(15.)))
                .ghost()
                .xsmall()
                .w(px(30.))
                .h(px(30.))
                .rounded_lg()
                .dropdown_menu_with_anchor(gpui::Anchor::TopRight, move |menu, _window, _cx| {
                    menu.min_w(px(220.))
                        .action_context(action_ctx.clone())
                        .menu("Command Palette", Box::new(TogglePalette))
                        .menu("Settings…", Box::new(OpenSettings))
                }),
        );

        // Outer strip: the clipping chip row and the always-visible "+" anchored
        // left, the overflow "⋯" pushed to the right edge by a flexible spacer.
        // Only `chips` is width-capped and `overflow_hidden`, so neither button is
        // pushed off-screen no matter how many tabs are open.
        h_flex()
            .items_center()
            .gap_1p5()
            // Chip mode: viewport-derived width (see `strip_w`) so the right edge —
            // and the "⋯" pinned to it — tracks the window instead of drifting.
            // Sidebar mode: the strip lives in the narrower right column beside the
            // rail, so it just fills that column (`w_full`) and the "⋯" pins to its
            // right; the viewport width would overrun the column and push it off.
            .when(show_chips, |this| this.w(strip_w))
            .when(!show_chips, |this| this.w_full())
            // Padding, not margin: taffy is border-box, so a horizontal *margin*
            // would push the strip past its box and clip the "⋯"; padding stays
            // inside the width. `pr_2` (8px) sets the "⋯"'s gap from the right edge
            // — the original tight inset, which now holds steady on resize since
            // `strip_w` keeps the right edge tracking the window.
            .pl_0()
            .pr_2()
            // On Windows/Linux the window controls (─ ▢ ✕) sit on the right, right
            // where the "⋯" lands; give it extra right breathing room there so it
            // reads as a menu affordance, not a fourth window control.
            .when(!cfg!(target_os = "macos"), |this| this.pr_3())
            .min_w_0()
            .child(chips)
            // In sidebar mode the rail owns "New Tab" (a "+" in its own top bar),
            // so the title bar drops its "+" to avoid a redundant second one —
            // leaving just the "⋯" overflow menu on a thin strip.
            .when(show_chips, move |this| this.child(add_button))
            .child(div().flex_1())
            .child(menu_button)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_title_strips_user_host_and_shows_shallow_path_in_full() {
        // Up to KEEP_SEGMENTS deep (home `~` counts as one) shows in full.
        assert_eq!(short_title("user@host:~/projects/app"), "~/projects/app");
        assert_eq!(short_title("/usr/local/bin"), "/usr/local/bin");
        assert_eq!(short_title("plain"), "plain");
    }

    #[test]
    fn short_title_truncates_deep_paths_to_trailing_segments() {
        // Deeper than KEEP_SEGMENTS collapses to `…/` plus the last three.
        assert_eq!(short_title("user@host:~/repo/025/tty7"), "…/repo/025/tty7");
        assert_eq!(short_title("/usr/local/share/man"), "…/local/share/man");
        assert_eq!(short_title("a/b/c/d"), "…/b/c/d");
    }

    #[test]
    fn short_title_keeps_home_tilde_and_normalizes_trailing_slash() {
        assert_eq!(short_title("user@host:~"), "~");
        assert_eq!(short_title("~"), "~");
        // Trailing slash is dropped; the path is shown, not just its basename.
        assert_eq!(short_title("a/b/c/"), "a/b/c");
    }

    #[test]
    fn short_title_blank_input_is_empty_and_long_names_are_clamped() {
        assert_eq!(short_title("   "), "");
        let long = "a".repeat(50);
        let out = short_title(&long);
        // Clamp is 40 chars plus a single ellipsis.
        assert_eq!(out.chars().count(), 41);
        assert!(out.ends_with('…'));
    }
}
