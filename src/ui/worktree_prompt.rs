//! The "New Worktree Tab" sheet: confirms (or edits) the generated worktree
//! name, the new branch, and the branch it starts from before anything touches
//! git. Opened from the tab context menu (`tab_strip::tab_context_menu`); the
//! defaults are probed off the UI thread in `Tty7App::new_worktree_tab`.

use gpui::{AnyElement, Context, Entity, Subscription, Window, div, prelude::*, px};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};

use crate::core::worktree::{WorktreeDefaults, WorktreeRequest};
use crate::ui::app::Tty7App;

/// State for the open sheet. Held on [`Tty7App`] so it survives re-renders and
/// tab switches; there is at most one, app-wide.
pub(crate) struct WorktreePrompt {
    /// The directory the repo was derived from (the right-clicked tab's cwd) —
    /// what the eventual `git worktree add` resolves the repository through.
    cwd: std::path::PathBuf,
    /// Where the checkout will land (`<root>/<repo-name>`), for the live path
    /// preview under the name field.
    dir: std::path::PathBuf,
    name: Entity<InputState>,
    branch: Entity<InputState>,
    base: Entity<InputState>,
    /// True while `git worktree add` runs, so a second Enter can't double-create.
    busy: bool,
    _subs: Vec<Subscription>,
}

impl Tty7App {
    /// Open the sheet with probed defaults: the generated candidate fills both
    /// the name and the branch (edit either independently), the current branch
    /// fills the start point. Focus lands on the name field; Enter anywhere
    /// submits, Esc cancels.
    pub(crate) fn open_worktree_prompt(
        &mut self,
        cwd: std::path::PathBuf,
        defaults: WorktreeDefaults,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = cx.new(|cx| InputState::new(window, cx).default_value(defaults.name.clone()));
        let branch = cx.new(|cx| InputState::new(window, cx).default_value(defaults.name));
        let base = cx.new(|cx| InputState::new(window, cx).default_value(defaults.base));
        name.update(cx, |state, cx| state.focus(window, cx));
        let subs = [&name, &branch, &base]
            .into_iter()
            .map(|input| {
                cx.subscribe_in(input, window, |this, _, ev: &InputEvent, window, cx| {
                    match ev {
                        InputEvent::PressEnter { .. } => this.submit_worktree_prompt(window, cx),
                        // Keep the path preview tracking the name field.
                        InputEvent::Change => cx.notify(),
                        _ => {}
                    }
                })
            })
            .collect();
        self.worktree_prompt = Some(WorktreePrompt {
            cwd,
            dir: defaults.dir,
            name,
            branch,
            base,
            busy: false,
            _subs: subs,
        });
        cx.notify();
    }

    fn cancel_worktree_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.worktree_prompt.take().is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    /// Validate the fields and run the creation off the UI thread. Blanking
    /// one of name/branch falls back to the other (one name is enough); a
    /// blank start point means the repo's HEAD. On failure the sheet stays up
    /// with the values intact, so a typo'd branch is a fix away.
    fn submit_worktree_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.worktree_prompt.as_ref() else {
            return;
        };
        if p.busy {
            return;
        }
        let name = p.name.read(cx).value().trim().to_string();
        let branch = p.branch.read(cx).value().trim().to_string();
        let base = p.base.read(cx).value().trim().to_string();
        let (name, branch) = match (name.is_empty(), branch.is_empty()) {
            (true, true) => {
                window.push_notification("The worktree needs a name", cx);
                return;
            }
            (true, false) => (branch.clone(), branch),
            (false, true) => (name.clone(), name),
            (false, false) => (name, branch),
        };
        let req = WorktreeRequest {
            name,
            branch,
            base: if base.is_empty() {
                "HEAD".to_string()
            } else {
                base
            },
        };
        let p = self.worktree_prompt.as_mut().expect("checked above");
        p.busy = true;
        let cwd = p.cwd.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { crate::core::worktree::create(&cwd, &req) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(wt) => {
                    this.worktree_prompt = None;
                    this.open_worktree_tab(wt, window, cx);
                }
                Err(e) => {
                    if let Some(p) = this.worktree_prompt.as_mut() {
                        p.busy = false;
                    }
                    window.push_notification(format!("New worktree failed: {e}"), cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The sheet itself, floated near the top of the terminal area like the
    /// SSH auth sheet. `None` while no prompt is open.
    pub(crate) fn render_worktree_prompt_overlay(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let p = self.worktree_prompt.as_ref()?;
        let muted = cx.theme().muted_foreground;
        let field = |label: &'static str, input: &Entity<InputState>| {
            v_flex()
                .gap_1()
                .child(div().text_xs().text_color(muted).child(label))
                .child(Input::new(input).small())
        };
        // Live preview of where the checkout will land, following the name field.
        let name_now = p.name.read(cx).value().trim().to_string();
        let preview = p
            .dir
            .join(if name_now.is_empty() {
                "…"
            } else {
                &name_now
            })
            .display()
            .to_string();

        let card = v_flex()
            .occlude()
            .w(px(420.))
            .gap_3()
            .p_4()
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .rounded_lg()
            .shadow_lg()
            // Esc cancels from anywhere in the sheet (Enter submits via the
            // inputs' PressEnter events).
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                if ev.keystroke.key == "escape" {
                    this.cancel_worktree_prompt(window, cx);
                }
            }))
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("New Worktree Tab"),
            )
            .child(field("Worktree Name", &p.name))
            .child(
                div()
                    .text_xs()
                    .font_family("monospace")
                    .text_color(muted)
                    .child(preview),
            )
            .child(field("New Branch", &p.branch))
            .child(field("Start From", &p.base))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("worktree-create")
                            .label(if p.busy { "Creating…" } else { "Create" })
                            .small()
                            .primary()
                            .disabled(p.busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_worktree_prompt(window, cx)
                            })),
                    )
                    .child(
                        Button::new("worktree-cancel")
                            .label("Cancel")
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.cancel_worktree_prompt(window, cx)
                            })),
                    ),
            );

        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_start()
                .pt(px(48.))
                .child(card)
                .into_any_element(),
        )
    }
}
