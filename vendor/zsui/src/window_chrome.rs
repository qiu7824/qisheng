//! Application-drawn window chrome.
//!
//! A window built with `custom_title_bar(true)` hands its caption area to the
//! View. Nodes marked with `ViewNode::window_drag_region` move the window
//! where no interactive target covers them, and caption buttons raise
//! [`ZsWindowCommand`]s through `AppCx::window_command`.

use serde::{Deserialize, Serialize};

/// A window-management request raised from a View update.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ZsWindowCommand {
    /// Minimizes the window to the taskbar or dock.
    Minimize,
    /// Maximizes a restored window and restores a maximized one.
    ToggleMaximize,
    /// Requests closing the window through the normal close path, so close
    /// requests can still be vetoed by the application.
    Close,
    /// Hides the window without closing it, for example to keep running
    /// from a notification-area icon.
    Hide,
    /// Shows, restores and activates the window.
    Show,
    /// Shows a system notification from the window's notification-area
    /// icon (a toast on current Windows). Requires a declared status item.
    /// `silent` suppresses the notification sound.
    Notify {
        title: String,
        body: String,
        silent: bool,
    },
}

/// The window placement an application-drawn title bar mirrors, for example
/// to show a restore button instead of maximize. Reported through
/// `ViewNode::on_window_state_change` whenever it changes, including
/// changes made by the system (caption double-click, snap, keyboard).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct ZsWindowState {
    pub maximized: bool,
    /// The restored bounds and maximized state, for saving the window's
    /// placement and restoring it with `NativeWindowBuilder::placement`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<crate::ZsWindowPlacement>,
}
