//! Menu / keyboard actions, defined in one place so both the application shell
//! (`app.rs`) and the terminal view (`terminal::view`) can reference them
//! without depending on each other. They drive the macOS menu bar and the
//! keymap, so a click and a shortcut go through exactly the same path.

use gpui::actions;

actions!(
    tty7,
    [
        NewTab,
        CloseActiveTab,
        SplitRight,
        SplitDown,
        FocusNextPane,
        FocusPrevPane,
        // Directional pane focus (tmux `prefix ←/→/↑/↓`): move focus to the
        // adjacent pane in that direction.
        FocusPaneLeft,
        FocusPaneRight,
        FocusPaneUp,
        FocusPaneDown,
        // Grow (Right/Down) or shrink (Left/Up) the focused pane along the
        // matching axis by nudging its nearest enclosing split's ratio.
        ResizePaneLeft,
        ResizePaneRight,
        ResizePaneUp,
        ResizePaneDown,
        // Swap the focused pane with its next / previous sibling in leaf order
        // (tmux `prefix }` / `prefix {`); focus follows the moved pane.
        SwapPaneNext,
        SwapPanePrev,
        // Relative tab navigation (tmux `prefix n` / `prefix p`).
        NextTab,
        PrevTab,
        // Jump straight to tab 1‑9 (⌘/Ctrl+1‑9, tmux `prefix 1‑9`). Unit actions
        // rather than one parameterized action so config/Settings can index them
        // by name like every other binding.
        ActivateTab1,
        ActivateTab2,
        ActivateTab3,
        ActivateTab4,
        ActivateTab5,
        ActivateTab6,
        ActivateTab7,
        ActivateTab8,
        ActivateTab9,
        IncreaseFontSize,
        DecreaseFontSize,
        ResetFontSize,
        TogglePalette,
        ReopenClosedTab,
        ToggleMaximizePane,
        ToggleFullscreen,
        // Switch the tab bar between the horizontal title-bar strip and the
        // vertical left-side sidebar (persists `tab_bar_position`).
        ToggleTabSidebar,
        // Collapse/expand the left tab sidebar in place (persists
        // `sidebar_collapsed`). Unlike `ToggleTabSidebar` this does not switch
        // the tab bar to the horizontal strip — the rail just goes away and
        // comes back at the same width.
        ToggleLeftPanel,
        // Show/hide the right detail panel — session info, working-tree changes,
        // and the file tree (persists `right_panel_visible`).
        ToggleRightPanel,
        // Jump straight to one of the right panel's tabs, opening the panel if
        // it was closed. Unit actions rather than one parameterized action so
        // config/Settings can bind them by name; unbound by default, since the
        // panel's own tab row is the primary way in.
        ShowRightPanelInfo,
        ShowRightPanelOutline,
        ShowRightPanelChanges,
        ShowRightPanelFiles,
        OpenSettings,
        RestartDaemon,
        // Toggle the SFTP file panel for the focused native-SSH pane (WS5).
        ToggleSftp,
        // Toggle the code panel: a full-body overlay of [file tree | editor]
        // covering the terminal (settings-overlay style).
        ToggleCodePanel,
        // Save the editor panel's active file (⌘S).
        EditorSave,
        // Open the SSH profile manager/editor full-window page (WS6, FR-P1).
        OpenSshProfiles,
        // Reconnect a dead native-SSH pane in place (WS6, FR-E4).
        RestartSshSession,
        SendTab,
        SendBackTab,
        Quit
    ]
);
