use std::sync::{Arc, Mutex, mpsc};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Page {
    #[default]
    Library,
    Sources,
    Queue,
}

#[derive(Clone, Debug)]
pub enum UiCommand {
    Navigate(Page),
    Search(String),
    ImportSource,
    ImportAudio,
    AddDemoSource,
    ApproveSource(String),
    EnableSource(String),
    DisableSource,
    DeleteSource(String),
    Play(String),
    Toggle,
    Previous,
    Next,
    Seek(f32),
    Volume(f32),
    ClearError,
    ApprovePlaybackUrl,
    CancelPlaybackUrl,
}

#[derive(Clone, Debug, Default)]
pub struct TrackView {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: f32,
    pub cover_color: u32,
}

#[derive(Clone, Debug, Default)]
pub struct SourceView {
    pub id: String,
    pub name: String,
    pub version: String,
    pub author: String,
    pub description: String,
    pub hash: String,
    pub enabled: bool,
    pub approved: bool,
    pub status: String,
}

#[derive(Clone, Debug)]
pub struct UiSnapshot {
    pub page: Page,
    pub tracks: Vec<TrackView>,
    pub queue: Vec<TrackView>,
    pub sources: Vec<SourceView>,
    pub query: String,
    pub total_tracks: usize,
    pub now_playing: Option<TrackView>,
    pub status: String,
    pub playing: bool,
    pub busy: bool,
    pub position: f32,
    pub duration: f32,
    pub volume: f32,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub pending_playback_url: Option<String>,
}

impl Default for UiSnapshot {
    fn default() -> Self {
        Self {
            page: Page::Library,
            tracks: vec![],
            queue: vec![],
            sources: vec![],
            query: String::new(),
            total_tracks: 0,
            now_playing: None,
            status: "就绪".into(),
            playing: false,
            busy: false,
            position: 0.0,
            duration: 0.0,
            volume: 0.7,
            error: None,
            notice: None,
            pending_playback_url: None,
        }
    }
}

pub type SharedSnapshot = Arc<Mutex<UiSnapshot>>;
pub type CommandSender = mpsc::SyncSender<UiCommand>;
