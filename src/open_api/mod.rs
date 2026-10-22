//! Bounded external control adapters use the application's existing command owner.
mod deeplink;
mod html_text;
mod protocol;
mod server;

pub use deeplink::{DeepLinkAction, DeepLinkMusicInfo, ParsedDeepLink, parse_deeplink};
pub use protocol::{DEFAULT_FILTER, Filter, Route, STATUS_FIELDS, parse_filter, parse_route};
use serde::{Deserialize, Serialize};
pub(crate) use server::OpenApiShutdownToken;
pub use server::{
    Authentication, BearerDigest, OpenApiServer, ServerConfig, ServerEvent, ServerState, ServerView,
};
use std::sync::{Arc, atomic::AtomicBool};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlayerState {
    #[serde(rename = "playing")]
    Playing,
    #[serde(rename = "paused")]
    Paused,
    #[serde(rename = "error")]
    Error,
    #[default]
    #[serde(rename = "stoped")]
    Stoped,
}
/// Field declaration order matches the fixed LX player's initial status object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LxPlayerStatus {
    pub status: PlayerState,
    pub name: String,
    pub singer: String,
    #[serde(rename = "albumName")]
    pub album_name: String,
    #[serde(rename = "picUrl")]
    pub pic_url: String,
    pub progress: f64,
    pub duration: f64,
    #[serde(rename = "playbackRate")]
    pub playback_rate: f64,
    #[serde(rename = "lyricLineText")]
    pub lyric_line_text: String,
    #[serde(rename = "lyricLineAllText")]
    pub lyric_line_all_text: String,
    pub lyric: String,
    pub tlyric: String,
    pub rlyric: String,
    pub lxlyric: String,
    pub collect: bool,
    pub volume: u8,
    pub mute: bool,
}
impl Default for LxPlayerStatus {
    fn default() -> Self {
        Self {
            status: PlayerState::Stoped,
            name: String::new(),
            singer: String::new(),
            album_name: String::new(),
            pic_url: String::new(),
            progress: 0.0,
            duration: 0.0,
            playback_rate: 1.0,
            lyric_line_text: String::new(),
            lyric_line_all_text: String::new(),
            lyric: String::new(),
            tlyric: String::new(),
            rlyric: String::new(),
            lxlyric: String::new(),
            collect: false,
            volume: 0,
            mute: false,
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub enum ControlAction {
    Play,
    Pause,
    Toggle,
    Previous,
    Next,
    Stop,
    Seek {
        seconds: f64,
    },
    SeekChecked {
        requested_seconds: f64,
        seconds: f64,
    },
    CollectCurrent {
        collected: bool,
    },
    DislikeCurrent,
    SetVolume {
        percent: u8,
    },
    SetMute {
        muted: bool,
    },
    ToggleMute,
    SeekRelative {
        seconds: f64,
    },
    VolumeRelative {
        percent: i8,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlErrorKind {
    InvalidData,
    Unauthorized,
    Busy,
    Closed,
    Timeout,
    Unavailable,
    CursorExpired,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlError {
    pub kind: ControlErrorKind,
    pub message: String,
}
impl ControlError {
    pub fn new(kind: ControlErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into().chars().take(1024).collect(),
        }
    }
}
impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ControlError {}
pub type ControlResult<T> = Result<T, ControlError>;
#[derive(Clone, Debug)]
pub struct StatusSnapshot {
    pub status: LxPlayerStatus,
    pub cursor: u64,
}
#[derive(Clone, Debug)]
pub struct StatusUpdate {
    pub cursor: u64,
    pub status: LxPlayerStatus,
}
/// A same-endpoint reference to already verified artwork for the current song.
/// Its hashes identify data and permission metadata; they are not credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtworkReference {
    pub session_epoch: u64,
    pub generation: u64,
    pub key_sha256: String,
    pub sha256: String,
}
impl ArtworkReference {
    pub fn path(&self) -> ControlResult<String> {
        self.validate()?;
        Ok(format!(
            "/artwork/v1/{}/{}/{}/{}",
            self.session_epoch, self.generation, self.key_sha256, self.sha256
        ))
    }
    pub(crate) fn validate(&self) -> ControlResult<()> {
        if self.session_epoch == 0
            || [&self.key_sha256, &self.sha256].iter().any(|hash| {
                hash.len() != 64
                    || !hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
        {
            return Err(protocol::invalid("Invalid artwork reference"));
        }
        Ok(())
    }
}
/// The bytes share the verified picture cache allocation. Retirement invalidates
/// both queued and partially transmitted responses without another image GET.
#[derive(Clone)]
pub struct ArtworkResponse {
    pub mime_type: &'static str,
    pub bytes: Arc<[u8]>,
    pub retired: Arc<AtomicBool>,
}
/// An adapter must sample state and cursor at one application-owner sequence point.
pub trait ExternalControlBackend: Send + Sync + 'static {
    fn snapshot(&self) -> ControlResult<StatusSnapshot>;
    fn updates_after(&self, cursor: u64) -> ControlResult<Vec<StatusUpdate>>;
    fn artwork(&self, _reference: ArtworkReference) -> ControlResult<ArtworkResponse> {
        Err(ControlError::new(
            ControlErrorKind::Unavailable,
            "Current verified artwork is unavailable",
        ))
    }
    /// Session validity and current-song/duration selection are checked atomically
    /// on the application's command owner, immediately before a real effect.
    fn control(&self, session_epoch: u64, action: ControlAction) -> ControlResult<()>;
}
