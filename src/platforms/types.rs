use crate::domain::music::TrackRecord;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Kw,
    Tx,
    Kg,
    Wy,
    Mg,
}
impl Platform {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Kw => "kw",
            Self::Tx => "tx",
            Self::Kg => "kg",
            Self::Wy => "wy",
            Self::Mg => "mg",
        }
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommentKind {
    Normal,
    Hot,
    Replies,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum ProviderRequest {
    Search {
        keyword: String,
        page: u32,
        limit: u32,
    },
    HotSearch,
    Suggest {
        keyword: String,
    },
    Detail {
        ids: Vec<String>,
    },
    Charts,
    ChartTracks {
        id: String,
        page: u32,
        limit: u32,
    },
    PlaylistTags,
    Playlists {
        tag: String,
        sort: String,
        page: u32,
        limit: u32,
    },
    SearchPlaylists {
        keyword: String,
        page: u32,
        limit: u32,
    },
    PlaylistDetail {
        id: String,
        page: u32,
        limit: u32,
    },
    Album {
        id: String,
        page: u32,
        limit: u32,
    },
    Comments {
        id: String,
        kind: CommentKind,
        page: u32,
        limit: u32,
        cursor: Option<String>,
        root_comment_id: Option<String>,
    },
    Lyrics {
        track: TrackRecord,
        word_by_word: bool,
    },
    Picture {
        track: TrackRecord,
    },
}
impl ProviderRequest {
    pub fn operation(&self) -> &'static str {
        match self {
            Self::Search { .. } => "search",
            Self::HotSearch => "hot_search",
            Self::Suggest { .. } => "suggest",
            Self::Detail { .. } => "detail",
            Self::Charts => "charts",
            Self::ChartTracks { .. } => "chart_tracks",
            Self::PlaylistTags => "playlist_tags",
            Self::Playlists { .. } => "playlists",
            Self::SearchPlaylists { .. } => "search_playlists",
            Self::PlaylistDetail { .. } => "playlist_detail",
            Self::Album { .. } => "album",
            Self::Comments { .. } => "comments",
            Self::Lyrics { .. } => "lyrics",
            Self::Picture { .. } => "picture",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: u64,
    pub page: u32,
    pub limit: u32,
    pub next_cursor: Option<String>,
}
impl<T> Page<T> {
    pub fn new(items: Vec<T>, total: u64, page: u32, limit: u32) -> Self {
        Self {
            items,
            total,
            page,
            limit,
            next_cursor: None,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Chart {
    pub id: String,
    pub name: String,
    pub image_url: Option<String>,
    pub description: Option<String>,
    pub extra: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PlaylistTag {
    pub id: String,
    pub name: String,
    pub group: String,
    pub extra: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub image_url: Option<String>,
    pub author: Option<String>,
    pub description: Option<String>,
    pub play_count: Option<u64>,
    pub track_count: Option<u64>,
    pub extra: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Collection {
    pub info: Playlist,
    pub tracks: Page<TrackRecord>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Comment {
    pub id: String,
    pub text: String,
    pub author: String,
    pub avatar_url: Option<String>,
    pub timestamp_ms: Option<u64>,
    pub likes: u64,
    pub replies: Vec<Comment>,
    pub extra: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct Lyrics {
    pub lyric: String,
    pub translated: Option<String>,
    pub romanized: Option<String>,
    pub word_by_word: Option<String>,
    pub extra: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ProviderResponse {
    Tracks(Page<TrackRecord>),
    Keywords(Vec<String>),
    Charts(Vec<Chart>),
    PlaylistTags(Vec<PlaylistTag>),
    Playlists(Page<Playlist>),
    Collection(Collection),
    Comments(Page<Comment>),
    Lyrics(Lyrics),
    Picture(String),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderError {
    Unsupported {
        platform: Platform,
        operation: String,
    },
    Cancelled,
    DeadlineExceeded,
    InvalidRequest {
        message: String,
    },
    Network {
        message: String,
    },
    Http {
        status: u16,
        message: String,
    },
    ResponseTooLarge {
        limit: usize,
    },
    Parse {
        message: String,
    },
    Upstream {
        message: String,
    },
}
impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ProviderError {}
impl ProviderError {
    pub fn parse(message: impl Into<String>) -> Self {
        Self::Parse {
            message: message.into(),
        }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidRequest {
            message: message.into(),
        }
    }
    pub fn upstream(message: impl Into<String>) -> Self {
        Self::Upstream {
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProviderContext {
    pub cancel: Arc<AtomicBool>,
    pub deadline: Instant,
}
impl ProviderContext {
    pub fn new(timeout: Duration) -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            deadline: Instant::now() + timeout,
        }
    }
    pub fn check(&self) -> Result<(), ProviderError> {
        if self.cancel.load(Ordering::Acquire) {
            Err(ProviderError::Cancelled)
        } else if Instant::now() >= self.deadline {
            Err(ProviderError::DeadlineExceeded)
        } else {
            Ok(())
        }
    }
    pub fn remaining(&self) -> Result<Duration, ProviderError> {
        self.check()?;
        Ok(self.deadline.saturating_duration_since(Instant::now()))
    }
}
impl Default for ProviderContext {
    fn default() -> Self {
        Self::new(Duration::from_secs(20))
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub follow_redirects: bool,
}
impl HttpRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: "GET".into(),
            url: url.into(),
            headers: BTreeMap::new(),
            body: Vec::new(),
            follow_redirects: true,
        }
    }
    pub fn post(url: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            method: "POST".into(),
            url: url.into(),
            headers: BTreeMap::new(),
            body,
            follow_redirects: true,
        }
    }
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }
    pub fn without_redirects(mut self) -> Self {
        self.follow_redirects = false;
        self
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub bytes: Vec<u8>,
}
impl HttpResponse {
    pub fn json(&self) -> Result<Value, ProviderError> {
        serde_json::from_slice(&self.bytes).map_err(|e| ProviderError::parse(e.to_string()))
    }
    pub fn text(&self) -> Result<String, ProviderError> {
        String::from_utf8(self.bytes.clone()).map_err(|e| ProviderError::parse(e.to_string()))
    }
}
pub trait HttpTransport: Send + Sync {
    fn execute(
        &self,
        request: HttpRequest,
        context: &ProviderContext,
    ) -> Result<HttpResponse, ProviderError>;
}
pub trait Provider: Send + Sync {
    fn platform(&self) -> Platform;
    fn execute(
        &self,
        request: ProviderRequest,
        context: &ProviderContext,
    ) -> Result<ProviderResponse, ProviderError>;
}

pub fn encode(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
pub fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}
pub fn number(value: &Value) -> u64 {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}
pub fn interval(seconds: u64) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}
pub fn page_args(page: u32, limit: u32) -> Result<(u32, u32), ProviderError> {
    if page == 0 || limit == 0 || limit > 1000 {
        return Err(ProviderError::invalid(
            "page must be >= 1 and limit must be 1..=1000",
        ));
    }
    Ok((page, limit))
}
pub fn json(
    transport: &dyn HttpTransport,
    request: HttpRequest,
    context: &ProviderContext,
) -> Result<Value, ProviderError> {
    context.check()?;
    let response = transport.execute(request, context)?;
    context.check()?;
    if !(200..300).contains(&response.status) {
        return Err(ProviderError::Http {
            status: response.status,
            message: "platform request failed".into(),
        });
    }
    response.json()
}
