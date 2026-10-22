use crate::{core::Command, menu::MenuSpec};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraySpec {
    pub tooltip: Option<String>,
    pub icon_path: Option<String>,
    pub menu: MenuSpec,
    /// Raised when the icon is clicked with the primary button, for example
    /// to bring the main window back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activate: Option<Command>,
}

impl TraySpec {
    pub fn new() -> Self {
        Self {
            tooltip: None,
            icon_path: None,
            menu: MenuSpec::new(),
            activate: None,
        }
    }

    /// The command raised by a primary click on the icon.
    pub fn on_activate(mut self, command: Command) -> Self {
        self.activate = Some(command);
        self
    }

    pub fn tooltip(mut self, tooltip: impl Into<String>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }

    pub fn icon_path(mut self, icon_path: impl Into<String>) -> Self {
        self.icon_path = Some(icon_path.into());
        self
    }

    pub fn menu(mut self, menu: MenuSpec) -> Self {
        self.menu = menu;
        self
    }

    pub fn item(mut self, label: impl Into<String>, command: Command) -> Self {
        self.menu = self.menu.item(label, command);
        self
    }

    pub fn separator(mut self) -> Self {
        self.menu = self.menu.separator();
        self
    }
}

impl Default for TraySpec {
    fn default() -> Self {
        Self::new()
    }
}
