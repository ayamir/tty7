//! System tray / menu bar status item.
//!
//! The tray is the app's face outside the window: the icon flips to an
//! attention state the moment any pane's coding agent blocks on the user
//! (amber `Waiting`), and the menu lists every agent pane — click one to
//! reveal it — plus window/notification/quit controls. Menu labels are
//! English, matching the native app menus (`ui::theme::set_menus`).
//!
//! Platform split (see Cargo.toml for the why):
//! - macOS / Windows: tauri's `tray-icon` (NSStatusItem / Shell_NotifyIcon),
//!   in [`native`]. Both are driven by the main-thread event loop gpui
//!   already pumps, so the backend lives on the foreground executor.
//! - Linux: `ksni` (StatusNotifierItem over DBus, pure Rust), in [`sni`] —
//!   `tray-icon`'s Linux backend would drag in GTK + libappindicator, which
//!   the AppImage doesn't bundle. On desktops without an SNI host the spawn
//!   fails and the app simply runs without a tray.
//!
//! Data flow mirrors the rest of the UI (which polls rather than observes —
//! see `TerminalView::poll_foreground`): a foreground task snapshots the
//! agent panes once a second, diffs against the last snapshot, and only
//! touches the native tray when something changed. Menu clicks come back on
//! a channel and are applied to the app on the foreground executor.

mod icon;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod native;
#[cfg(target_os = "linux")]
mod sni;

#[cfg(any(target_os = "macos", target_os = "windows"))]
use native::Backend;
#[cfg(target_os = "linux")]
use sni::Backend;

use crate::core::cli_agent::AgentStatus;
use crate::core::config::{Config, NotifyMode};
use crate::ui::app::Tty7App;
use gpui::Context;

/// How often the poll loop re-snapshots the app. Agent status itself is
/// polled into the views on a 300 ms timer; 1 s here keeps the tray a hair
/// behind the in-window dots at negligible cost.
const POLL: std::time::Duration = std::time::Duration::from_millis(1000);

/// A menu click, decoded from the platform menu item id and applied to the
/// app by [`Tty7App::handle_tray_action`] on the foreground executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrayAction {
    /// Bring the window to the front.
    ShowWindow,
    /// Reveal the pane hosting this agent: switch to its tab, focus the
    /// leaf, and activate the window. The id is the leaf's gpui entity id —
    /// resolved against the live tab tree at click time, so a row that
    /// outlived its pane (menu open across a close) degrades to a no-op.
    RevealPane { leaf_id: u64 },
    /// Set the notification policy (the same knob as Settings → Terminal).
    SetNotifyMode(NotifyMode),
    OpenSettings,
    /// Force an update check (even with the startup check disabled) and open
    /// Settings → About, where the result lands.
    CheckForUpdates,
    /// Plain quit — identical to ⌘Q: the daemon and every session survive.
    Quit,
    /// Quit *and* shut the daemon down, ending every running session.
    /// Confirmed with a prompt before anything happens.
    QuitStopSessions,
}

/// One agent pane, as shown in the tray menu.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AgentRow {
    /// The hosting leaf's entity id (`EntityId::as_u64`), the reveal key.
    pub leaf_id: u64,
    /// Which agent — names the row and picks the brand avatar.
    pub agent: crate::core::cli_agent::CLIAgent,
    pub status: AgentStatus,
    /// Where it's working: the cwd's directory name, plus the git branch
    /// when known — e.g. "tty7 @ main".
    pub detail: String,
}

/// Everything the tray renders, diffed once a second against the app.
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct TraySnapshot {
    /// Agent panes, most urgent first (waiting > working > done > idle).
    pub agents: Vec<AgentRow>,
    pub notify_mode: NotifyMode,
}

impl TraySnapshot {
    /// Whether any agent is blocked on the user — drives the attention icon.
    pub(crate) fn attention(&self) -> bool {
        self.agents
            .iter()
            .any(|a| a.status == AgentStatus::Waiting)
    }

    /// Hover text: a one-line census of the agent panes ("tty7 — 1 waiting,
    /// 2 working"), or just "tty7" when none are running.
    pub(crate) fn tooltip(&self) -> String {
        let count = |s: AgentStatus| self.agents.iter().filter(|a| a.status == s).count();
        let mut parts = Vec::new();
        for (n, word) in [
            (count(AgentStatus::Waiting), "waiting"),
            (count(AgentStatus::Working), "working"),
            (count(AgentStatus::Done), "done"),
        ] {
            if n > 0 {
                parts.push(format!("{n} {word}"));
            }
        }
        if parts.is_empty() {
            "tty7".to_string()
        } else {
            format!("tty7 — {}", parts.join(", "))
        }
    }
}

/// The platform-independent menu shape; each backend translates it 1:1 into
/// its native menu type, so the layout and labels live in exactly one place
/// ([`menu_spec`]).
pub(crate) enum SpecItem {
    Item {
        id: String,
        label: String,
        /// `Some(_)` renders a checkable item (the notification radio).
        checked: Option<bool>,
        /// `Some(_)` renders the agent's brand avatar (colored disc + white
        /// mark + status dot — the tab avatar's menu translation) next to the
        /// label. The backends rasterize it via [`icon::agent_avatar`].
        avatar: Option<(crate::core::cli_agent::CLIAgent, AgentStatus)>,
    },
    Separator,
    Submenu {
        label: String,
        items: Vec<SpecItem>,
    },
}

/// Build the menu for a snapshot. Layout: reveal/window on top, then the
/// live agent panes, then notification policy + settings, then the two quit
/// flavors — plain quit keeps sessions (like ⌘Q), the second one stops them.
pub(crate) fn menu_spec(snap: &TraySnapshot) -> Vec<SpecItem> {
    let item = |id: &str, label: String| SpecItem::Item {
        id: id.to_string(),
        label,
        checked: None,
        avatar: None,
    };
    let mut items = vec![
        item("show", "Show tty7".into()),
        SpecItem::Separator,
    ];
    for a in &snap.agents {
        // The avatar (brand disc + status dot) carries the who/state visually,
        // exactly like the tab chip; the textual suffix repeats the state for
        // scanability in a text-first menu.
        let state = match a.status {
            AgentStatus::Waiting => " — needs input",
            AgentStatus::Working => " — working",
            AgentStatus::Done => " — done",
            AgentStatus::Idle => "",
        };
        items.push(SpecItem::Item {
            id: format!("agent:{}", a.leaf_id),
            label: format!("{} · {}{state}", a.agent.display_name(), a.detail),
            checked: None,
            avatar: Some((a.agent, a.status)),
        });
    }
    if !snap.agents.is_empty() {
        items.push(SpecItem::Separator);
    }
    let notify = |id: &str, label: &str, mode: NotifyMode| SpecItem::Item {
        id: id.to_string(),
        label: label.to_string(),
        checked: Some(snap.notify_mode == mode),
        avatar: None,
    };
    items.push(SpecItem::Submenu {
        label: "Notifications".into(),
        items: vec![
            notify("notify:always", "Always", NotifyMode::Always),
            notify("notify:unfocused", "When Unfocused", NotifyMode::Unfocused),
            notify("notify:never", "Never", NotifyMode::Never),
        ],
    });
    items.push(item("settings", "Settings…".into()));
    items.push(item("updates", "Check for Updates…".into()));
    items.push(SpecItem::Separator);
    items.push(item("quit", "Quit tty7".into()));
    // Plain quit leaves the daemon (and every session) running; this one
    // stops the daemon too. "Daemon" is already in the product vocabulary —
    // the app menu ships "Restart Daemon…" — and the confirm prompt spells
    // out the consequences.
    items.push(item("quit-stop", "Quit and Stop Daemon…".into()));
    items
}

/// Decode a clicked menu item id back into an action. Ids are assigned in
/// [`menu_spec`]; unknown ids (never expected) decode to `None` and the
/// click is dropped.
pub(crate) fn action_from_id(id: &str) -> Option<TrayAction> {
    match id {
        "show" => Some(TrayAction::ShowWindow),
        "settings" => Some(TrayAction::OpenSettings),
        "updates" => Some(TrayAction::CheckForUpdates),
        "quit" => Some(TrayAction::Quit),
        "quit-stop" => Some(TrayAction::QuitStopSessions),
        "notify:always" => Some(TrayAction::SetNotifyMode(NotifyMode::Always)),
        "notify:unfocused" => Some(TrayAction::SetNotifyMode(NotifyMode::Unfocused)),
        "notify:never" => Some(TrayAction::SetNotifyMode(NotifyMode::Never)),
        _ => {
            let leaf_id = id.strip_prefix("agent:")?.parse().ok()?;
            Some(TrayAction::RevealPane { leaf_id })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_with_agent(status: AgentStatus) -> TraySnapshot {
        TraySnapshot {
            agents: vec![AgentRow {
                leaf_id: 42,
                agent: crate::core::cli_agent::CLIAgent::Claude,
                status,
                detail: "tty7 @ main".into(),
            }],
            notify_mode: NotifyMode::Unfocused,
        }
    }

    /// Every id the menu builder mints must decode back to an action —
    /// otherwise a click on that item silently does nothing.
    #[test]
    fn every_menu_id_decodes_to_an_action() {
        fn check(items: &[SpecItem]) {
            for item in items {
                match item {
                    SpecItem::Item { id, label, .. } => assert!(
                        action_from_id(id).is_some(),
                        "menu item {label:?} has undecodable id {id:?}"
                    ),
                    SpecItem::Separator => {}
                    SpecItem::Submenu { items, .. } => check(items),
                }
            }
        }
        check(&menu_spec(&snapshot_with_agent(AgentStatus::Waiting)));
        check(&menu_spec(&TraySnapshot::default()));
    }

    #[test]
    fn agent_rows_decode_to_reveal_with_their_leaf_id() {
        assert_eq!(
            action_from_id("agent:42"),
            Some(TrayAction::RevealPane { leaf_id: 42 })
        );
        // Garbage after the prefix is dropped, not a panic or a mis-decode.
        assert_eq!(action_from_id("agent:nope"), None);
        assert_eq!(action_from_id("bogus"), None);
    }

    #[test]
    fn attention_follows_waiting_and_tooltip_counts() {
        assert!(snapshot_with_agent(AgentStatus::Waiting).attention());
        assert!(!snapshot_with_agent(AgentStatus::Working).attention());
        assert_eq!(
            snapshot_with_agent(AgentStatus::Waiting).tooltip(),
            "tty7 — 1 waiting"
        );
        assert_eq!(TraySnapshot::default().tooltip(), "tty7");
    }

    /// The empty snapshot renders no agent section (no dangling separator),
    /// and the notification radio reflects the snapshot's mode.
    #[test]
    fn menu_spec_shape() {
        let empty = menu_spec(&TraySnapshot::default());
        let labels: Vec<_> = empty
            .iter()
            .filter_map(|i| match i {
                SpecItem::Item { label, .. } => Some(label.as_str()),
                SpecItem::Submenu { label, .. } => Some(label.as_str()),
                SpecItem::Separator => None,
            })
            .collect();
        assert_eq!(
            labels,
            [
                "Show tty7",
                "Notifications",
                "Settings…",
                "Check for Updates…",
                "Quit tty7",
                "Quit and Stop Daemon…"
            ]
        );
        // No two separators in a row when the agent section is absent.
        assert!(!empty.windows(2).any(|w| matches!(
            w,
            [SpecItem::Separator, SpecItem::Separator]
        )));

        let with_agent = menu_spec(&snapshot_with_agent(AgentStatus::Waiting));
        assert!(with_agent.iter().any(|i| matches!(
            i,
            SpecItem::Item { id, avatar: Some(_), .. } if id == "agent:42"
        )));
    }
}

/// Wire the tray up: one task pumps menu clicks into the app, another polls
/// the app into the tray. Called once from `Tty7App::with_session`; both
/// tasks end (dropping the tray icon) when the app entity drops.
///
/// `show_tray_icon` is re-read every tick, so the Settings toggle and a
/// `config.json` hot-reload both take effect within a second — the backend
/// is dropped (icon removed) when off and re-created when back on.
pub(crate) fn init(cx: &mut Context<Tty7App>) {
    let (tx, rx) = smol::channel::unbounded::<TrayAction>();

    // Menu clicks → the app. The platform handler feeds `tx` from wherever
    // the OS delivers menu events; this task is the only place they touch
    // gpui state, with a real window + context in hand.
    cx.spawn(async move |this, cx| {
        while let Ok(action) = rx.recv().await {
            let alive = this.update_in(cx, |app, window, cx| {
                app.handle_tray_action(action, window, cx)
            });
            if alive.is_err() {
                break;
            }
        }
    })
    .detach();

    // App → tray poll loop. Owns the backend; dropping it removes the icon.
    // The backend types are !Send on macOS (NSStatusItem), which is fine on
    // the foreground executor — exactly where tray-icon requires them.
    cx.spawn(async move |this, cx| {
        let mut backend: Option<Backend> = None;
        // Last snapshot actually pushed; `None` forces a push after
        // (re)creation so a fresh icon never shows a stale menu.
        let mut shown: Option<TraySnapshot> = None;
        // Creation can fail transiently — on Linux the SNI host may simply
        // not be on the bus *yet* (tty7 autostarting at login races the
        // desktop shell / AppIndicator extension) — so a failed create is
        // retried on a slow backoff before giving up for this enable-cycle:
        // one attempt every RETRY_EVERY ticks, MAX_ATTEMPTS total. Toggling
        // the setting off and on re-arms.
        const MAX_ATTEMPTS: u32 = 10;
        const RETRY_EVERY: u32 = 30; // ticks ≈ seconds
        let mut attempts = 0u32;
        let mut cooldown = 0u32;
        loop {
            cx.background_executor().timer(POLL).await;
            let Ok((enabled, snap)) = this.update(cx, |app, cx| {
                (
                    cx.global::<Config>().show_tray_icon,
                    app.tray_snapshot(cx),
                )
            }) else {
                break; // app gone — backend drops with the task
            };
            if !enabled {
                backend = None;
                shown = None;
                attempts = 0;
                cooldown = 0;
                continue;
            }
            if backend.is_none() && attempts < MAX_ATTEMPTS {
                if cooldown > 0 {
                    cooldown -= 1;
                    continue;
                }
                attempts += 1;
                backend = Backend::create(tx.clone(), cx).await;
                if backend.is_none() {
                    cooldown = RETRY_EVERY;
                    if attempts == MAX_ATTEMPTS {
                        log::warn!(
                            "tray icon unavailable after {MAX_ATTEMPTS} attempts; \
                             running without one"
                        );
                    }
                }
                shown = None;
            }
            if let Some(backend) = backend.as_mut()
                && shown.as_ref() != Some(&snap)
            {
                backend.update(&snap);
                shown = Some(snap);
            }
        }
    })
    .detach();
}
