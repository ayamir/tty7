//! Linux tray backend: `ksni`, a pure-Rust StatusNotifierItem over DBus.
//!
//! ksni owns a service thread and re-queries the [`ksni::Tray`] impl for
//! icon/menu/status whenever we call `Handle::update`, so the backend just
//! swaps the stored snapshot in. That call is a blocking round-trip to the
//! service thread, so updates flow through a background task rather than the
//! foreground poll loop. Menu item activation runs on ksni's thread; actions
//! cross back to gpui over the same channel the other platforms use.
//!
//! On desktops without an SNI host (bare GNOME without the AppIndicator
//! extension) the spawn fails; the poll loop logs once and the app runs
//! without a tray.

use super::{SpecItem, TrayAction, TraySnapshot, action_from_id, icon};
use gpui::{AppContext as _, AsyncApp};

pub(super) struct Backend {
    /// Feeds the updater task spawned in [`Backend::create`]. Dropping the
    /// Backend closes the channel, which makes that task shut the SNI
    /// service down — removing the icon.
    updates: smol::channel::Sender<TraySnapshot>,
}

impl Backend {
    /// Spawn the SNI service plus its updater task. Registration and every
    /// later `Handle::update` are blocking round-trips to ksni's service
    /// thread, so both live on the background executor rather than stalling
    /// the foreground poll loop.
    pub(super) async fn create(
        tx: smol::channel::Sender<TrayAction>,
        cx: &AsyncApp,
    ) -> Option<Self> {
        let handle = cx
            .background_spawn(async move {
                use ksni::blocking::TrayMethods as _;
                let tray = SniTray {
                    tx,
                    snap: TraySnapshot::default(),
                };
                match tray.spawn() {
                    Ok(handle) => Some(handle),
                    Err(e) => {
                        log::warn!("failed to register StatusNotifierItem: {e}");
                        None
                    }
                }
            })
            .await?;
        let (updates, update_rx) = smol::channel::unbounded::<TraySnapshot>();
        cx.background_spawn(async move {
            while let Ok(mut snap) = update_rx.recv().await {
                // Coalesce a queued burst down to the newest snapshot —
                // intermediate states would each cost a DBus push.
                while let Ok(later) = update_rx.try_recv() {
                    snap = later;
                }
                // `update` re-reads menu/icon/status from the Tray impl and
                // pushes the changed properties over DBus. `None` (service
                // gone) can only follow the `shutdown` below; a vanished SNI
                // *host* is ksni's problem — it re-registers by itself when
                // a watcher returns to the bus.
                handle.update(move |tray| tray.snap = snap);
            }
            // Channel closed: the Backend was dropped (tray toggled off or
            // app exit). Dropping the handle alone would leave the service
            // thread (and the icon) alive; ask it to unregister. The awaiter
            // is intentionally not waited on — teardown can finish on ksni's
            // thread.
            handle.shutdown();
        })
        .detach();
        Some(Backend { updates })
    }

    pub(super) fn update(&mut self, snap: &TraySnapshot) {
        // Unbounded channel: the only send failure is "closed", impossible
        // while the Backend (whose drop is what closes it) is alive.
        let _ = self.updates.try_send(snap.clone());
    }
}

struct SniTray {
    tx: smol::channel::Sender<TrayAction>,
    snap: TraySnapshot,
}

impl SniTray {
    /// A menu item that sends the action decoded from `id` — the same id
    /// space `action_from_id` serves on the other platforms.
    fn send(&self, id: &str) {
        if let Some(action) = action_from_id(id) {
            let _ = self.tx.try_send(action);
        }
    }
}

impl ksni::Tray for SniTray {
    fn id(&self) -> String {
        "tty7".into()
    }

    fn title(&self) -> String {
        "tty7".into()
    }

    fn status(&self) -> ksni::Status {
        if self.snap.attention() {
            // Hosts surface this — e.g. KDE moves the item out of the
            // overflow and may highlight it.
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let Some((data, size)) = icon::render_argb(self.snap.attention()) else {
            return Vec::new();
        };
        vec![ksni::Icon {
            width: size as i32,
            height: size as i32,
            data,
        }]
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.snap.tooltip(),
            ..Default::default()
        }
    }

    /// Plain left click on the item (not the menu): reveal the window.
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.try_send(TrayAction::ShowWindow);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        super::menu_spec(&self.snap)
            .into_iter()
            .map(translate)
            .collect()
    }
}

/// Translate one shared spec item into ksni's menu tree.
fn translate(item: SpecItem) -> ksni::MenuItem<SniTray> {
    match item {
        SpecItem::Item {
            id,
            label,
            checked: None,
            avatar,
        } => ksni::menu::StandardItem {
            label,
            // Agent rows carry the brand avatar (disc + white mark + status
            // dot) as PNG bytes — dbusmenu's icon-data. A failed render just
            // leaves the row text-only.
            icon_data: avatar
                .and_then(|(agent, status)| icon::agent_avatar(agent, status))
                .and_then(|pm| pm.encode_png().ok())
                .unwrap_or_default(),
            activate: Box::new(move |tray: &mut SniTray| tray.send(&id)),
            ..Default::default()
        }
        .into(),
        SpecItem::Item {
            id,
            label,
            checked: Some(checked),
            ..
        } => ksni::menu::CheckmarkItem {
            label,
            checked,
            activate: Box::new(move |tray: &mut SniTray| tray.send(&id)),
            ..Default::default()
        }
        .into(),
        SpecItem::Separator => ksni::MenuItem::Separator,
        SpecItem::Submenu { label, items } => ksni::menu::SubMenu {
            label,
            submenu: items.into_iter().map(translate).collect(),
            ..Default::default()
        }
        .into(),
    }
}
