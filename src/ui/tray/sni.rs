//! Linux tray backend: `ksni`, a pure-Rust StatusNotifierItem over DBus.
//!
//! ksni owns a service thread and re-queries the [`ksni::Tray`] impl for
//! icon/menu/status whenever we call `Handle::update`, so the backend just
//! swaps the stored snapshot in. Menu item activation runs on ksni's thread;
//! actions cross back to gpui over the same channel the other platforms use.
//!
//! On desktops without an SNI host (bare GNOME without the AppIndicator
//! extension) the spawn fails; the poll loop logs once and the app runs
//! without a tray.

use super::{action_from_id, icon, SpecItem, TrayAction, TraySnapshot};
use gpui::AsyncApp;

pub(super) struct Backend {
    handle: ksni::blocking::Handle<SniTray>,
}

impl Backend {
    /// Spawn the SNI service. Registration is a DBus round-trip, so it runs
    /// on the background executor rather than stalling the foreground poll
    /// loop; the returned handle is `Send` and lives with the poll task.
    pub(super) async fn create(
        tx: smol::channel::Sender<TrayAction>,
        cx: &AsyncApp,
    ) -> Option<Self> {
        cx.background_spawn(async move {
            use ksni::blocking::TrayMethods as _;
            let tray = SniTray {
                tx,
                snap: TraySnapshot::default(),
            };
            match tray.spawn() {
                Ok(handle) => Some(Backend { handle }),
                Err(e) => {
                    log::warn!("failed to register StatusNotifierItem: {e}");
                    None
                }
            }
        })
        .await
    }

    pub(super) fn update(&mut self, snap: &TraySnapshot) {
        let snap = snap.clone();
        // `update` re-reads menu/icon/status from the Tray impl and pushes
        // the changed properties over DBus. Returns None once the service is
        // gone (host died) — nothing to do about it here.
        self.handle.update(move |tray| tray.snap = snap);
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        // Dropping the handle alone leaves the service thread (and the icon)
        // alive; ask it to unregister. The awaiter is intentionally not
        // waited on — teardown can finish on ksni's thread.
        self.handle.shutdown();
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
