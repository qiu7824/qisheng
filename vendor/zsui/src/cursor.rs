//! Pointer cursor shapes a View requests for the area under the pointer.

use serde::{Deserialize, Serialize};

/// A pointer cursor shape. Hosts map each shape to the platform cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ZsCursor {
    /// The platform's default arrow.
    #[default]
    Arrow,
    /// A hand, for links and custom-drawn clickable regions.
    Pointer,
    /// An I-beam over editable or selectable text.
    Text,
    /// Four-way arrows for moving something.
    Move,
    /// Left-right arrows for horizontal resizing or sliders.
    ResizeHorizontal,
    /// Up-down arrows for vertical resizing.
    ResizeVertical,
    /// The action under the pointer is unavailable.
    NotAllowed,
    /// Work is in progress but the UI stays usable.
    Progress,
}
