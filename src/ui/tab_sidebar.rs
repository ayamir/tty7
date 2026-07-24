//! The vertical tab sidebar: the left-side alternative to the horizontal
//! [`tab_strip`](crate::ui::tab_strip), shown when `tab_bar_position` is `left`.
//! One full-width row per tab — label, inline rename, drag-to-reorder, hover
//! close — under a search + new-tab control bar at the top of the rail.
//!
//! Split out of `app.rs` as an `impl Tty7App` block, exactly like `tab_strip`.
//! It shares the model wholesale: the same `self.tabs`/`self.active` state, the
//! same `tab_label`, the same `activate`/`close_tab`/`move_tab`/`start_rename`
//! operations, the same `DragTab` payload, and the same theme tokens the chips
//! use — so the vertical list stays pixel-consistent with the strip and adds no
//! new state or business logic, only a new set of click targets in a new shape.

use gpui::{
    AnyElement, Bounds, Context, FontWeight, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, SharedString, Window, canvas, div, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::Input;
use gpui_component::menu::ContextMenuExt as _;
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use std::cell::Cell;
use std::rc::Rc;

use std::path::PathBuf;

use crate::core::config::{Config, SidebarGrouping};
use crate::terminal::git_status::GitStatusCache;
use crate::ui::app::{TITLE_BAR_HEIGHT, Tty7App};
use crate::ui::hints::tab_badge_label;
use crate::ui::tab_strip::DragTab;

/// Minimum sidebar width, and the maximum as a fraction of the window width, so
/// a resize drag can't collapse the rail or let it swallow the terminal.
const MIN_SIDEBAR_WIDTH: f32 = 180.;
const MAX_SIDEBAR_WIDTH_RATIO: f32 = 0.5;

/// Width (px) of the draggable resize handle's invisible hit-area, centered on
/// the rail's right border; it holds a 1px hairline that brightens on hover /
/// drag. Centered (half overhangs the body) so it clears the row close buttons.
const RESIZE_HANDLE_WIDTH: f32 = 8.;

impl Tty7App {
    /// The vertical tab sidebar rendered down the left edge of the body in
    /// `tab_bar_position: left` mode. Only reached when at least one tab is open
    /// (the caller keeps the horizontal layout for the zero-tab home page), so
    /// there's no empty state to render.
    pub(crate) fn tab_sidebar(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let active = self.active;
        // While the bare ⌘/Ctrl hold is armed (see `ui::hints`), each of the
        // first nine rows swaps its close affordance for a ⌘N badge — same slot
        // and footprint as the chips, so the vertical list gets the identical
        // "hold to see switch digits" hint the horizontal strip has.
        let show_badges = self.mod_hint_badges;
        // Width from the persisted/drag-updated cell, re-clamped to the live
        // window so a saved value never exceeds half the (possibly smaller)
        // viewport. The floor wins if half the window is somehow narrower.
        let max_width = (window.viewport_size().width.as_f32() * MAX_SIDEBAR_WIDTH_RATIO)
            .max(MIN_SIDEBAR_WIDTH);
        let width = self.sidebar_width.get().clamp(MIN_SIDEBAR_WIDTH, max_width);
        // Live filter query from the top-bar search box; empty matches all.
        let query = self.sidebar_search.read(cx).value().trim().to_lowercase();

        let mut list = v_flex()
            // An id + `overflow_y_scroll` makes the row column scroll on its own
            // when the tabs outgrow the window height, leaving the "+" footer
            // pinned (same pattern the settings panel uses). `track_scroll` lets
            // `activate` pull the selected row into view.
            .id("tab-sidebar-list")
            .track_scroll(&self.sidebar_scroll)
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            // 4px horizontal, so a row's own `pl_2` puts its content on the rail's
            // 12px content inset — the same line the search row and the top
            // controls use. The 8px the capsule stops short of the rail edge is
            // what makes the active row read as inset rather than full-bleed.
            .px_1()
            .py_1p5()
            // Tight row-to-row spacing so the tabs read as one dense list, not a
            // set of far-apart cards (each row already has its own padding).
            .gap_0p5();

        // ── Repo grouping ─────────────────────────────────────────────────────
        // Per-tab sticky group keys and the sections derived from them (both
        // documented on their functions). The same pair drives
        // `visual_tab_order`, so the ⌘N digits painted below and the ⌘N
        // actions always agree on which row is "tab 3".
        let keys: Rc<Vec<Option<PathBuf>>> = Rc::new(self.sidebar_group_keys(cx));
        let sections = sidebar_sections(&keys);

        // Each row's position in display order — the digit its ⌘N badge
        // shows. Advanced for every tab, filtered-out ones included, so the
        // digits (and what ⌘N targets) don't shift while the search box
        // narrows the list.
        let mut visual_pos = 0usize;
        for (group_name, idxs) in sections {
            // Rows first: the search filter may empty a group, in which case
            // its header is skipped too (and the header's count reflects the
            // *visible* rows while a filter narrows the list).
            let mut rows: Vec<AnyElement> = Vec::new();
            for i in idxs {
                // This row's display-order digit; claimed before the search
                // filter can skip the row (see `visual_pos` above).
                let badge_pos = visual_pos;
                visual_pos += 1;
                let tab = &self.tabs[i];
                let is_active = i == active;
                let label = self.tab_label(tab, i, Some(window), cx);
                // No status/cwd text under the title: the avatar's status dot
                // already carries working/waiting/done, and the group header + the
                // trailing branch tag carry the location — a "Working…" or cwd
                // line would just be noise. One line per row, nothing else.
                // Leading avatar inputs: the SSH connection-status colour (PRD
                // FR-E2) and the coding agent running in the tab, if any — the
                // avatar brands the row by whichever applies.
                let ssh_dot = self.tab_ssh_dot(tab, cx);
                let agent = tab.agent(cx);
                let agent_status = tab.agent_status(cx);
                let agent_unread = tab.agent_unread_count(cx);
                // Second line, when the pane is inside a git work tree: the
                // branch (flexes + truncates) with the working-tree diff pinned
                // to the row's right. Kept *off* the title line on purpose — a
                // long branch or a big `+426 −238` would otherwise crowd the
                // title into an ellipsis. The branch is also the row's most
                // volatile text (checkouts, rebases), so isolating it here means
                // a change never disturbs the title; grouping keys on the repo
                // root only, so a branch switch never relocates the row either
                // (see `Tab::sidebar_group`). The diff counts are a quiet
                // green/red readout and double as the diff-overlay toggle: click
                // them to peek another session's changes in an overlay without
                // activating this row's tab. The cwd they probe is the same one
                // the status resolved through, so overlay and counts always
                // describe the same repo.
                let git_cwd = tab
                    .pane
                    .focused_or_first(window, cx)
                    .and_then(|leaf| leaf.read(cx).git_status_cwd().map(|p| p.to_path_buf()));
                let git_line = tab.git_status(Some(window), cx).map(|g| {
                    let mut line = h_flex()
                        .id(("sidebar-git", i))
                        .w_full()
                        .items_center()
                        .gap_1p5()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(
                            gpui::svg()
                                .path("icons/git-branch.svg")
                                .flex_shrink_0()
                                .size(px(11.))
                                .text_color(cx.theme().muted_foreground),
                        )
                        // Branch name flexes and truncates; the counts stay
                        // pinned right (a long branch ellipsizes, counts don't).
                        .child(div().flex_1().min_w_0().truncate().child(g.branch.clone()));
                    if g.added > 0 || g.removed > 0 {
                        let mut counts = h_flex()
                            .id(("sidebar-diff", i))
                            .flex_shrink_0()
                            .items_center()
                            .gap_1p5()
                            .when_some(git_cwd, |counts, cwd| {
                                counts.cursor_pointer().on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                                        // Swallow the press so the row/label
                                        // handlers don't also activate the tab.
                                        cx.stop_propagation();
                                        this.toggle_diff_overlay(cwd.clone(), window, cx);
                                    }),
                                )
                            });
                        if g.added > 0 {
                            counts = counts.child(
                                div()
                                    .text_color(cx.theme().success)
                                    .child(format!("+{}", g.added)),
                            );
                        }
                        if g.removed > 0 {
                            counts = counts.child(
                                div()
                                    .text_color(cx.theme().danger)
                                    .child(format!("−{}", g.removed)),
                            );
                        }
                        line = line.child(counts);
                    }
                    line
                });
                // Filter by the search box; matching is on the visible label. The row
                // keeps its real index `i`, so activate/close/move still hit the right
                // tab even when the list is narrowed.
                if !query.is_empty() && !label.to_lowercase().contains(&query) {
                    continue;
                }
                let drag_label: SharedString = label.clone().into();

                // Inline rename input for this tab, if it's the one being renamed —
                // the same `self.renaming` branch the strip uses, so a context-menu
                // rename works identically in either layout.
                let rename_input = self
                    .renaming
                    .as_ref()
                    .filter(|r| r.index == i)
                    .map(|r| r.input.clone());

                let label_region = match rename_input {
                    Some(input) => div()
                        .id(("sidebar-rename", i))
                        .flex_1()
                        .min_w_0()
                        // Swallow the mouse-down (incl. double-click word-select) so
                        // it doesn't reach the row's activate handler below.
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(Input::new(&input).appearance(false))
                        .into_any_element(),
                    None => v_flex()
                        .id(("sidebar-label", i))
                        .flex_1()
                        .min_w_0()
                        // A hair of air between the title and branch lines.
                        .gap(px(2.))
                        // Title line — ellipsis-truncate so a long label degrades
                        // gracefully in the fixed-width rail rather than hard-clipping.
                        .child(
                            div()
                                .w_full()
                                .truncate()
                                .text_sm()
                                // Active row carries a hair more weight, matching the chip.
                                .when(is_active, |d| d.font_weight(FontWeight::MEDIUM))
                                .child(label),
                        )
                        // Branch + diff line, when the pane sits in a git repo.
                        .children(git_line)
                        // Click activates. (Renaming lives in the context menu,
                        // matching the strip — no double-click rename.)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                this.activate(i, window, cx);
                            }),
                        )
                        // Drag the row by its label to reorder it (shared `DragTab`).
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

                let row = h_flex()
                    .id(("tab-row", i))
                    // A per-row group so this row's close affordance reveals on its own
                    // hover without touching siblings (same trick as the chip).
                    .group(SharedString::from(format!("tab-row-{i}")))
                    .w_full()
                    // Size to content with a small, uniform vertical padding: a
                    // one-line shell tab is a short row, a two-line git tab
                    // (title + branch) is a taller one. The *padding* is what
                    // stays consistent, so rows read as harmonious even though
                    // heights differ.
                    .py_1()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .pl_2()
                    .pr_2()
                    .rounded_lg()
                    // Sidebar-surface token scheme (gpui-component's Sidebar
                    // semantics), so the rows sit cohesively on the sunk rail rather
                    // than reading as chips: active = the sidebar-accent fill + its
                    // paired foreground; inactive = the muted sidebar foreground with
                    // a half-strength accent on hover (a natural hover→active ramp).
                    .when(is_active, |s| {
                        s.bg(cx.theme().sidebar_accent)
                            .text_color(cx.theme().sidebar_accent_foreground)
                    })
                    .when(!is_active, |s| {
                        s.text_color(cx.theme().sidebar_foreground)
                            .hover(|s| s.bg(cx.theme().sidebar_accent.opacity(0.5)))
                    })
                    // Drop target: dropping a dragged row here moves it to this
                    // slot — but only within the same group; a cross-group drop is
                    // a no-op, since a tab's group comes from its cwd's repo, not
                    // from where it sits in the list. (With grouping off all keys
                    // are `None`, so the check never blocks anything.)
                    .drag_over::<DragTab>(|s, _, _, cx| s.bg(cx.theme().drag_border.opacity(0.2)))
                    .on_drop(cx.listener({
                        let keys = keys.clone();
                        move |this, drag: &DragTab, _window, cx| {
                            if keys.get(drag.index) == keys.get(i) {
                                this.move_tab(drag.index, i, cx);
                            }
                        }
                    }))
                    // A click anywhere on the row (padding, gaps) activates it; the
                    // label and close children stop propagation for their own actions.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.activate(i, window, cx);
                        }),
                    )
                    // Leading avatar: agent brand mark, SSH status, or shell glyph.
                    .child(self.tab_avatar(agent, agent_status, agent_unread, ssh_dot, 22., cx))
                    .child(label_region)
                    // Trailing slot: while the shortcut hints are armed it shows the
                    // row's ⌘N switch digit; otherwise the close affordance —
                    // opacity-0-until-hover on every row, active or not, so a column
                    // of tabs reads clean. Space is reserved either way. The digit
                    // is the row's *display* position (`activate_visual` speaks the
                    // same order), so under grouping the rail still reads 1…9 top
                    // to bottom instead of scattering the tab-vector indices.
                    .child(if show_badges && badge_pos < 9 {
                        // Bare digit, no keycap box — matches the chip badge exactly.
                        div()
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(20.))
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(if is_active {
                                cx.theme().sidebar_accent_foreground
                            } else {
                                cx.theme().muted_foreground
                            })
                            .child(tab_badge_label(badge_pos))
                            .into_any_element()
                    } else {
                        div()
                            .flex_shrink_0()
                            .opacity(0.)
                            .group_hover(SharedString::from(format!("tab-row-{i}")), |s| {
                                s.opacity(1.)
                            })
                            .child(
                                Button::new(("sidebar-close", i))
                                    .icon(IconName::Close)
                                    .ghost()
                                    .xsmall()
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.close_tab(i, window, cx);
                                    })),
                            )
                            .into_any_element()
                    });

                // Per-tab right-click menu, shared with the strip's chips;
                // `below_wording` flips the trailing close to "Close Tabs Below"
                // to match the vertical layout.
                let menu_app = cx.entity().downgrade();
                rows.push(
                    row.context_menu(move |menu, window, cx| {
                        Tty7App::tab_context_menu(menu, i, true, &menu_app, window, cx)
                    })
                    .into_any_element(),
                );
            }

            if rows.is_empty() {
                continue;
            }
            // Group header: the repo's directory name (or "Scratch"), small
            // and muted so it labels without competing with the rows, plus the
            // visible-row count. Not a click target — rows do the activating.
            if let Some(name) = group_name {
                list = list.child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap_1p5()
                        .pl_2()
                        .pr_2()
                        .pt_1p5()
                        .pb_0p5()
                        .text_size(px(11.))
                        .text_color(cx.theme().muted_foreground)
                        // Count sits right next to the name (not pushed to the
                        // rail's right edge): the name shrinks and truncates if
                        // long, the count trails it as a quiet tally.
                        .child(
                            div()
                                .flex_shrink(1.)
                                .min_w_0()
                                .truncate()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(name.to_uppercase()),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_color(cx.theme().muted_foreground.opacity(0.7))
                                .child(rows.len().to_string()),
                        ),
                );
            }
            for row in rows {
                list = list.child(row);
            }
        }

        // The rail's own controls — new tab, and collapse — live in the top zone
        // beside the traffic lights, right-aligned to the rail's content edge
        // rather than sitting in the search row. Two reasons: the search row is
        // for searching, and a collapse button that lives *inside* the rail would
        // disappear along with it (its counterpart then appears in the title
        // strip, see `tab_strip`). Right-aligned, they ride the rail's right edge,
        // which is what says "these belong to this panel" when it's resized.
        let controls = h_flex()
            .flex_shrink_0()
            .h(px(TITLE_BAR_HEIGHT))
            .items_center()
            .justify_end()
            .gap(px(2.))
            // Glyph, not hit box, on the content edge — see `TILE_PAD`.
            .pr(px(crate::ui::app::CONTENT_INSET - crate::ui::app::TILE_PAD))
            .child(
                self.attach_new_tab_menu(
                    Button::new("sidebar-add")
                        .icon(Icon::new(IconName::Plus).size(px(15.)))
                        .ghost()
                        .xsmall()
                        .w(px(30.))
                        .h(px(30.))
                        .rounded_lg(),
                    cx,
                ),
            )
            .child(
                crate::ui::tab_strip::chrome_tile(
                    Button::new("sidebar-collapse")
                        .icon(Icon::new(IconName::PanelLeft).size(px(15.))),
                    false,
                    cx,
                )
                .xsmall()
                .w(px(30.))
                .h(px(30.))
                .rounded_lg()
                .tooltip("Hide Sidebar")
                .on_click(cx.listener(|this, _, _window, cx| this.toggle_left_panel(cx))),
            );
        // Borderless "Search tabs…" that sits directly on the sunk surface: a
        // leading magnifier + an appearance-less input, no box and no divider
        // under the bar, so the control row and list read as one continuous rail
        // rather than stacked panels.
        let top_bar = h_flex()
            .flex_shrink_0()
            .items_center()
            .gap_1()
            .h(px(44.))
            .px(px(crate::ui::app::CONTENT_INSET))
            .child(
                Icon::new(IconName::Search)
                    .small()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.sidebar_search).appearance(false)),
            );

        // ── Resize drag (mirrors the split divider in `pane.rs`) ──────────────
        // A backing canvas measures the rail's bounds into a per-frame cell and,
        // while the handle is held, installs window-level mouse listeners so the
        // drag keeps tracking even when the pointer outruns the thin handle.
        let container: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::new(Cell::new(None));
        let backing = canvas(
            {
                let container = container.clone();
                move |bounds, _window, _cx| container.set(Some(bounds))
            },
            {
                let container = container.clone();
                let width_cell = self.sidebar_width.clone();
                let dragging = self.sidebar_dragging.clone();
                move |_bounds, _state, window, _cx| {
                    // Track the pointer while the handle is held: width = pointer
                    // x minus the rail's left edge, clamped to the live bounds.
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
                            let raw = (ev.position.x - b.origin.x).as_f32();
                            let max = (window.viewport_size().width.as_f32()
                                * MAX_SIDEBAR_WIDTH_RATIO)
                                .max(MIN_SIDEBAR_WIDTH);
                            width_cell.set(raw.clamp(MIN_SIDEBAR_WIDTH, max));
                            window.refresh();
                        }
                    });
                    // On release, end the drag and persist the final width so it
                    // survives a restart (the config observer re-syncs the cell).
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
                            if cfg.sidebar_width != w {
                                cfg.sidebar_width = w;
                                cfg.save();
                            }
                            window.refresh();
                        }
                    });
                }
            },
        )
        .absolute()
        .size_full();

        // The draggable handle at the right edge: a comfortable invisible hit-area
        // centered over the border, holding a 1px line that brightens on hover /
        // drag (the border stays visible underneath when idle).
        let handle_active = self.sidebar_dragging.get();
        let handle = div()
            .group("sidebar-resize")
            .absolute()
            .top_0()
            .right(px(-(RESIZE_HANDLE_WIDTH / 2.)))
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
                    .when(handle_active, |d| d.bg(cx.theme().drag_border))
                    .group_hover("sidebar-resize", |s| s.bg(cx.theme().drag_border)),
            )
            .on_mouse_down(MouseButton::Left, {
                let dragging = self.sidebar_dragging.clone();
                move |_ev, window, _cx| {
                    dragging.set(true);
                    window.refresh();
                }
            });

        div()
            .relative()
            .flex_shrink_0()
            .w(px(width))
            .h_full()
            // The whole rail is the sunk `sidebar` surface (a few % off the body),
            // so the color contrast — not hard lines — separates it from the
            // terminal. A single hairline right edge in the paired border token
            // delineates the seam, so the rail reads as one cohesive surface.
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            // The measurer/listener sits behind the content, the handle on top.
            .child(backing)
            .child(
                v_flex()
                    .size_full()
                    // A title-bar-height top zone: on macOS the traffic lights
                    // sit on the rail's surface here, and it aligns the search box
                    // with the terminal's top (which starts below the title bar),
                    // so the rail reads as one panel from the very top edge. The
                    // rail's controls ride its right end, on the title bar's own
                    // center line — same row as the "⋯" across the window.
                    .child(controls)
                    .child(top_bar)
                    .child(list),
            )
            .child(handle)
    }

    /// Each tab's sidebar group key, in tab order: the *repository home* of
    /// its *first* pane's cwd (the main checkout's root — linked worktrees of
    /// one repo share a group), resolved through the tab's sticky
    /// `sidebar_group` cell — only a landed probe answer moves a tab (see the
    /// field's doc), so an in-flight cd never reshuffles the list. The first
    /// pane rather than the focused one (which the branch line follows), so
    /// switching focus between splits in different repos never relocates the
    /// row — the group answers "where does this tab live", not "what am I
    /// touching". `None` = the Scratch group. With grouping configured off
    /// every key is `None`, which collapses the list to one flat section and
    /// makes the same-group drop check a no-op.
    fn sidebar_group_keys(&self, cx: &gpui::App) -> Vec<Option<PathBuf>> {
        let grouping = cx.global::<Config>().sidebar_grouping == SidebarGrouping::Repo;
        self.tabs
            .iter()
            .map(|tab| {
                if !grouping {
                    return None;
                }
                let cwd = tab
                    .pane
                    .first_leaf()
                    .and_then(|leaf| leaf.read(cx).git_status_cwd().map(|p| p.to_path_buf()));
                if let Some(known) =
                    cwd.and_then(|cwd| cx.global::<GitStatusCache>().known_repo_for(&cwd))
                {
                    *tab.sidebar_group.borrow_mut() = known;
                }
                tab.sidebar_group.borrow().clone()
            })
            .collect()
    }

    /// Tab indices in the order the tab UI displays them: the grouped order
    /// when the vertical sidebar is grouping by repo, plain tab order in
    /// every other layout. ⌘N and the hint digits both go through this, so
    /// "press 3 for the third row you see" stays true under grouping.
    fn visual_tab_order(&self, cx: &gpui::App) -> Vec<usize> {
        if cx.global::<Config>().tab_bar_position != crate::core::config::TabBarPosition::Left {
            return (0..self.tabs.len()).collect();
        }
        let keys = self.sidebar_group_keys(cx);
        sidebar_sections(&keys)
            .into_iter()
            .flat_map(|(_, idxs)| idxs)
            .collect()
    }

    /// Activate the `n`-th tab *as displayed* (see
    /// [`visual_tab_order`](Self::visual_tab_order)) — the ⌘N actions land
    /// here so the shortcut always matches the digit badge on the row.
    pub(crate) fn activate_visual(
        &mut self,
        n: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(&i) = self.visual_tab_order(cx).get(n) {
            self.activate(i, window, cx);
        }
    }
}

/// Partition per-tab group keys into the sidebar's sections: `(header,
/// indices)` with groups in first-appearance order (a new repo appends, the
/// existing ones never reshuffle) and the Scratch group pinned last. A `None`
/// header means "render flat, no headers" — used when no tab is in any repo,
/// where a lone Scratch header over everything would be noise.
fn sidebar_sections(keys: &[Option<PathBuf>]) -> Vec<(Option<String>, Vec<usize>)> {
    let mut group_order: Vec<&PathBuf> = Vec::new();
    for k in keys.iter().flatten() {
        if !group_order.iter().any(|g| *g == k) {
            group_order.push(k);
        }
    }
    if group_order.is_empty() {
        return vec![(None, (0..keys.len()).collect())];
    }
    let names = group_names(&group_order);
    let mut sections: Vec<(Option<String>, Vec<usize>)> = group_order
        .iter()
        .zip(names)
        .map(|(root, name)| {
            let idxs = (0..keys.len())
                .filter(|&i| keys[i].as_ref() == Some(*root))
                .collect();
            (Some(name), idxs)
        })
        .collect();
    let scratch: Vec<usize> = (0..keys.len()).filter(|&i| keys[i].is_none()).collect();
    if !scratch.is_empty() {
        sections.push((Some("Scratch".into()), scratch));
    }
    sections
}

/// Display names for the group roots: each root's directory name, extended
/// upward by parent components only while it collides with another root's
/// (`app` stays `app` on its own; two checkouts both named `app` become
/// `work/app` and `fork/app`). Distinct roots must differ somewhere, so the
/// loop settles; a root that exhausts its components while still colliding
/// just keeps its longest suffix.
fn group_names(roots: &[&PathBuf]) -> Vec<String> {
    // Only the normal components — no root-dir "/" entry, so a joined suffix
    // never renders as "//work/app".
    let comps: Vec<Vec<String>> = roots
        .iter()
        .map(|r| {
            r.components()
                .filter(|c| matches!(c, std::path::Component::Normal(_)))
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect()
        })
        .collect();
    let mut depth = vec![1usize; roots.len()];
    loop {
        let names: Vec<String> = comps
            .iter()
            .zip(&depth)
            .enumerate()
            .map(|(i, (c, &d))| {
                if c.is_empty() {
                    // Degenerate root with no normal components (e.g. "/").
                    roots[i].display().to_string()
                } else {
                    c[c.len().saturating_sub(d)..].join("/")
                }
            })
            .collect();
        let mut grew = false;
        for i in 0..names.len() {
            let collides = names
                .iter()
                .enumerate()
                .any(|(j, n)| j != i && *n == names[i]);
            if collides && depth[i] < comps[i].len() {
                depth[i] += 1;
                grew = true;
            }
        }
        if !grew {
            return names;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// Groups appear in first-appearance order with Scratch pinned last, and
    /// an all-`None` key set renders as one headerless flat section.
    #[test]
    fn sections_order_groups_by_first_appearance_scratch_last() {
        let keys = vec![
            Some(p("/w/beta")),
            None,
            Some(p("/w/alpha")),
            Some(p("/w/beta")),
        ];
        let sections = sidebar_sections(&keys);
        assert_eq!(
            sections,
            vec![
                (Some("beta".into()), vec![0, 3]),
                (Some("alpha".into()), vec![2]),
                (Some("Scratch".into()), vec![1]),
            ]
        );

        let flat = sidebar_sections(&[None, None]);
        assert_eq!(flat, vec![(None, vec![0, 1])]);
    }

    /// Same-named roots grow a parent prefix until distinct; unrelated names
    /// stay short even alongside the colliding pair.
    #[test]
    fn group_names_disambiguate_only_the_collisions() {
        let (a, b, c) = (
            p("/home/u/work/app"),
            p("/home/u/fork/app"),
            p("/home/u/tty7"),
        );
        let names = group_names(&[&a, &b, &c]);
        assert_eq!(names, vec!["work/app", "fork/app", "tty7"]);
    }

    /// A root that runs out of components keeps its longest suffix instead of
    /// looping forever, and the other side still grows past it to distinctness.
    #[test]
    fn group_names_handle_suffix_roots() {
        let (short, long) = (p("/app"), p("/x/app"));
        let names = group_names(&[&short, &long]);
        assert_eq!(names, vec!["app", "x/app"]);
    }
}
