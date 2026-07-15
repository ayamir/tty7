//! Persisted last-window geometry, stored at `window.json` in the config dir
//! (alongside `config.json` / `session.json`). The quit hook in `ui::app`
//! writes the window's final bounds here unconditionally; startup reads it
//! back only when `Config::remember_window_size` is on, so toggling the
//! setting off and on again still restores the most recent quit's geometry.
//! Same durability contract as the other config-dir files: missing/malformed
//! reads fall back to "nothing remembered", writes are atomic.

use gpui::{Bounds, Pixels, point, px};
use serde::{Deserialize, Serialize};

/// Don't restore a window smaller than this (logical px) — a corrupt or
/// hand-edited file shouldn't reopen tty7 as a sliver.
const MIN_SIZE: f32 = 200.0;

/// Last known window geometry, in gpui's global coordinate space (logical
/// pixels; origins can be negative or beyond the primary display on
/// multi-monitor setups). For a fullscreen window this records the *restore*
/// bounds, so the next normal launch isn't screen-sized.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl WindowState {
    fn path() -> Option<std::path::PathBuf> {
        crate::core::config::config_path("window.json")
    }

    pub fn from_bounds(bounds: Bounds<Pixels>) -> Self {
        Self {
            x: bounds.origin.x.into(),
            y: bounds.origin.y.into(),
            width: bounds.size.width.into(),
            height: bounds.size.height.into(),
        }
    }

    pub fn bounds(&self) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(self.x), px(self.y)),
            size: gpui::size(px(self.width), px(self.height)),
        }
    }

    /// Load the remembered geometry; `None` when nothing usable is on disk
    /// (never saved, unreadable, malformed, or degenerate values), in which
    /// case the caller falls back to the centered default.
    pub fn load() -> Option<Self> {
        let path = Self::path()?;
        let text = std::fs::read_to_string(&path).ok()?;
        let state: Self = serde_json::from_str(&text)
            .map_err(|e| log::warn!("failed to parse {}: {e}; ignoring", path.display()))
            .ok()?;
        state.is_usable().then_some(state)
    }

    /// A geometry worth restoring: all values finite and the size at least
    /// [`MIN_SIZE`] each way.
    fn is_usable(&self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|v| v.is_finite())
            && self.width >= MIN_SIZE
            && self.height >= MIN_SIZE
    }

    /// Persist the geometry; IO / serialization errors are logged and swallowed
    /// (worst case the next launch opens at the default size).
    pub fn save(&self) {
        let Some(path) = Self::path() else {
            return;
        };
        let json = match serde_json::to_string_pretty(self) {
            Ok(j) => j,
            Err(e) => {
                log::warn!("failed to serialize window state: {e}");
                return;
            }
        };
        if let Err(e) = crate::core::config::write_atomic(&path, json.as_bytes()) {
            log::warn!("failed to write {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_bounds() {
        let state = WindowState {
            x: -120.5,
            y: 42.0,
            width: 1440.0,
            height: 900.0,
        };
        assert_eq!(WindowState::from_bounds(state.bounds()), state);
    }

    #[test]
    fn rejects_degenerate_geometry() {
        let usable =
            |json: &str| serde_json::from_str::<WindowState>(json).is_ok_and(|s| s.is_usable());
        assert!(usable(r#"{"x":-120.5,"y":42,"width":1440,"height":900}"#));
        assert!(!usable(r#"{"x":0,"y":0,"width":50,"height":900}"#));
        assert!(!usable(r#"{"x":null,"y":0,"width":1440,"height":900}"#));
        assert!(!usable("not json"));
    }
}
