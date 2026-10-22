//! Desktop integration a View declares or observes: the system media session
//! (media keys, the volume overlay and lock screen), taskbar thumbnail
//! buttons and progress, files dropped on the window, a second launch of a
//! single-instance application, system theme changes, and window placement
//! to restore on the next start.
//!
//! Declarations follow the same pattern as tray icons and shortcuts: every
//! rebuild of the View may change them, and a View without a declaration
//! removes it. Hosts without a platform equivalent ignore them.

use crate::{Command, ZsuiThemeMode};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// What the system media session shows and allows: media keys, Bluetooth
/// headset buttons, the volume flyout and the lock screen all act on it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ZsMediaSession {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub playing: bool,
    pub can_previous: bool,
    pub can_next: bool,
    /// Encoded artwork (PNG or JPEG), shown by the system where supported.
    pub artwork: Option<std::sync::Arc<Vec<u8>>>,
    /// Playback position and length in seconds, for timeline displays.
    pub position: Option<f64>,
    pub duration: Option<f64>,
}

impl ZsMediaSession {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            can_previous: true,
            can_next: true,
            ..Self::default()
        }
    }

    pub fn artist(mut self, artist: impl Into<String>) -> Self {
        self.artist = artist.into();
        self
    }

    pub fn album(mut self, album: impl Into<String>) -> Self {
        self.album = album.into();
        self
    }

    pub fn playing(mut self, playing: bool) -> Self {
        self.playing = playing;
        self
    }

    pub fn can_previous(mut self, enabled: bool) -> Self {
        self.can_previous = enabled;
        self
    }

    pub fn can_next(mut self, enabled: bool) -> Self {
        self.can_next = enabled;
        self
    }

    pub fn artwork(mut self, encoded: std::sync::Arc<Vec<u8>>) -> Self {
        self.artwork = Some(encoded);
        self
    }

    pub fn timeline(mut self, position: f64, duration: f64) -> Self {
        if position.is_finite() && duration.is_finite() && duration > 0.0 {
            self.position = Some(position.clamp(0.0, duration));
            self.duration = Some(duration);
        }
        self
    }
}

/// A transport request from the system media session or a media key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ZsMediaCommand {
    Play,
    Pause,
    /// A play/pause key that toggles.
    TogglePlayPause,
    Stop,
    Next,
    Previous,
}

/// Buttons and progress on the window's taskbar entry.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ZsTaskbarSpec {
    /// Up to seven buttons under the taskbar thumbnail. Their commands reach
    /// the window's application-command mapper like menu commands.
    pub buttons: Vec<ZsTaskbarButton>,
    pub progress: ZsTaskbarProgress,
}

impl ZsTaskbarSpec {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn button(mut self, button: ZsTaskbarButton) -> Self {
        if self.buttons.len() < 7 {
            self.buttons.push(button);
        }
        self
    }

    pub fn progress(mut self, progress: ZsTaskbarProgress) -> Self {
        self.progress = progress;
        self
    }
}

/// One taskbar thumbnail button, drawn from an icon-font glyph.
#[derive(Debug, Clone, PartialEq)]
pub struct ZsTaskbarButton {
    pub glyph: char,
    pub tooltip: String,
    pub command: Command,
    pub enabled: bool,
}

impl ZsTaskbarButton {
    pub fn new(glyph: char, tooltip: impl Into<String>, command: Command) -> Self {
        Self {
            glyph,
            tooltip: tooltip.into(),
            command,
            enabled: true,
        }
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}

/// The progress shown on the taskbar button.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ZsTaskbarProgress {
    #[default]
    None,
    Indeterminate,
    /// A fraction between 0 and 1.
    Normal(f32),
    Paused(f32),
    Error(f32),
}

/// A system notification a View declares, for example "now playing" while
/// the window is hidden. The host shows it once each time a new `key`
/// appears, so a View declares one only while there is something new to
/// tell and keeps the key stable while it stays declared. On Windows it is
/// shown from the window's notification-area icon, which the View must also
/// declare.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ZsNotification {
    pub key: u64,
    pub title: String,
    pub body: String,
    /// Shows the notification without the system sound, for example while
    /// audio is playing.
    pub silent: bool,
}

impl ZsNotification {
    pub fn new(key: u64, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            key,
            title: title.into(),
            body: body.into(),
            silent: false,
        }
    }

    pub fn silent(mut self, silent: bool) -> Self {
        self.silent = silent;
        self
    }
}

/// Something the host reports to the View outside ordinary input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ZsHostEvent {
    /// The system switched between light and dark app themes.
    SystemThemeChanged(ZsuiThemeMode),
    /// Files or folders were dropped on the window.
    FilesDropped(Vec<PathBuf>),
    /// The application was launched again while this instance runs; the
    /// new launch's arguments, without the program path.
    SecondInstance(Vec<String>),
    /// The system media session or a media key requested a transport action.
    Media(ZsMediaCommand),
}

/// A window's restored position and size in screen pixels and whether it
/// is maximized, to save on exit and restore on the next start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct ZsWindowPlacement {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
}

impl ZsWindowPlacement {
    /// A compact text form for settings files: `x,y,width,height,maximized`.
    pub fn to_text(&self) -> String {
        format!(
            "{},{},{},{},{}",
            self.x,
            self.y,
            self.width,
            self.height,
            u8::from(self.maximized)
        )
    }

    pub fn from_text(text: &str) -> Option<Self> {
        let mut parts = text.split(',').map(str::trim);
        let mut next = || parts.next()?.parse::<i32>().ok();
        let placement = Self {
            x: next()?,
            y: next()?,
            width: next()?,
            height: next()?,
            maximized: next()? != 0,
        };
        (placement.width > 0 && placement.height > 0).then_some(placement)
    }
}

/// Handlers a View installs for [`ZsHostEvent`]s; the first handler in
/// preorder receives each event.
#[derive(Debug, Clone)]
pub(crate) struct ViewHostHandlers<Msg> {
    pub(crate) theme: Option<fn(ZsuiThemeMode) -> Msg>,
    pub(crate) files: Option<fn(Vec<PathBuf>) -> Msg>,
    pub(crate) instance: Option<fn(Vec<String>) -> Msg>,
    pub(crate) media: Option<fn(ZsMediaCommand) -> Msg>,
}

impl<Msg> Default for ViewHostHandlers<Msg> {
    fn default() -> Self {
        Self {
            theme: None,
            files: None,
            instance: None,
            media: None,
        }
    }
}

impl<Msg> ViewHostHandlers<Msg> {
    pub(crate) fn message(&self, event: &ZsHostEvent) -> Option<Msg> {
        match event {
            ZsHostEvent::SystemThemeChanged(mode) => self.theme.map(|f| f(*mode)),
            ZsHostEvent::FilesDropped(paths) => self.files.map(|f| f(paths.clone())),
            ZsHostEvent::SecondInstance(args) => self.instance.map(|f| f(args.clone())),
            ZsHostEvent::Media(command) => self.media.map(|f| f(*command)),
        }
    }

    pub(crate) fn accepts_files(&self) -> bool {
        self.files.is_some()
    }
}

/// What a View declares for the host to apply after each rebuild.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ZsHostDeclarations {
    pub media_session: Option<ZsMediaSession>,
    pub taskbar: Option<ZsTaskbarSpec>,
    pub notification: Option<ZsNotification>,
    pub accepts_files: bool,
}

impl ZsHostDeclarations {
    /// The notification to show after `previous` was applied: a declared
    /// notification whose key was not declared before.
    pub fn new_notification(&self, previous: &Self) -> Option<&ZsNotification> {
        self.notification.as_ref().filter(|notification| {
            previous
                .notification
                .as_ref()
                .is_none_or(|shown| shown.key != notification.key)
        })
    }
}

/// Forwards this launch to a running instance of the application `id` and
/// reports whether it did. Call it early in `main`, before opening files or
/// databases the running instance owns; when it returns `true`, exit. The
/// first instance claims `id` here and keeps it until it exits; build its
/// window with `NativeWindowBuilder::single_instance(id)` so a second launch
/// finds it and brings it to the front.
pub fn zs_forward_to_running_instance(id: &str) -> bool {
    #[cfg(all(windows, feature = "windows-win32"))]
    {
        crate::windows_win32_host::zs_forward_to_running_instance_win32(id)
    }
    #[cfg(not(all(windows, feature = "windows-win32")))]
    {
        let _ = id;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_placement_round_trips_through_text() {
        let placement = ZsWindowPlacement {
            x: -8,
            y: 40,
            width: 1280,
            height: 800,
            maximized: true,
        };
        assert_eq!(
            ZsWindowPlacement::from_text(&placement.to_text()),
            Some(placement)
        );
        assert_eq!(ZsWindowPlacement::from_text("1,2,0,4,0"), None);
        assert_eq!(ZsWindowPlacement::from_text("garbage"), None);
    }

    #[test]
    fn host_handlers_route_each_event_to_its_handler() {
        let handlers = ViewHostHandlers::<String> {
            theme: Some(|mode| format!("theme:{mode:?}")),
            files: Some(|paths| format!("files:{}", paths.len())),
            instance: None,
            media: Some(|command| format!("media:{command:?}")),
        };
        assert_eq!(
            handlers.message(&ZsHostEvent::FilesDropped(vec![PathBuf::from("a.mp3")])),
            Some("files:1".to_string())
        );
        assert_eq!(
            handlers.message(&ZsHostEvent::Media(ZsMediaCommand::Next)),
            Some("media:Next".to_string())
        );
        assert_eq!(
            handlers.message(&ZsHostEvent::SecondInstance(Vec::new())),
            None
        );
        assert!(handlers.accepts_files());
    }

    #[test]
    fn notifications_show_once_per_key() {
        let declared = |key: Option<u64>| ZsHostDeclarations {
            notification: key.map(|key| ZsNotification::new(key, "Now playing", "A song")),
            ..ZsHostDeclarations::default()
        };
        assert!(declared(Some(1))
            .new_notification(&declared(None))
            .is_some());
        assert!(declared(Some(1))
            .new_notification(&declared(Some(1)))
            .is_none());
        assert!(declared(Some(2))
            .new_notification(&declared(Some(1)))
            .is_some());
        assert!(declared(None)
            .new_notification(&declared(Some(1)))
            .is_none());
    }

    #[test]
    fn taskbar_keeps_at_most_seven_buttons() {
        let mut spec = ZsTaskbarSpec::new();
        for index in 0..9 {
            spec = spec.button(ZsTaskbarButton::new(
                'x',
                format!("{index}"),
                Command::custom(format!("b{index}")),
            ));
        }
        assert_eq!(spec.buttons.len(), 7);
    }
}
