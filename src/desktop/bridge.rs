//! The backend bridge: one worker thread owns every backend call.
//!
//! The UI thread never blocks on the backend. It sends [`Job`]s; the worker
//! executes them, mirrors the results into the shared [`Store`] and asks the
//! window to rebuild through ZSUI's coalescing invalidation handle. While idle
//! the worker only watches the backend snapshot and event journal.

mod integrations;
pub use integrations::ApiView;
use integrations::PendingImport;

use crate::art::{self, ArtImage};
use crate::art_cache::ByteLru;
use lx_gpui_player::{
    backend::{
        BackendCommand, BackendError, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, BackendSnapshot, CommandEnvelope, ProviderOperationState,
        QueueSnapshot,
    },
    domain::{
        artwork::PictureState,
        lists::{AddLocation, ListAction, ListInfo},
        music::TrackRecord,
    },
    platforms::{
        Chart, HttpRequest, HttpTransport, Platform, Playlist, PlaylistTag, ProviderContext,
        ProviderRequest, ProviderResponse, ReqwestTransport, TransportConfig,
    },
};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex, MutexGuard, Weak,
        atomic::{AtomicBool, AtomicU8, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use zsui::UiInvalidationHandle;

pub const SEARCH_PAGE: u32 = 30;
pub const CHART_PAGE: u32 = 100;
pub const PLAYLIST_PAGE: u32 = 30;
pub const PLAYLIST_TRACKS: u32 = 100;
const IMAGE_QUEUE_LIMIT: usize = 16;
const LIST_CHUNK: usize = 500;
const LIST_LIMIT: usize = 20_000;

/// How finely the worker should follow the playback clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TickRate {
    /// Seconds and coarse progress only.
    Coarse = 0,
    /// Every backend tick (lyrics karaoke, dragging).
    Fine = 1,
}

#[derive(Clone, Debug)]
pub enum Job {
    SetOpenApi {
        enabled: bool,
        port: u16,
    },
    ImportPlaylistFile(std::path::PathBuf),
    ImportPlaylistUrl {
        platform: Platform,
        input: String,
    },
    CommitPlaylistImport,
    CancelPlaylistImport,
    DialogError(String),
    Command(BackendCommand),
    /// A command whose success is announced with a toast.
    CommandNotice(BackendCommand, String),
    Search {
        platform: Platform,
        keyword: String,
        page: u32,
    },
    HotSearch(Platform),
    Charts(Platform),
    ChartTracks {
        platform: Platform,
        id: String,
        page: u32,
    },
    /// Replaces the temporary list with `tracks` and plays `track_id`.
    PlayTracks {
        tracks: Arc<Vec<TrackRecord>>,
        track_id: String,
    },
    AddToList {
        list_id: String,
        tracks: Vec<TrackRecord>,
    },
    RemoveFromList {
        list_id: String,
        ids: Vec<String>,
    },
    CreateList(String),
    RenameList {
        id: String,
        name: String,
    },
    DeleteList(String),
    LoadList(String),
    RefreshLists,
    RefreshQueue,
    RefreshSearchHistory,
    RefreshLibraryRoots,
    ApproveArt(String),
    ApproveAllArt(bool),
    AutoApproveAudio(bool),
    AddLibraryRoot(std::path::PathBuf),
    RescanLibrary,
    /// Loads a list and plays it from the first track.
    PlayList(String),
    ClearList(String),
    RefreshTimer,
    StartTimer {
        seconds: f64,
        wait_for_track_end: bool,
    },
    CancelTimer,
    SetLyricOffset(i64),
    /// Loads the artwork of a list member for lists and cards.
    ResolveArt {
        key: String,
        list_id: String,
        music_id: String,
    },
    /// Playlist categories of a platform.
    PlaylistTags(Platform),
    /// A page of a platform's playlists in category `tag` ("" for the
    /// recommended ones).
    Playlists {
        platform: Platform,
        tag: String,
        page: u32,
    },
    /// The tracks of an online playlist.
    PlaylistDetail {
        platform: Platform,
        id: String,
        page: u32,
    },
    SearchPlaylists {
        platform: Platform,
        keyword: String,
        page: u32,
    },
    /// Loads an online playlist and plays it from the first track.
    PlayPlaylist {
        platform: Platform,
        id: String,
    },
    /// Downloads a cover by URL, once covers may be loaded automatically.
    FetchImage(String),
    /// Saves tracks as a new playlist of the user.
    SaveAsList {
        name: String,
        tracks: Vec<TrackRecord>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

#[derive(Clone, Debug)]
pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub at: Instant,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Load {
    #[default]
    Idle,
    Running,
    Done,
    Failed(String),
}

#[derive(Clone, Debug, Default)]
pub struct SearchResults {
    pub platform: Option<Platform>,
    pub keyword: String,
    pub page: u32,
    pub total: u64,
    pub load: Load,
    pub tracks: Arc<Vec<TrackRecord>>,
}

#[derive(Clone, Debug, Default)]
pub struct ChartTracks {
    pub platform: Option<Platform>,
    pub id: String,
    pub total: u64,
    pub load: Load,
    pub tracks: Arc<Vec<TrackRecord>>,
}

#[derive(Clone, Debug, Default)]
pub struct ListData {
    pub load: Load,
    pub tracks: Arc<Vec<TrackRecord>>,
}

/// Loaded pages of a platform's playlists for one category.
#[derive(Clone, Debug, Default)]
pub struct PlaylistWall {
    pub load: Load,
    pub page: u32,
    pub total: u64,
    pub items: Arc<Vec<Playlist>>,
}

/// The open online playlist.
#[derive(Clone, Debug, Default)]
pub struct PlaylistView {
    pub platform: Option<Platform>,
    pub id: String,
    pub info: Option<Playlist>,
    pub total: u64,
    pub page: u32,
    pub load: Load,
    pub tracks: Arc<Vec<TrackRecord>>,
}

/// Playlists found by a search.
#[derive(Clone, Debug, Default)]
pub struct FoundLists {
    pub platform: Option<Platform>,
    pub keyword: String,
    pub page: u32,
    pub total: u64,
    pub load: Load,
    pub items: Arc<Vec<Playlist>>,
}

/// The artwork key of a downloaded image.
pub fn image_key(url: &str) -> String {
    format!("url:{url}")
}

#[derive(Clone, Debug)]
pub enum ArtState {
    Resolving,
    AwaitingApproval { operation_id: String },
    Ready(Arc<ArtImage>),
    Missing,
}

/// Shared master-image budget for both downloaded and local artwork.
pub struct ArtCache {
    cached: ByteLru<String, ArtState>,
    live: HashMap<String, Weak<ArtImage>>,
}

impl Default for ArtCache {
    fn default() -> Self {
        Self {
            cached: ByteLru::new(art::MASTER_CACHE_BYTES, 512),
            live: HashMap::new(),
        }
    }
}

impl ArtCache {
    pub fn get(&self, key: &str) -> Option<&ArtState> {
        self.cached.get(key)
    }
    fn contains_key(&self, key: &str) -> bool {
        self.cached.contains_key(key)
            || self
                .live
                .get(key)
                .is_some_and(|image| image.strong_count() > 0)
    }
    fn iter(&self) -> impl Iterator<Item = (&String, &ArtState)> {
        self.cached.iter()
    }
    pub(crate) fn remove(&mut self, key: &str) {
        self.cached.remove(key);
        self.live.remove(key);
    }
    pub(crate) fn insert(&mut self, key: String, state: ArtState) {
        let bytes = match &state {
            ArtState::Ready(image) => image.decoded_bytes(),
            _ => 0,
        };
        self.live.retain(|_, image| image.strong_count() > 0);
        if let ArtState::Ready(image) = &state {
            self.live.insert(key.clone(), Arc::downgrade(image));
        } else {
            self.live.remove(&key);
        }
        self.cached.insert(key, state, bytes);
    }

    pub fn image(&self, key: &str) -> Option<Arc<ArtImage>> {
        if let Some(ArtState::Ready(image)) = self.cached.get(key) {
            return Some(image.clone());
        }
        self.live.get(key).and_then(Weak::upgrade)
    }

    /// Transfer only images held by live views across a view-tree rebuild.
    pub fn live_images(&self) -> HashMap<String, Arc<ArtImage>> {
        self.live
            .iter()
            .filter_map(|(key, weak)| {
                let cache_owners = usize::from(self.cached.contains_key(key));
                (weak.strong_count() > cache_owners)
                    .then(|| weak.upgrade())
                    .flatten()
                    .map(|image| (key.clone(), image))
            })
            .collect()
    }
}

#[cfg(test)]
mod art_cache_tests {
    use super::*;

    #[test]
    fn live_cover_survives_keep_alive_eviction_without_another_request() {
        let data = include_bytes!("../../assets/lx-native-64.png");
        let visible = Arc::new(art::decode(data).unwrap());
        let another = Arc::new(art::decode(data).unwrap());
        let mut cache = ArtCache {
            cached: ByteLru::new(visible.decoded_bytes(), 4),
            live: HashMap::new(),
        };
        cache.insert("visible".into(), ArtState::Ready(visible.clone()));
        cache.insert("other".into(), ArtState::Ready(another));
        assert!(cache.get("visible").is_none());
        assert!(cache.contains_key("visible"));
        assert!(Arc::ptr_eq(&visible, &cache.image("visible").unwrap()));
        let replacement_view = cache.live_images();
        assert_eq!(replacement_view.len(), 1);
        drop(visible);
        assert!(cache.image("visible").is_some());
        drop(replacement_view);
        assert!(cache.image("visible").is_none());
        assert!(!cache.contains_key("visible"));
    }
}

/// Everything the UI renders that comes from the backend.
#[derive(Default)]
pub struct Store {
    pub api: ApiView,
    pub import_busy: bool,
    pub playlist_preview: Option<lx_gpui_player::backend::BackupPreviewView>,
    pub snapshot: BackendSnapshot,
    pub lists: Vec<ListInfo>,
    pub list_data: HashMap<String, ListData>,
    /// Track counts of every list, for the playlist wall.
    pub list_totals: HashMap<String, usize>,
    /// Favorite identity (`love_key`) -> id of the record in the love list.
    pub love: Arc<HashMap<String, String>>,
    pub queue: Option<Arc<QueueSnapshot>>,
    pub search: SearchResults,
    pub hot: HashMap<Platform, Vec<String>>,
    pub history: Vec<String>,
    pub charts: HashMap<Platform, Load>,
    pub chart_list: HashMap<Platform, Vec<Chart>>,
    pub chart: ChartTracks,
    pub art: ArtCache,
    pub toasts: VecDeque<Toast>,
    pub library_roots: Vec<lx_gpui_player::domain::library::LibraryRoot>,
    /// Bumped whenever list membership changes, to refresh derived views.
    pub lists_revision: u64,
    pub auto_art: bool,
    pub auto_approve_audio: bool,
    pub timer: Option<(
        lx_gpui_player::playback::timeout_stop::TimeoutStopState,
        Instant,
    )>,
    pub lyric_offset_ms: i64,
    pub started: bool,
    pub tags: HashMap<Platform, Load>,
    pub tag_list: HashMap<Platform, Vec<PlaylistTag>>,
    /// Playlist walls by platform and category.
    pub walls: HashMap<(Platform, String), PlaylistWall>,
    pub playlist: PlaylistView,
    pub found_lists: FoundLists,
}

pub struct Bridge {
    jobs: mpsc::Sender<Job>,
    store: Arc<Mutex<Store>>,
    tick: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Bridge {
    pub fn start(backend: BackendHandle, invalidate: UiInvalidationHandle) -> Self {
        let (jobs, receiver) = mpsc::channel();
        let store = Arc::new(Mutex::new(Store {
            snapshot: backend.snapshot(),
            ..Store::default()
        }));
        let tick = Arc::new(AtomicU8::new(TickRate::Coarse as u8));
        let stop = Arc::new(AtomicBool::new(false));
        let images = ImageFetcher::start(store.clone(), invalidate.clone());
        let mut worker = Worker {
            open_api: None,
            import: None,
            backend,
            store: store.clone(),
            invalidate,
            tick: tick.clone(),
            stop: stop.clone(),
            request: 0,
            cursor: 0,
            providers: HashMap::new(),
            pictures: HashMap::new(),
            pending_art: VecDeque::new(),
            signature: None,
            dirty: true,
            scan_after_add: false,
            approved_url: None,
            images,
            autoplay: None,
        };
        let worker = thread::Builder::new()
            .name("lx-desktop-bridge".into())
            .spawn(move || worker.run(receiver))
            .expect("spawn the desktop bridge");
        let bridge = Self {
            jobs,
            store,
            tick,
            stop,
            worker: Some(worker),
        };
        for job in [
            Job::RefreshLists,
            Job::RefreshQueue,
            Job::RefreshSearchHistory,
            Job::RefreshLibraryRoots,
            Job::RefreshTimer,
        ] {
            bridge.send(job);
        }
        bridge
    }

    /// A sender for jobs raised while views are laid out.
    pub fn sender(&self) -> mpsc::Sender<Job> {
        self.jobs.clone()
    }

    pub fn send(&self, job: Job) {
        let _ = self.jobs.send(job);
    }

    pub fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub fn shared_store(&self) -> Arc<Mutex<Store>> {
        self.store.clone()
    }

    pub fn set_tick(&self, rate: TickRate) {
        self.tick.store(rate as u8, Ordering::Relaxed);
    }

    pub fn toast(&self, text: impl Into<String>, kind: ToastKind) {
        push_toast(&mut self.store(), text.into(), kind);
    }

    /// Stops the worker and returns the backend handle's owner thread result.
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Store {
    /// What the walls and searches already know about a playlist.
    pub fn playlist_info(&self, platform: Platform, id: &str) -> Option<Playlist> {
        self.walls
            .iter()
            .filter(|((p, _), _)| *p == platform)
            .flat_map(|(_, wall)| wall.items.iter())
            .chain(
                (self.found_lists.platform == Some(platform))
                    .then(|| self.found_lists.items.iter())
                    .into_iter()
                    .flatten(),
            )
            .find(|list| list.id == id)
            .cloned()
    }

    /// Records a finished download under the shared byte budget.
    fn store_image(&mut self, key: String, state: ArtState) {
        self.art.insert(key, state);
    }
}

/// Downloads covers by URL on a few threads. Only covers the user allowed
/// to load automatically reach it (see `Job::FetchImage`).
struct ImageFetcher {
    queue: mpsc::SyncSender<(String, Instant)>,
}

impl ImageFetcher {
    fn start(store: Arc<Mutex<Store>>, invalidate: UiInvalidationHandle) -> Self {
        let (queue, receiver) = mpsc::sync_channel::<(String, Instant)>(IMAGE_QUEUE_LIMIT);
        let receiver = Arc::new(Mutex::new(receiver));
        let transport = Arc::new(
            ReqwestTransport::new(TransportConfig {
                proxy: None,
                max_response_bytes: 4 * 1024 * 1024,
                retries: 1,
            })
            .ok(),
        );
        for index in 0..2 {
            let receiver = receiver.clone();
            let store = store.clone();
            let invalidate = invalidate.clone();
            let transport = transport.clone();
            let _ = thread::Builder::new()
                .name(format!("lx-desktop-images-{index}"))
                .spawn(move || {
                    loop {
                        let (url, queued) = {
                            let receiver = receiver.lock().unwrap_or_else(|e| e.into_inner());
                            match receiver.recv() {
                                Ok(url) => url,
                                Err(_) => break,
                            }
                        };
                        // Old scroll requests must not delay the current viewport.
                        if queued.elapsed() > Duration::from_secs(3) {
                            store
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .art
                                .remove(&image_key(&url));
                            invalidate.request_rebuild();
                            continue;
                        }
                        let image = transport
                            .as_ref()
                            .as_ref()
                            .and_then(|transport| download_image(transport, &url));
                        let state = match image {
                            Some(image) => ArtState::Ready(Arc::new(image)),
                            None => ArtState::Missing,
                        };
                        store
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .store_image(image_key(&url), state);
                        invalidate.request_rebuild();
                    }
                });
        }
        Self { queue }
    }

    fn fetch(&self, url: String) -> bool {
        self.queue.try_send((url, Instant::now())).is_ok()
    }
}

/// Fetches and decodes one cover; small sizes are asked of the CDNs that
/// support them.
fn download_image(transport: &ReqwestTransport, url: &str) -> Option<ArtImage> {
    let mut url = url.trim().replace("{size}", "240");
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    if url.contains("music.126.net") && !url.contains('?') {
        url.push_str("?param=300y300");
    }
    let response = transport
        .execute(
            HttpRequest::get(url),
            &ProviderContext::new(Duration::from_secs(15)),
        )
        .ok()?;
    if !(200..300).contains(&response.status) {
        return None;
    }
    art::decode_thumb(&response.bytes).ok()
}

fn push_toast(store: &mut Store, text: String, kind: ToastKind) {
    if store
        .toasts
        .back()
        .is_some_and(|last| last.text == text && last.at.elapsed() < Duration::from_secs(2))
    {
        return;
    }
    store.toasts.push_back(Toast {
        text,
        kind,
        at: Instant::now(),
    });
    while store.toasts.len() > 4 {
        store.toasts.pop_front();
    }
}

#[derive(Clone, Debug)]
enum Purpose {
    Search {
        platform: Platform,
        keyword: String,
        page: u32,
    },
    Hot(Platform),
    Charts(Platform),
    ChartTracks {
        id: String,
        page: u32,
    },
    Tags(Platform),
    Playlists {
        platform: Platform,
        tag: String,
        page: u32,
    },
    PlaylistDetail {
        id: String,
        page: u32,
    },
    SearchPlaylists {
        platform: Platform,
        keyword: String,
        page: u32,
    },
}

/// What a change in the snapshot means for the UI.
#[derive(Clone, PartialEq)]
struct Signature {
    revision: u64,
    status: lx_gpui_player::audio::PlaybackStatus,
    generation: u64,
    track: Option<String>,
    position_bucket: i64,
    duration: i64,
    volume: u32,
    busy: bool,
    error: Option<String>,
    notice: Option<String>,
    pending: Option<String>,
    lyrics: Option<(String, u64)>,
    frame: Option<(Option<usize>, Option<usize>, i64)>,
    lyrics_error: Option<String>,
    sources: usize,
    tracks_ptr: usize,
    library: (usize, usize, usize, u64),
    equalizer: [i64; 10],
    processing: (i64, bool, i64),
    output: (u64, usize, bool, bool),
}

struct Worker {
    open_api: Option<lx_gpui_player::backend::BackendOpenApiServer>,
    import: Option<PendingImport>,
    backend: BackendHandle,
    store: Arc<Mutex<Store>>,
    invalidate: UiInvalidationHandle,
    tick: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
    request: u64,
    cursor: u64,
    providers: HashMap<String, Purpose>,
    /// Picture operation id -> artwork key.
    pictures: HashMap<String, String>,
    pending_art: VecDeque<(String, String, String)>,
    signature: Option<Signature>,
    dirty: bool,
    /// Roots added in this session that still need their first scan.
    scan_after_add: bool,
    approved_url: Option<String>,
    images: ImageFetcher,
    /// An online playlist to play as soon as its tracks arrive.
    autoplay: Option<String>,
}

impl Worker {
    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn run(&mut self, receiver: mpsc::Receiver<Job>) {
        self.cursor = self.backend.event_cursor();
        self.refresh_lyric_offset();
        self.store().started = true;
        while !self.stop.load(Ordering::Acquire) {
            let playing = matches!(
                self.store().snapshot.playback.status,
                lx_gpui_player::audio::PlaybackStatus::Playing
                    | lx_gpui_player::audio::PlaybackStatus::Buffering
                    | lx_gpui_player::audio::PlaybackStatus::Loading
            );
            let wait = if playing
                || !self.providers.is_empty()
                || !self.pictures.is_empty()
                || self.import.is_some()
            {
                Duration::from_millis(50)
            } else {
                Duration::from_millis(200)
            };
            match receiver.recv_timeout(wait) {
                Ok(job) => {
                    self.handle(job);
                    while let Ok(job) = receiver.try_recv() {
                        self.handle(job);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            self.poll_events();
            self.poll_snapshot();
            self.poll_integrations();
            self.expire_toasts();
            if std::mem::take(&mut self.dirty) {
                self.invalidate.request_rebuild();
            }
        }
        self.set_open_api(false, 0);
        self.cancel_playlist_import();
    }

    fn envelope(&mut self, command: BackendCommand) -> CommandEnvelope {
        self.request += 1;
        CommandEnvelope {
            request_id: format!("desktop-{}-{}", std::process::id(), self.request),
            origin: "desktop".into(),
            expected_revision: None,
            command,
        }
    }

    fn execute(&mut self, command: BackendCommand) -> Result<String, BackendError> {
        let envelope = self.envelope(command);
        self.backend
            .execute(envelope)
            .map(|receipt| receipt.operation_id)
    }

    fn fail(&mut self, error: impl std::fmt::Display) {
        let text = error.to_string();
        push_toast(&mut self.store(), friendly_error(&text), ToastKind::Error);
        self.dirty = true;
    }

    fn notice(&mut self, text: impl Into<String>) {
        push_toast(&mut self.store(), text.into(), ToastKind::Success);
        self.dirty = true;
    }

    fn command(&mut self, command: BackendCommand) -> bool {
        match self.execute(command) {
            Ok(_) => {
                self.dirty = true;
                true
            }
            Err(error) => {
                self.fail(error);
                false
            }
        }
    }

    fn handle(&mut self, job: Job) {
        match job {
            Job::SetOpenApi { enabled, port } => self.set_open_api(enabled, port),
            Job::ImportPlaylistFile(path) => self.begin_playlist_import(Some(path), None),
            Job::ImportPlaylistUrl { platform, input } => {
                self.begin_playlist_import(None, Some((platform, input)))
            }
            Job::CommitPlaylistImport => self.commit_playlist_import(),
            Job::CancelPlaylistImport => self.cancel_playlist_import(),
            Job::DialogError(error) => self.fail(error),
            Job::Command(command) => {
                self.command(command);
            }
            Job::CommandNotice(command, text) => {
                if self.command(command) {
                    self.notice(text);
                }
            }
            Job::Search {
                platform,
                keyword,
                page,
            } => {
                {
                    let mut store = self.store();
                    let same =
                        store.search.platform == Some(platform) && store.search.keyword == keyword;
                    if !same || page <= 1 {
                        store.search = SearchResults {
                            platform: Some(platform),
                            keyword: keyword.clone(),
                            page,
                            load: Load::Running,
                            ..SearchResults::default()
                        };
                    } else {
                        store.search.load = Load::Running;
                    }
                }
                self.dirty = true;
                let _ = self.execute(BackendCommand::RecordSearch {
                    keyword: keyword.clone(),
                });
                self.refresh_history();
                self.provider(
                    platform,
                    ProviderRequest::Search {
                        keyword: keyword.clone(),
                        page,
                        limit: SEARCH_PAGE,
                    },
                    Purpose::Search {
                        platform,
                        keyword,
                        page,
                    },
                );
            }
            Job::HotSearch(platform) => {
                if !self.store().hot.contains_key(&platform) {
                    self.provider(platform, ProviderRequest::HotSearch, Purpose::Hot(platform));
                }
            }
            Job::Charts(platform) => {
                let state = self.store().charts.get(&platform).cloned();
                if matches!(state, None | Some(Load::Failed(_))) {
                    self.store().charts.insert(platform, Load::Running);
                    self.dirty = true;
                    self.provider(platform, ProviderRequest::Charts, Purpose::Charts(platform));
                } else if state == Some(Load::Done) {
                    // A known list of another platform: open its first chart.
                    let first = {
                        let store = self.store();
                        (store.chart.platform != Some(platform))
                            .then(|| {
                                store
                                    .chart_list
                                    .get(&platform)
                                    .and_then(|list| list.first())
                                    .map(|c| c.id.clone())
                            })
                            .flatten()
                    };
                    if let Some(id) = first {
                        self.handle(Job::ChartTracks {
                            platform,
                            id,
                            page: 1,
                        });
                    }
                }
            }
            Job::ChartTracks { platform, id, page } => {
                {
                    let mut store = self.store();
                    if page <= 1 || store.chart.id != id {
                        store.chart = ChartTracks {
                            platform: Some(platform),
                            id: id.clone(),
                            load: Load::Running,
                            ..ChartTracks::default()
                        };
                    } else {
                        store.chart.load = Load::Running;
                    }
                }
                self.dirty = true;
                self.provider(
                    platform,
                    ProviderRequest::ChartTracks {
                        id: id.clone(),
                        page,
                        limit: CHART_PAGE,
                    },
                    Purpose::ChartTracks { id, page },
                );
            }
            Job::PlaylistTags(platform) => {
                let state = self.store().tags.get(&platform).cloned();
                if matches!(state, None | Some(Load::Failed(_))) {
                    self.store().tags.insert(platform, Load::Running);
                    self.dirty = true;
                    self.provider(
                        platform,
                        ProviderRequest::PlaylistTags,
                        Purpose::Tags(platform),
                    );
                }
            }
            Job::Playlists {
                platform,
                tag,
                page,
            } => {
                {
                    let mut store = self.store();
                    let wall = store.walls.entry((platform, tag.clone())).or_default();
                    if wall.load == Load::Running
                        || (page <= 1 && wall.load == Load::Done)
                        || (page > 1 && page <= wall.page)
                    {
                        return;
                    }
                    wall.load = Load::Running;
                }
                self.dirty = true;
                self.provider(
                    platform,
                    ProviderRequest::Playlists {
                        tag: tag.clone(),
                        sort: String::new(),
                        page,
                        limit: PLAYLIST_PAGE,
                    },
                    Purpose::Playlists {
                        platform,
                        tag,
                        page,
                    },
                );
            }
            Job::PlaylistDetail { platform, id, page } => {
                {
                    let mut store = self.store();
                    if page <= 1 || store.playlist.id != id {
                        if store.playlist.id == id
                            && store.playlist.platform == Some(platform)
                            && matches!(store.playlist.load, Load::Done | Load::Running)
                        {
                            return;
                        }
                        let info = store.playlist_info(platform, &id);
                        store.playlist = PlaylistView {
                            platform: Some(platform),
                            id: id.clone(),
                            info,
                            load: Load::Running,
                            ..PlaylistView::default()
                        };
                    } else if store.playlist.load == Load::Running {
                        return;
                    } else {
                        store.playlist.load = Load::Running;
                    }
                }
                self.dirty = true;
                self.provider(
                    platform,
                    ProviderRequest::PlaylistDetail {
                        id: id.clone(),
                        page,
                        limit: PLAYLIST_TRACKS,
                    },
                    Purpose::PlaylistDetail { id, page },
                );
            }
            Job::SearchPlaylists {
                platform,
                keyword,
                page,
            } => {
                {
                    let mut store = self.store();
                    let same = store.found_lists.platform == Some(platform)
                        && store.found_lists.keyword == keyword;
                    if same
                        && page <= store.found_lists.page
                        && store.found_lists.load != Load::Idle
                    {
                        return;
                    }
                    if !same || page <= 1 {
                        store.found_lists = FoundLists {
                            platform: Some(platform),
                            keyword: keyword.clone(),
                            load: Load::Running,
                            ..FoundLists::default()
                        };
                    } else {
                        store.found_lists.load = Load::Running;
                    }
                }
                self.dirty = true;
                self.provider(
                    platform,
                    ProviderRequest::SearchPlaylists {
                        keyword: keyword.clone(),
                        page,
                        limit: PLAYLIST_PAGE,
                    },
                    Purpose::SearchPlaylists {
                        platform,
                        keyword,
                        page,
                    },
                );
            }
            Job::PlayPlaylist { platform, id } => {
                let ready = {
                    let store = self.store();
                    (store.playlist.id == id
                        && store.playlist.platform == Some(platform)
                        && store.playlist.load == Load::Done)
                        .then(|| store.playlist.tracks.clone())
                };
                match ready {
                    Some(tracks) if !tracks.is_empty() => self.play_first(tracks),
                    _ => {
                        self.autoplay = Some(id.clone());
                        self.handle(Job::PlaylistDetail {
                            platform,
                            id,
                            page: 1,
                        });
                    }
                }
            }
            Job::FetchImage(url) => {
                let key = image_key(&url);
                let mut store = self.store();
                if !store.auto_art || store.art.contains_key(&key) {
                    return;
                }
                store.art.insert(key.clone(), ArtState::Resolving);
                drop(store);
                if !self.images.fetch(url) {
                    self.store().art.remove(&key);
                }
            }
            Job::SaveAsList { name, tracks } => {
                let id = format!(
                    "userlist_{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or_default()
                );
                let count = tracks.len();
                let create = ListAction::Create {
                    position: -1,
                    lists: vec![ListInfo::user(id.clone(), name.clone())],
                };
                if self.command(BackendCommand::ListAction {
                    action: Box::new(create),
                }) {
                    let add = ListAction::MusicAdd {
                        list_id: id,
                        tracks,
                        location: AddLocation::Bottom,
                    };
                    if self.command(BackendCommand::ListAction {
                        action: Box::new(add),
                    }) {
                        self.notice(format!("已收藏为歌单「{name}」，共 {count} 首"));
                    }
                    self.refresh_lists();
                }
            }
            Job::PlayTracks { tracks, track_id } => {
                let action = ListAction::MusicOverwrite {
                    list_id: "temp".into(),
                    tracks: tracks.as_ref().clone(),
                };
                if self.command(BackendCommand::ListAction {
                    action: Box::new(action),
                }) {
                    self.command(BackendCommand::PlayListTrack {
                        list_id: "temp".into(),
                        music_id: track_id,
                        quality: None,
                    });
                }
            }
            Job::AddToList { list_id, tracks } => {
                let count = tracks.len();
                let name = self.list_name(&list_id);
                let action = ListAction::MusicAdd {
                    list_id: list_id.clone(),
                    tracks,
                    location: AddLocation::Bottom,
                };
                if self.command(BackendCommand::ListAction {
                    action: Box::new(action),
                }) {
                    if list_id == "love" {
                        self.notice("已加入我的收藏");
                    } else {
                        self.notice(format!("已添加 {count} 首到「{name}」"));
                    }
                }
            }
            Job::RemoveFromList { list_id, ids } => {
                let action = ListAction::MusicRemove { list_id, ids };
                self.command(BackendCommand::ListAction {
                    action: Box::new(action),
                });
            }
            Job::CreateList(name) => {
                let id = format!(
                    "userlist_{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or_default()
                );
                let action = ListAction::Create {
                    position: -1,
                    lists: vec![ListInfo::user(id, name.clone())],
                };
                if self.command(BackendCommand::ListAction {
                    action: Box::new(action),
                }) {
                    self.notice(format!("已创建歌单「{name}」"));
                }
            }
            Job::RenameList { id, name } => {
                let Some(mut info) = self.store().lists.iter().find(|l| l.id == id).cloned() else {
                    return;
                };
                info.name = name;
                let action = ListAction::Update { lists: vec![info] };
                self.command(BackendCommand::ListAction {
                    action: Box::new(action),
                });
            }
            Job::DeleteList(id) => {
                let action = ListAction::Remove { ids: vec![id] };
                self.command(BackendCommand::ListAction {
                    action: Box::new(action),
                });
            }
            Job::LoadList(id) => self.load_list(&id),
            Job::RefreshLists => self.refresh_lists(),
            Job::RefreshQueue => self.refresh_queue(),
            Job::RefreshSearchHistory => self.refresh_history(),
            Job::RefreshLibraryRoots => self.refresh_roots(),
            Job::ApproveArt(key) => self.approve_art(&key),
            Job::ResolveArt {
                key,
                list_id,
                music_id,
            } => self.resolve_art(key, list_id, music_id),
            Job::AutoApproveAudio(enabled) => {
                self.store().auto_approve_audio = enabled;
                self.approved_url = None;
                self.dirty = true;
            }
            Job::AddLibraryRoot(path) => {
                if self.command(BackendCommand::AddLibraryRoot {
                    path,
                    recursive: true,
                }) {
                    self.scan_after_add = true;
                    self.notice("正在添加文件夹，完成后会自动扫描");
                }
            }
            Job::RescanLibrary => {
                self.refresh_roots();
                let roots: Vec<String> = self
                    .store()
                    .library_roots
                    .iter()
                    .map(|r| r.root_id.clone())
                    .collect();
                if roots.is_empty() {
                    self.notice("还没有添加音乐文件夹");
                }
                for root_id in roots {
                    if !self.command(BackendCommand::ScanLibraryRoot {
                        root_id,
                        options: Default::default(),
                    }) {
                        break;
                    }
                }
            }
            Job::PlayList(id) => {
                self.load_list(&id);
                let first = self
                    .store()
                    .list_data
                    .get(&id)
                    .and_then(|d| d.tracks.first().map(|t| t.id.clone()));
                match first {
                    Some(music_id) => {
                        self.command(BackendCommand::PlayListTrack {
                            list_id: id,
                            music_id,
                            quality: None,
                        });
                    }
                    None => self.notice("歌单是空的"),
                }
            }
            Job::ClearList(id) => {
                let action = ListAction::MusicClear { list_ids: vec![id] };
                self.command(BackendCommand::ListAction {
                    action: Box::new(action),
                });
            }
            Job::RefreshTimer => self.refresh_timer(),
            Job::StartTimer {
                seconds,
                wait_for_track_end,
            } => {
                if self.command(BackendCommand::StartTimeoutStop {
                    seconds,
                    wait_for_track_end: Some(wait_for_track_end),
                }) {
                    self.notice(format!("将在 {} 分钟后停止播放", (seconds / 60.0).round()));
                }
                self.refresh_timer();
            }
            Job::CancelTimer => {
                self.command(BackendCommand::CancelTimeoutStop);
                self.refresh_timer();
            }
            Job::SetLyricOffset(milliseconds) => {
                if self.command(BackendCommand::SetLyricOffset { milliseconds }) {
                    self.store().lyric_offset_ms = milliseconds;
                }
            }
            Job::ApproveAllArt(enabled) => {
                self.store().auto_art = enabled;
                if enabled {
                    let keys: Vec<String> = self
                        .store()
                        .art
                        .iter()
                        .filter(|(_, state)| matches!(state, ArtState::AwaitingApproval { .. }))
                        .map(|(key, _)| key.clone())
                        .collect();
                    for key in keys {
                        self.approve_art(&key);
                    }
                }
                self.dirty = true;
            }
        }
    }

    fn list_name(&self, id: &str) -> String {
        let store = self.store();
        store
            .lists
            .iter()
            .find(|list| list.id == id)
            .map(|list| list_display_name(list))
            .unwrap_or_else(|| id.to_string())
    }

    fn provider(&mut self, platform: Platform, request: ProviderRequest, purpose: Purpose) {
        match self.execute(BackendCommand::ProviderRequest {
            platform,
            request: Box::new(request),
        }) {
            Ok(operation) => {
                self.providers.insert(operation, purpose);
            }
            Err(error) => {
                self.provider_failed(&purpose, error.to_string());
            }
        }
    }

    fn provider_failed(&mut self, purpose: &Purpose, error: String) {
        let message = friendly_error(&error);
        {
            let mut store = self.store();
            match purpose {
                Purpose::Search {
                    platform, keyword, ..
                } => {
                    if store.search.platform == Some(*platform) && &store.search.keyword == keyword
                    {
                        store.search.load = Load::Failed(message.clone());
                    }
                }
                Purpose::Hot(platform) => {
                    store.hot.insert(*platform, Vec::new());
                }
                Purpose::Charts(platform) => {
                    store
                        .charts
                        .insert(*platform, Load::Failed(message.clone()));
                }
                Purpose::ChartTracks { id, .. } => {
                    if &store.chart.id == id {
                        store.chart.load = Load::Failed(message.clone());
                    }
                }
                Purpose::Tags(platform) => {
                    store.tags.insert(*platform, Load::Failed(message.clone()));
                }
                Purpose::Playlists { platform, tag, .. } => {
                    let wall = store.walls.entry((*platform, tag.clone())).or_default();
                    wall.load = Load::Failed(message.clone());
                }
                Purpose::PlaylistDetail { id, .. } => {
                    if &store.playlist.id == id {
                        store.playlist.load = Load::Failed(message.clone());
                    }
                }
                Purpose::SearchPlaylists {
                    platform, keyword, ..
                } => {
                    if store.found_lists.platform == Some(*platform)
                        && &store.found_lists.keyword == keyword
                    {
                        store.found_lists.load = Load::Failed(message.clone());
                    }
                }
            }
        }
        self.dirty = true;
    }

    fn finish_provider(&mut self, operation_id: &str) {
        let Some(purpose) = self.providers.remove(operation_id) else {
            return;
        };
        let result = match self.backend.provider_result(operation_id) {
            Ok(result) => result,
            Err(error) => return self.provider_failed(&purpose, error.to_string()),
        };
        if result.state != ProviderOperationState::Completed {
            let error = result
                .error
                .as_ref()
                .map(|error| provider_error_text(error))
                .unwrap_or_else(|| "请求失败".into());
            return self.provider_failed(&purpose, error);
        }
        let Some(response) = result.response.clone() else {
            return self.provider_failed(&purpose, "空响应".into());
        };
        let mut store = self.store();
        match (&purpose, response.as_ref()) {
            (
                Purpose::Search {
                    platform,
                    keyword,
                    page,
                },
                ProviderResponse::Tracks(found),
            ) => {
                if store.search.platform == Some(*platform) && &store.search.keyword == keyword {
                    let mut tracks = if *page <= 1 {
                        Vec::new()
                    } else {
                        store.search.tracks.as_ref().clone()
                    };
                    for track in &found.items {
                        if !tracks.iter().any(|t: &TrackRecord| t.id == track.id) {
                            tracks.push(track.clone());
                        }
                    }
                    store.search.tracks = Arc::new(tracks);
                    store.search.total = found.total;
                    store.search.page = *page;
                    store.search.load = Load::Done;
                }
            }
            (Purpose::Hot(platform), ProviderResponse::Keywords(words)) => {
                store
                    .hot
                    .insert(*platform, words.iter().take(24).cloned().collect());
            }
            (Purpose::Charts(platform), ProviderResponse::Charts(charts)) => {
                store.chart_list.insert(*platform, charts.clone());
                store.charts.insert(*platform, Load::Done);
                // Open the first chart when none of this platform is shown.
                if store.chart.platform != Some(*platform)
                    && let Some(first) = charts.first()
                {
                    let (platform, id) = (*platform, first.id.clone());
                    drop(store);
                    self.dirty = true;
                    self.handle(Job::ChartTracks {
                        platform,
                        id,
                        page: 1,
                    });
                    return;
                }
            }
            (Purpose::ChartTracks { id, page, .. }, ProviderResponse::Tracks(found)) => {
                if &store.chart.id == id {
                    let mut tracks = if *page <= 1 {
                        Vec::new()
                    } else {
                        store.chart.tracks.as_ref().clone()
                    };
                    tracks.extend(found.items.iter().cloned());
                    store.chart.tracks = Arc::new(tracks);
                    store.chart.total = found.total;
                    store.chart.load = Load::Done;
                }
            }
            (Purpose::ChartTracks { id, page, .. }, ProviderResponse::Collection(collection)) => {
                if &store.chart.id == id {
                    let mut tracks = if *page <= 1 {
                        Vec::new()
                    } else {
                        store.chart.tracks.as_ref().clone()
                    };
                    tracks.extend(collection.tracks.items.iter().cloned());
                    store.chart.tracks = Arc::new(tracks);
                    store.chart.total = collection.tracks.total;
                    store.chart.load = Load::Done;
                }
            }
            (Purpose::Tags(platform), ProviderResponse::PlaylistTags(tags)) => {
                store.tag_list.insert(*platform, tags.clone());
                store.tags.insert(*platform, Load::Done);
            }
            (
                Purpose::Playlists {
                    platform,
                    tag,
                    page,
                },
                ProviderResponse::Playlists(found),
            ) => {
                let wall = store.walls.entry((*platform, tag.clone())).or_default();
                let mut items = if *page <= 1 {
                    Vec::new()
                } else {
                    wall.items.as_ref().clone()
                };
                for list in &found.items {
                    if !items.iter().any(|l: &Playlist| l.id == list.id) {
                        items.push(list.clone());
                    }
                }
                wall.items = Arc::new(items);
                wall.total = found.total;
                wall.page = *page;
                wall.load = Load::Done;
            }
            (Purpose::PlaylistDetail { id, page }, ProviderResponse::Collection(collection)) => {
                if &store.playlist.id == id {
                    let mut tracks = if *page <= 1 {
                        Vec::new()
                    } else {
                        store.playlist.tracks.as_ref().clone()
                    };
                    for track in &collection.tracks.items {
                        if !tracks.iter().any(|t: &TrackRecord| t.id == track.id) {
                            tracks.push(track.clone());
                        }
                    }
                    store.playlist.tracks = Arc::new(tracks);
                    store.playlist.total = collection.tracks.total;
                    store.playlist.page = *page;
                    // The detail fills in what a card did not know.
                    let mut info = collection.info.clone();
                    if let Some(known) = &store.playlist.info {
                        if info.name.trim().is_empty() {
                            info.name = known.name.clone();
                        }
                        if info.image_url.is_none() {
                            info.image_url = known.image_url.clone();
                        }
                        if info.author.is_none() {
                            info.author = known.author.clone();
                        }
                        if info.play_count.is_none() {
                            info.play_count = known.play_count;
                        }
                        if info.description.is_none() {
                            info.description = known.description.clone();
                        }
                    }
                    store.playlist.info = Some(info);
                    store.playlist.load = Load::Done;
                }
            }
            (Purpose::PlaylistDetail { id, page }, ProviderResponse::Tracks(found)) => {
                if &store.playlist.id == id {
                    let mut tracks = if *page <= 1 {
                        Vec::new()
                    } else {
                        store.playlist.tracks.as_ref().clone()
                    };
                    tracks.extend(found.items.iter().cloned());
                    store.playlist.tracks = Arc::new(tracks);
                    store.playlist.total = found.total;
                    store.playlist.page = *page;
                    store.playlist.load = Load::Done;
                }
            }
            (
                Purpose::SearchPlaylists {
                    platform,
                    keyword,
                    page,
                },
                ProviderResponse::Playlists(found),
            ) => {
                if store.found_lists.platform == Some(*platform)
                    && &store.found_lists.keyword == keyword
                {
                    let mut items = if *page <= 1 {
                        Vec::new()
                    } else {
                        store.found_lists.items.as_ref().clone()
                    };
                    for list in &found.items {
                        if !items.iter().any(|l: &Playlist| l.id == list.id) {
                            items.push(list.clone());
                        }
                    }
                    store.found_lists.items = Arc::new(items);
                    store.found_lists.total = found.total;
                    store.found_lists.page = *page;
                    store.found_lists.load = Load::Done;
                }
            }
            _ => {
                drop(store);
                return self.provider_failed(&purpose, "返回的数据类型与请求不符".into());
            }
        }
        drop(store);
        self.dirty = true;
        if let Purpose::PlaylistDetail { id, page: 1 } = &purpose
            && self.autoplay.as_deref() == Some(id.as_str())
        {
            self.autoplay = None;
            let tracks = self.store().playlist.tracks.clone();
            if !tracks.is_empty() {
                self.play_first(tracks);
            }
        }
    }

    /// Plays `tracks` through the temporary list from the first one.
    fn play_first(&mut self, tracks: Arc<Vec<TrackRecord>>) {
        if let Some(first) = tracks.first().map(|t| t.id.clone()) {
            self.handle(Job::PlayTracks {
                tracks,
                track_id: first,
            });
        }
    }

    fn query(&mut self, query: BackendQuery) -> Option<BackendQueryResult> {
        match self.backend.query(query) {
            Ok(result) => Some(result),
            Err(error) => {
                eprintln!("lx-native: query failed: {error}");
                None
            }
        }
    }

    fn refresh_lists(&mut self) {
        if let Some(BackendQueryResult::ListInfos(lists)) = self.query(BackendQuery::ListInfos) {
            let mut totals = HashMap::new();
            for list in &lists {
                if let Some(BackendQueryResult::ListTracks(page)) =
                    self.query(BackendQuery::ListTracks {
                        list_id: list.id.clone(),
                        offset: 0,
                        limit: 1,
                    })
                {
                    totals.insert(list.id.clone(), page.total);
                }
            }
            let mut store = self.store();
            store.lists = lists;
            store.list_totals = totals;
            store.lists_revision += 1;
            drop(store);
            self.dirty = true;
        }
        // Love membership drives the heart buttons everywhere.
        self.load_list("love");
        let open: Vec<String> = self
            .store()
            .list_data
            .keys()
            .filter(|id| id.as_str() != "love")
            .cloned()
            .collect();
        for id in open {
            self.load_list(&id);
        }
    }

    fn load_list(&mut self, id: &str) {
        {
            let mut store = self.store();
            let entry = store.list_data.entry(id.to_string()).or_default();
            if entry.load != Load::Done {
                entry.load = Load::Running;
            }
        }
        let mut tracks = Vec::new();
        let mut revision = 0;
        let mut total;
        let mut offset = 0;
        loop {
            match self.query(BackendQuery::ListTracks {
                list_id: id.to_string(),
                offset,
                limit: LIST_CHUNK,
            }) {
                Some(BackendQueryResult::ListTracks(page)) => {
                    if offset > 0 && page.revision != revision {
                        // The list changed while paging; start over once.
                        tracks.clear();
                        offset = 0;
                        revision = page.revision;
                        continue;
                    }
                    revision = page.revision;
                    total = page.total;
                    let count = page.items.len();
                    tracks.extend(page.items);
                    offset += count;
                    if count == 0 || offset >= total || offset >= LIST_LIMIT {
                        break;
                    }
                }
                _ => {
                    if let Some(entry) = self.store().list_data.get_mut(id) {
                        entry.load = Load::Failed("无法读取列表".into());
                    }
                    self.dirty = true;
                    return;
                }
            }
        }
        let mut store = self.store();
        if id == "love" {
            store.love = Arc::new(tracks.iter().map(|t| (love_key(t), t.id.clone())).collect());
        }
        store.list_data.insert(
            id.to_string(),
            ListData {
                load: Load::Done,
                tracks: Arc::new(tracks),
            },
        );
        drop(store);
        self.dirty = true;
    }

    fn refresh_queue(&mut self) {
        if let Some(BackendQueryResult::Queue(queue)) = self.query(BackendQuery::Queue {
            offset: 0,
            limit: 1000,
        }) {
            let origin = queue.origin_list.clone();
            let current = queue.current.clone();
            self.store().queue = Some(Arc::new(*queue));
            self.dirty = true;
            if !origin.is_empty()
                && origin != "catalog"
                && !self.store().list_data.contains_key(&origin)
            {
                self.load_list(&origin);
            }
            if let Some(entry) = current {
                self.resolve_art(entry.track_id.clone(), entry.list_id, entry.track_id);
            }
        }
    }

    fn refresh_timer(&mut self) {
        if let Some(BackendQueryResult::TimeoutStop(timer)) = self.query(BackendQuery::TimeoutStop)
        {
            self.store().timer = Some((timer, Instant::now()));
            self.dirty = true;
        }
    }

    fn refresh_lyric_offset(&mut self) {
        if let Some(BackendQueryResult::PlaybackCheckpoint(Some(checkpoint))) =
            self.query(BackendQuery::PlaybackCheckpoint)
        {
            self.store().lyric_offset_ms = checkpoint.lyric_offset_ms;
        }
    }

    fn refresh_history(&mut self) {
        if let Some(BackendQueryResult::SearchHistory(history)) =
            self.query(BackendQuery::SearchHistory)
        {
            self.store().history = history;
            self.dirty = true;
        }
    }

    fn refresh_roots(&mut self) {
        if let Some(BackendQueryResult::LibraryRoots(page)) =
            self.query(BackendQuery::LibraryRoots {
                offset: 0,
                limit: 100,
            })
        {
            // Scan folders added in this session once their grant is ready.
            if self.scan_after_add {
                for root in page
                    .items
                    .iter()
                    .filter(|r| r.last_complete_generation == 0)
                {
                    let known = self
                        .store()
                        .library_roots
                        .iter()
                        .any(|r| r.root_id == root.root_id);
                    if !known {
                        self.scan_after_add = false;
                        let _ = self.execute(BackendCommand::ScanLibraryRoot {
                            root_id: root.root_id.clone(),
                            options: Default::default(),
                        });
                        self.notice("正在扫描文件夹…");
                    }
                }
            }
            self.store().library_roots = page.items;
            self.dirty = true;
        }
    }

    fn resolve_art(&mut self, key: String, list_id: String, music_id: String) {
        if self.store().art.contains_key(&key) {
            return;
        }
        if self.pictures.len() >= 6 {
            // Keep the picture service queue short; retry when one finishes.
            if !self.pending_art.iter().any(|(k, _, _)| k == &key) {
                self.pending_art.push_back((key, list_id, music_id));
                while self.pending_art.len() > 48 {
                    self.pending_art.pop_front();
                }
            }
            return;
        }
        self.store().art.insert(key.clone(), ArtState::Resolving);
        match self.execute(BackendCommand::ResolvePicture { list_id, music_id }) {
            Ok(operation) => {
                self.pictures.insert(operation, key);
            }
            Err(_) => {
                self.store().art.insert(key, ArtState::Missing);
            }
        }
        self.dirty = true;
    }

    fn next_art(&mut self) {
        while self.pictures.len() < 6 {
            let Some((key, list, music)) = self.pending_art.pop_front() else {
                break;
            };
            self.resolve_art(key, list, music);
        }
    }

    fn approve_art(&mut self, key: &str) {
        let operation = match self.store().art.get(key) {
            Some(ArtState::AwaitingApproval { operation_id }) => operation_id.clone(),
            _ => return,
        };
        let url = match self.query(BackendQuery::PictureOperations {
            offset: 0,
            limit: 100,
        }) {
            Some(BackendQueryResult::PictureOperations(operations)) => operations
                .into_iter()
                .find(|op| op.operation_id == operation)
                .and_then(|op| op.info)
                .and_then(|info| info.url)
                .map(|url| url.as_str().to_string()),
            _ => None,
        };
        let Some(url) = url else {
            self.store().art.insert(key.to_string(), ArtState::Missing);
            self.dirty = true;
            return;
        };
        match self.execute(BackendCommand::ApprovePictureUrl {
            operation_id: operation.clone(),
            url,
        }) {
            Ok(_) => {
                self.pictures.insert(operation, key.to_string());
                self.store()
                    .art
                    .insert(key.to_string(), ArtState::Resolving);
            }
            Err(_) => {
                self.store().art.insert(key.to_string(), ArtState::Missing);
            }
        }
        self.dirty = true;
    }

    fn picture_changed(&mut self, operation_id: &str, state: PictureState, sha: Option<String>) {
        let Some(key) = self.pictures.get(operation_id).cloned() else {
            return;
        };
        match state {
            PictureState::Resolving | PictureState::Fetching => {}
            PictureState::AwaitingApproval => {
                self.pictures.remove(operation_id);
                let auto = self.store().auto_art;
                self.store().art.insert(
                    key.clone(),
                    ArtState::AwaitingApproval {
                        operation_id: operation_id.to_string(),
                    },
                );
                if auto {
                    self.approve_art(&key);
                }
                self.next_art();
            }
            PictureState::Ready => {
                self.pictures.remove(operation_id);
                let image = sha
                    .and_then(|sha| self.picture_bytes(&sha))
                    .and_then(|bytes| {
                        art::decode(&bytes)
                            .map_err(|error| eprintln!("lx-native: cover decode failed: {error}"))
                            .ok()
                    });
                let state = match image {
                    Some(image) => ArtState::Ready(Arc::new(image)),
                    None => ArtState::Missing,
                };
                self.store().art.insert(key, state);
                self.next_art();
            }
            PictureState::Failed | PictureState::Cancelled => {
                self.pictures.remove(operation_id);
                self.store().art.insert(key, ArtState::Missing);
                self.next_art();
            }
        }
        self.dirty = true;
    }

    fn picture_bytes(&mut self, sha: &str) -> Option<Vec<u8>> {
        let mut bytes = Vec::new();
        loop {
            match self.query(BackendQuery::PictureCachedBytes {
                sha256: sha.to_string(),
                offset: bytes.len() as u64,
                limit: 65_536,
            }) {
                Some(BackendQueryResult::PictureCachedBytes(page)) => {
                    if page.bytes.is_empty() {
                        break;
                    }
                    bytes.extend_from_slice(&page.bytes);
                    if bytes.len() as u64 >= page.total || bytes.len() > 8 * 1024 * 1024 {
                        break;
                    }
                }
                _ => return None,
            }
        }
        (!bytes.is_empty()).then_some(bytes)
    }

    fn poll_events(&mut self) {
        let events = match self.backend.events_after(self.cursor) {
            Ok(events) => events,
            Err(_) => {
                // The journal moved past our cursor: refresh everything once.
                self.cursor = self.backend.event_cursor();
                self.refresh_lists();
                self.refresh_queue();
                return;
            }
        };
        let mut lists = false;
        let mut queue = false;
        let mut roots = false;
        let mut finished = Vec::new();
        for event in events {
            self.cursor = self.cursor.max(event.event_id);
            match event.payload {
                BackendEvent::ListsChanged { .. } => lists = true,
                BackendEvent::QueueChanged { .. }
                | BackendEvent::PlaybackSelectionChanged { .. } => queue = true,
                BackendEvent::ProviderFinished { operation_id, .. } => finished.push(operation_id),
                BackendEvent::PictureChanged {
                    operation_id,
                    state,
                    sha256,
                } => self.picture_changed(&operation_id, state, sha256),
                BackendEvent::LibraryRootChanged { .. }
                | BackendEvent::LibraryRootLocationChanged { .. } => roots = true,
                BackendEvent::LibraryScanFinished { state } => {
                    roots = true;
                    self.notice(format!(
                        "文件夹扫描完成：发现 {} 个音频文件，入库 {} 首",
                        state.discovered_files, state.committed_files
                    ));
                }
                BackendEvent::TimeoutStopChanged { state } => {
                    self.store().timer = Some((state, Instant::now()));
                    self.dirty = true;
                }
                BackendEvent::TrackSkipped { reason, .. } => {
                    push_toast(
                        &mut self.store(),
                        format!("已跳过无法播放的歌曲：{}", friendly_error(&reason)),
                        ToastKind::Info,
                    );
                    self.dirty = true;
                }
                BackendEvent::OperationFinished {
                    success: false,
                    error: Some(error),
                    operation_id,
                } => {
                    self.finish_import_failure(&operation_id);
                    if !self.providers.contains_key(&operation_id)
                        && !self.pictures.contains_key(&operation_id)
                    {
                        self.fail(error.message);
                    }
                }
                _ => {}
            }
        }
        for operation in finished {
            self.finish_provider(&operation);
        }
        // Provider results may also complete without an event reaching us
        // (journal overflow); poll stragglers.
        let stale: Vec<String> = self.providers.keys().cloned().collect();
        for operation in stale {
            if let Ok(result) = self.backend.provider_result(&operation)
                && result.state != ProviderOperationState::Running
            {
                self.finish_provider(&operation);
            }
        }
        if lists {
            self.refresh_lists();
        }
        if queue || lists {
            self.refresh_queue();
        }
        if roots {
            self.refresh_roots();
        }
    }

    fn poll_snapshot(&mut self) {
        let snapshot = self.backend.snapshot();
        let fine = self.tick.load(Ordering::Relaxed) == TickRate::Fine as u8;
        let signature = signature(&snapshot, fine);
        if self.signature.as_ref() != Some(&signature) {
            let track_changed = self.signature.as_ref().is_none_or(|old| {
                old.track != signature.track || old.revision != signature.revision
            });
            self.signature = Some(signature);
            let pending = snapshot.pending_playback_url.clone();
            self.store().snapshot = snapshot;
            self.dirty = true;
            if track_changed {
                self.refresh_queue();
            }
            // "Don't ask again" approves each new address exactly once.
            if let Some(url) = pending
                && self.store().auto_approve_audio
                && self.approved_url.as_deref() != Some(url.as_str())
            {
                self.approved_url = Some(url);
                let _ = self.execute(BackendCommand::ApprovePlaybackUrl);
            }
        }
    }

    fn expire_toasts(&mut self) {
        let mut store = self.store();
        let before = store.toasts.len();
        store
            .toasts
            .retain(|toast| toast.at.elapsed() < Duration::from_millis(4200));
        let changed = store.toasts.len() != before;
        drop(store);
        if changed {
            self.dirty = true;
        }
    }
}

fn signature(snapshot: &BackendSnapshot, fine: bool) -> Signature {
    let playback = &snapshot.playback;
    let bucket = if fine { 20.0 } else { 4.0 };
    Signature {
        revision: snapshot.revision,
        status: playback.status,
        generation: playback.generation,
        track: playback.track_id.clone(),
        position_bucket: (playback.position_seconds * bucket) as i64,
        duration: playback.duration_seconds as i64,
        volume: (playback.volume * 1000.0) as u32,
        busy: snapshot.busy,
        error: snapshot.error.clone(),
        notice: snapshot.notice.clone(),
        pending: snapshot.pending_playback_url.clone(),
        lyrics: snapshot
            .lyrics
            .as_ref()
            .map(|lyrics| (lyrics.track_id.clone(), lyrics.generation)),
        frame: snapshot.lyric_frame.as_ref().map(|frame| {
            (
                frame.line_index,
                frame.word_index,
                if fine {
                    (frame.word_progress * 40.0) as i64
                } else {
                    0
                },
            )
        }),
        lyrics_error: snapshot.lyrics_error.clone(),
        sources: snapshot
            .sources
            .iter()
            .map(|s| s.enabled as usize * 2 + s.approved as usize + s.status.len() * 4)
            .sum::<usize>()
            + snapshot.sources.len() * 1000,
        tracks_ptr: Arc::as_ptr(&snapshot.tracks) as usize,
        library: (
            snapshot.library.roots,
            snapshot.library.authorized_assets,
            snapshot.library.running_operations,
            snapshot.library.revision,
        ),
        equalizer: snapshot
            .equalizer
            .desired
            .gains_db
            .map(|gain| (gain * 10.0) as i64),
        processing: (
            (snapshot.playback_processing.desired.playback_rate * 100.0) as i64,
            snapshot.playback_processing.desired.preserves_pitch,
            (snapshot.playback_processing.desired.pitch_ratio * 1000.0) as i64,
        ),
        output: (
            snapshot.audio_output.state.switch_sequence,
            snapshot.audio_output.devices.len(),
            snapshot.audio_output.preferences.muted,
            snapshot.audio_output.preferences.pause_on_device_change,
        ),
    }
}

/// Identity used for "is this track in my favorites".
pub fn love_key(track: &TrackRecord) -> String {
    format!("{}|{}", track.platform.source, track.platform.song_id)
}

pub fn list_display_name(list: &ListInfo) -> String {
    match list.id.as_str() {
        "default" => "试听列表".into(),
        "love" => "我的收藏".into(),
        "temp" => "临时播放".into(),
        _ if list.name.starts_with("list__name_") => list.id.clone(),
        _ => list.name.clone(),
    }
}

fn provider_error_text(error: &lx_gpui_player::platforms::ProviderError) -> String {
    use lx_gpui_player::platforms::ProviderError as E;
    match error {
        E::Unsupported { .. } => "该平台暂不支持此功能".into(),
        E::Cancelled => "已取消".into(),
        E::DeadlineExceeded => "请求超时，请稍后重试".into(),
        E::InvalidRequest { message } => format!("请求无效：{message}"),
        E::Network { message } => format!("网络错误：{message}"),
        E::Http { status, .. } => format!("服务器返回 {status}"),
        E::ResponseTooLarge { .. } => "响应过大".into(),
        E::Parse { .. } => "无法解析平台返回的数据".into(),
        E::Upstream { message } => format!("平台错误：{message}"),
    }
}

/// Shortens raw backend errors into one readable line.
pub fn friendly_error(text: &str) -> String {
    let line = text.lines().next().unwrap_or(text).trim();
    let lower = line.to_ascii_lowercase();
    if lower.contains("no enabled source") || lower.contains("source is not enabled") {
        return "没有可用的音源：请先在「音源」页面导入并启用一个音源".into();
    }
    if lower.contains("timed out") || lower.contains("timeout") {
        return "操作超时，请稍后重试".into();
    }
    if lower.contains("queue is full") {
        return "操作太频繁，请稍后再试".into();
    }
    let mut out: String = line.chars().take(120).collect();
    if line.chars().count() > 120 {
        out.push('…');
    }
    out
}
