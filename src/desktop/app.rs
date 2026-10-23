//! Application state, messages and the update loop.
//!
//! The window has three columns: navigation on the left, the page in the
//! middle and the player panel — cover, transport, lyrics and queue — on the
//! right. The full-screen stage replaces all three while it is open. Views
//! live in `views/`; this module owns navigation, pointer interaction (press,
//! click, drag, context menus), the tray and window commands, and turns user
//! intent into bridge jobs.

use crate::{
    bridge::{Bridge, Job, TickRate, ToastKind, love_key},
    rows::{Row, RowSource},
    views,
};
use lx_gpui_player::{
    audio::PlaybackStatus,
    backend::{BackendCommand, QueueItemRef},
    domain::{
        audio_output::OutputSelection,
        equalizer::{EQUALIZER_MAX_GAIN_DB, EQUALIZER_MIN_GAIN_DB, EqualizerPreset},
        music::TrackRecord,
    },
    platforms::Platform,
    playback::queue::PlayMode,
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use zsui::{
    AppCx, Command, Dp, ViewNode, WidgetId, ZsCanvasPointerEvent, ZsCanvasPointerPhase,
    ZsCanvasWheelEvent, ZsMediaCommand, ZsPointerButton, ZsWindowCommand, ZsWindowPlacement,
    ZsuiThemeMode,
};

/// Audio file extensions the library imports.
const AUDIO_EXTENSIONS: [&str; 8] = ["mp3", "flac", "wav", "ogg", "oga", "m4a", "mp4", "aac"];

pub const PLATFORMS: [Platform; 5] = [
    Platform::Kw,
    Platform::Kg,
    Platform::Tx,
    Platform::Wy,
    Platform::Mg,
];

pub const PLATFORM_NAMES: [&str; 5] = ["小我", "小狗", "小Q", "小云", "小咪"];

pub fn platform_name(platform: Platform) -> &'static str {
    match platform {
        Platform::Kw => "小我",
        Platform::Kg => "小狗",
        Platform::Tx => "小Q",
        Platform::Wy => "小云",
        Platform::Mg => "小咪",
    }
}

pub fn platform_index(platform: Platform) -> usize {
    PLATFORMS.iter().position(|p| *p == platform).unwrap_or(0)
}

pub fn source_tag(source: &str) -> &'static str {
    match source {
        "kw" => "小我",
        "kg" => "小狗",
        "tx" => "小Q",
        "wy" => "小云",
        "mg" => "小咪",
        "local" => "本地",
        _ => "",
    }
}

/// Canvas surfaces. Each is one interactive canvas widget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Surface {
    NavHead,
    Nav,
    NavFoot,
    Toolbar,
    Discover,
    /// Card walls: albums, artists, playlists.
    Wall,
    /// The header of a collection page.
    Hero,
    /// Track tables.
    List,
    /// The chart list of the chart page.
    ChartIndex,
    /// The search landing page and the head of results.
    Search,
    Settings,
    Sources,
    PanelTop,
    Panel,
    Stage,
    Modal,
    Scrim,
}

impl Surface {
    pub const fn widget(self) -> WidgetId {
        WidgetId::new(1000 + self as u64)
    }
}

pub const W_SEARCH: WidgetId = WidgetId::new(2001);
pub const W_FILTER: WidgetId = WidgetId::new(2002);
pub const W_MODAL_INPUT: WidgetId = WidgetId::new(2003);
pub const W_MENU: WidgetId = WidgetId::new(2004);

/// Application commands raised by the tray icon and the window frame.
pub const CMD_CLOSE: &str = "window.close";
pub const CMD_SHOW: &str = "tray.show";
pub const CMD_TOGGLE: &str = "tray.toggle";
pub const CMD_NEXT: &str = "tray.next";
pub const CMD_PREVIOUS: &str = "tray.previous";
pub const CMD_QUIT: &str = "tray.quit";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GroupKind {
    Album,
    Artist,
}

/// What the player panel shows under the transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelTab {
    Lyrics,
    Queue,
}

/// What search results show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchTab {
    Songs,
    Lists,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Page {
    Discover,
    /// Playlists of the browsing platform, in the chosen category.
    Playlists,
    /// An online playlist.
    Playlist(Platform, String),
    /// The charts of the browsing platform and the open one.
    Charts,
    Search,
    /// The local catalog as one collection.
    Songs,
    Albums,
    Artists,
    Group(GroupKind, String),
    List(String),
    Sources,
    Settings,
}

impl Page {
    pub fn scroll_key(&self) -> String {
        match self {
            Page::Discover => "discover".into(),
            Page::Playlists => "playlists".into(),
            Page::Playlist(platform, id) => format!("playlist:{platform:?}:{id}"),
            Page::Charts => "charts".into(),
            Page::Search => "search".into(),
            Page::Songs => "songs".into(),
            Page::Albums => "albums".into(),
            Page::Artists => "artists".into(),
            Page::Group(kind, name) => format!("group:{kind:?}:{name}"),
            Page::List(id) => format!("list:{id}"),
            Page::Sources => "sources".into(),
            Page::Settings => "settings".into(),
        }
    }

    /// Where Escape leads from a drill-down page.
    pub fn parent(&self) -> Option<Page> {
        match self {
            Page::Group(GroupKind::Album, _) => Some(Page::Albums),
            Page::Group(GroupKind::Artist, _) => Some(Page::Artists),
            Page::Playlist(..) => Some(Page::Playlists),
            _ => None,
        }
    }

    /// Pages shown as a collection: header, actions and a track table.
    pub fn is_collection(&self) -> bool {
        matches!(
            self,
            Page::Songs | Page::Group(..) | Page::List(_) | Page::Playlist(..) | Page::Charts
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DragKind {
    Seek,
    Volume,
    Band(usize),
    Rate,
    Pitch,
}

#[derive(Clone, Copy, Debug)]
pub struct Drag {
    pub kind: DragKind,
    pub value: f64,
    pub sent: Instant,
}

#[derive(Clone, Debug)]
struct Press {
    surface: Surface,
    region: Option<u64>,
}

#[derive(Clone, Debug)]
pub enum MenuKind {
    Track {
        row: Row,
        context: RowSource,
    },
    List(String),
    /// The navigation's "+" button: new playlist and imports.
    Create,
    /// The "…" button of a collection page.
    Collection(Page),
    /// The panel's "…" button: the playing track.
    NowPlaying,
    /// Every playlist category of the browsing platform.
    Tags,
    /// Sleep timer presets.
    Timer,
}

#[derive(Clone, Debug)]
pub struct Menu {
    pub kind: MenuKind,
    pub target: Surface,
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Modal {
    ImportPlaylist { platform: Platform, input: String },
    ApiPort { input: String },
    NewList { name: String },
    RenameList { id: String, name: String },
    DeleteList { id: String, name: String },
    DeleteSource { id: String, name: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    PlayPause,
    Next,
    Previous,
    VolumeUp,
    VolumeDown,
    Search,
    Escape,
    Back,
    Forward,
    SeekForward,
    SeekBackward,
    Stage,
}

#[derive(Clone, Debug)]
pub enum Msg {
    Ptr(Surface, ZsCanvasPointerEvent),
    Hover(Surface, Option<u64>),
    Scroll(String, Dp),
    Query(String),
    Submit(String),
    Filter(String),
    ModalText(String),
    ModalSubmit(String),
    Menu(Command),
    MenuOpen(bool),
    Key(Key),
    WindowState(zsui::ZsWindowState),
    /// A tray menu item, a taskbar button, a tray click or a window close
    /// request.
    App(String),
    /// The system switched between light and dark.
    Theme(ZsuiThemeMode),
    /// Files or folders dropped on the window.
    Dropped(Vec<PathBuf>),
    /// 栖声 was launched again; the new launch's arguments.
    Relaunched(Vec<String>),
    /// A media key, headset button or the system media overlay.
    Media(ZsMediaCommand),
    SearchFocus(bool),
    Wheel(Surface, ZsCanvasWheelEvent),
}

/// Transport actions that both window shortcuts and the system media
/// session deliver for the same media key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    Toggle,
    Play,
    Pause,
    Next,
    Previous,
}

/// Options that take a value, so the value is not mistaken for a file.
pub const VALUE_OPTIONS: [&str; 12] = [
    "--data-dir",
    "--audio",
    "--smoke",
    "--smoke-ms",
    "--wait",
    "--click",
    "--rclick",
    "--move",
    "--type",
    "--key",
    "--scroll",
    "--size",
];

/// The files and folders named on a command line, for "Open with" and for a
/// second launch forwarded to this one. Options and their values are not
/// paths.
pub fn launch_paths(args: &[String]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if VALUE_OPTIONS.contains(&arg.as_str()) {
            args.next();
        } else if !arg.starts_with("--") {
            let path = PathBuf::from(arg);
            if path.exists() {
                paths.push(path);
            }
        }
    }
    paths
}

/// Maps tray and window-frame commands to messages.
pub fn app_command(command: &Command) -> Option<Msg> {
    match command {
        Command::Custom { id, .. } => Some(Msg::App(id.clone())),
        Command::Quit => Some(Msg::App(CMD_QUIT.into())),
        _ => None,
    }
}

/// Slider geometry recorded by canvas builders during layout, so drags map
/// pointer positions to values without re-deriving the layout.
#[derive(Clone, Default)]
pub struct Geo(Arc<Mutex<HashMap<(Surface, u64), [f32; 4]>>>);

impl Geo {
    pub fn set(&self, surface: Surface, region: u64, x: f32, y: f32, w: f32, h: f32) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((surface, region), [x, y, w, h]);
    }

    pub fn get(&self, surface: Surface, region: u64) -> Option<[f32; 4]> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(surface, region))
            .copied()
    }
}

pub struct Ui {
    pub page: Page,
    pub back: Vec<Page>,
    pub forward: Vec<Page>,
    pub query: String,
    /// The platform searches use.
    pub platform: Platform,
    /// The platform the discover, playlist and chart pages browse.
    pub browse: Platform,
    /// The playlist category of the playlist wall; empty for the
    /// recommended playlists.
    pub tag: String,
    pub search_tab: SearchTab,
    pub filter: String,
    pub hover: HashMap<Surface, u64>,
    pub scroll: HashMap<String, f32>,
    pub selected: Option<String>,
    pub panel_tab: PanelTab,
    /// Scroll offset of the panel's queue.
    pub panel_scroll: f32,
    /// The full-screen stage is open.
    pub stage: bool,
    pub drag: Option<Drag>,
    press: Option<Press>,
    last_click: Option<(Surface, u64, Instant)>,
    pub menu: Option<Menu>,
    pub modal: Option<Modal>,
    pub maximized: bool,
    pub timer_track_end: bool,
    /// Closing the window hides it to the notification area.
    pub close_to_tray: bool,
    pub api_enabled: bool,
    pub api_port: u16,
    /// Announce track changes while hidden to the notification area.
    pub notify_tracks: bool,
    /// The window is hidden to the notification area.
    pub hidden: bool,
    /// The track playing when the window was hidden; it is not announced.
    /// Cleared by the view once another track plays.
    pub hidden_track: Mutex<Option<String>>,
    placement: Option<ZsWindowPlacement>,
    last_transport: Option<(Transport, Instant)>,
    auto_art: bool,
}

pub struct State {
    pickers: crate::pickers::Pickers,
    pub bridge: Bridge,
    pub ui: Ui,
    pub geo: Geo,
    pub rows: crate::rows::RowCache,
    pub groups: crate::rows::GroupCache,
    pub data_dir: PathBuf,
    pub instance_inbox: crate::instance::Inbox,
    pub has_tray: bool,
    pub has_track_notifications: bool,
}

/// Window preferences kept next to the library data.
struct Prefs {
    api_enabled: bool,
    api_port: u16,
    close_to_tray: bool,
    notify_tracks: bool,
    auto_art: bool,
    browse: Platform,
    placement: Option<ZsWindowPlacement>,
}

impl Prefs {
    fn path(data_dir: &std::path::Path) -> PathBuf {
        data_dir.join("lx-native-ui.conf")
    }

    fn load(data_dir: &std::path::Path) -> Self {
        let mut prefs = Prefs {
            api_enabled: false,
            api_port: 23330,
            close_to_tray: true,
            notify_tracks: true,
            auto_art: false,
            browse: Platform::Kw,
            placement: None,
        };
        let text = std::fs::read_to_string(Self::path(data_dir)).unwrap_or_default();
        for line in text.lines() {
            match line.split_once('=').map(|(k, v)| (k.trim(), v.trim())) {
                Some(("api_enabled", v)) => prefs.api_enabled = v == "1",
                Some(("api_port", v)) => {
                    prefs.api_port = v.parse::<u16>().ok().filter(|p| *p > 0).unwrap_or(23330)
                }
                Some(("close_to_tray", v)) => prefs.close_to_tray = v != "0",
                Some(("notify_tracks", v)) => prefs.notify_tracks = v != "0",
                Some(("auto_art", v)) => prefs.auto_art = v != "0",
                Some(("browse", v)) => {
                    let legacy = ["酷我", "酷狗", "QQ", "网易云", "咪咕"];
                    if let Some(index) = PLATFORMS
                        .iter()
                        .position(|p| p.as_str() == v)
                        .or_else(|| PLATFORM_NAMES.iter().position(|n| *n == v))
                        .or_else(|| legacy.iter().position(|n| *n == v))
                    {
                        prefs.browse = PLATFORMS[index];
                    }
                }
                Some(("placement", v)) => prefs.placement = ZsWindowPlacement::from_text(v),
                _ => {}
            }
        }
        prefs
    }
}

/// The window placement saved by the last run, if any.
pub fn saved_placement(data_dir: &std::path::Path) -> Option<ZsWindowPlacement> {
    Prefs::load(data_dir).placement
}

impl State {
    pub fn new(bridge: Bridge, data_dir: PathBuf) -> Self {
        let prefs = Prefs::load(&data_dir);
        let capabilities = zsui::HostCapabilities::current_native_window_host();
        let mut state = Self {
            pickers: crate::pickers::Pickers::default(),
            bridge,
            ui: Ui {
                page: Page::Discover,
                back: Vec::new(),
                forward: Vec::new(),
                query: String::new(),
                platform: prefs.browse,
                browse: prefs.browse,
                tag: String::new(),
                search_tab: SearchTab::Songs,
                filter: String::new(),
                hover: HashMap::new(),
                scroll: HashMap::new(),
                selected: None,
                panel_tab: PanelTab::Lyrics,
                panel_scroll: 0.0,
                stage: false,
                drag: None,
                press: None,
                last_click: None,
                menu: None,
                modal: None,
                maximized: false,
                timer_track_end: true,
                close_to_tray: prefs.close_to_tray,
                api_enabled: prefs.api_enabled,
                api_port: prefs.api_port,
                notify_tracks: prefs.notify_tracks,
                hidden: false,
                hidden_track: Mutex::new(None),
                placement: prefs.placement,
                last_transport: None,
                auto_art: prefs.auto_art,
            },
            geo: Geo::default(),
            rows: crate::rows::RowCache::default(),
            groups: crate::rows::GroupCache::default(),
            data_dir,
            instance_inbox: crate::instance::Inbox::default(),
            has_tray: capabilities.tray_or_status_menu.is_fully_supported(),
            has_track_notifications: matches!(capabilities.platform, zsui::PlatformName::Windows),
        };
        if prefs.api_enabled {
            state.send(Job::SetOpenApi {
                enabled: true,
                port: prefs.api_port,
            });
        }
        if prefs.auto_art {
            state.send(Job::ApproveAllArt(true));
        }
        state.enter_page();
        state.send(Job::RefreshQueue);
        state
    }

    fn save_prefs(&self) {
        let mut text = format!(
            "close_to_tray={}\nnotify_tracks={}\nauto_art={}\nbrowse={}\n",
            u8::from(self.ui.close_to_tray),
            u8::from(self.ui.notify_tracks),
            u8::from(self.ui.auto_art),
            self.ui.browse.as_str(),
        );
        text.push_str(&format!(
            "api_enabled={}\napi_port={}\n",
            u8::from(self.ui.api_enabled),
            self.ui.api_port
        ));
        if let Some(placement) = self.ui.placement {
            text.push_str(&format!("placement={}\n", placement.to_text()));
        }
        let _ = std::fs::write(Prefs::path(&self.data_dir), text);
    }

    /// Runs a transport action unless the same one ran a moment ago: a
    /// media key can arrive both as a window shortcut and from the system
    /// media session.
    fn transport(&mut self, action: Transport) {
        let now = Instant::now();
        if let Some((last, at)) = self.ui.last_transport
            && last == action
            && now.duration_since(at) < Duration::from_millis(400)
        {
            return;
        }
        self.ui.last_transport = Some((action, now));
        match action {
            Transport::Toggle => self.toggle_play(),
            Transport::Play => {
                let status = self.bridge.store().snapshot.playback.status;
                if !views::now::is_active(status) {
                    self.toggle_play();
                }
            }
            Transport::Pause => self.cmd(BackendCommand::Pause),
            Transport::Next => self.cmd(BackendCommand::Next),
            Transport::Previous => self.cmd(BackendCommand::Previous),
        }
    }

    /// Imports dropped or forwarded paths: folders join the library, audio
    /// files are imported and LX source scripts are added for approval.
    pub fn import_paths(&self, paths: Vec<PathBuf>) {
        let mut audio = Vec::new();
        let mut folders = 0;
        let mut skipped = 0;
        for path in paths {
            let extension = path
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            if path.is_dir() {
                folders += 1;
                self.send(Job::AddLibraryRoot(path));
            } else if AUDIO_EXTENSIONS.contains(&extension.as_str()) {
                audio.push(path);
            } else if ["lxmc", "json", "gz"].contains(&extension.as_str()) {
                self.send(Job::ImportPlaylistFile(path));
            } else if extension == "js" {
                self.send(Job::CommandNotice(
                    BackendCommand::ImportSource { path },
                    "音源已导入：批准并启用后才会运行".into(),
                ));
            } else {
                skipped += 1;
            }
        }
        if !audio.is_empty() {
            let count = audio.len();
            self.send(Job::CommandNotice(
                BackendCommand::ImportAudio { paths: audio },
                format!("已导入 {count} 个文件"),
            ));
        } else if folders == 0 && skipped > 0 {
            self.bridge.toast(
                "可导入音频、音乐文件夹、歌单文件或音源脚本",
                ToastKind::Info,
            );
        }
    }

    /// Brings the window back from the notification area.
    fn show_window(&mut self, cx: &mut AppCx) {
        self.ui.hidden = false;
        cx.window_command(ZsWindowCommand::Show);
    }

    pub fn hover(&self, surface: Surface) -> Option<u64> {
        self.ui.hover.get(&surface).copied()
    }

    pub fn scroll(&self, key: &str) -> f32 {
        self.ui.scroll.get(key).copied().unwrap_or(0.0)
    }

    fn send(&self, job: Job) {
        self.bridge.send(job);
    }

    fn cmd(&self, command: BackendCommand) {
        self.bridge.send(Job::Command(command));
    }

    pub fn navigate(&mut self, page: Page) {
        self.ui.stage = false;
        if self.ui.page == page {
            // Re-entering a page scrolls it back to the top.
            self.ui.scroll.insert(page.scroll_key(), 0.0);
            return;
        }
        let previous = std::mem::replace(&mut self.ui.page, page);
        self.ui.back.push(previous);
        if self.ui.back.len() > 32 {
            self.ui.back.remove(0);
        }
        self.ui.forward.clear();
        self.enter_page();
    }

    fn enter_page(&mut self) {
        self.ui.filter.clear();
        self.ui.selected = None;
        let page = self.ui.page.clone();
        self.ui.scroll.insert(page.scroll_key(), 0.0);
        let browse = self.ui.browse;
        match page {
            Page::Discover => {
                self.send(Job::Playlists {
                    platform: browse,
                    tag: String::new(),
                    page: 1,
                });
                self.send(Job::Charts(browse));
                self.send(Job::HotSearch(browse));
                self.send(Job::RefreshLists);
                self.send(Job::RefreshLibraryRoots);
            }
            Page::Playlists => {
                self.send(Job::PlaylistTags(browse));
                self.send(Job::Playlists {
                    platform: browse,
                    tag: self.ui.tag.clone(),
                    page: 1,
                });
            }
            Page::Playlist(platform, id) => self.send(Job::PlaylistDetail {
                platform,
                id,
                page: 1,
            }),
            Page::Charts => self.send(Job::Charts(browse)),
            Page::Search => {
                self.send(Job::HotSearch(self.ui.platform));
                self.send(Job::RefreshSearchHistory);
            }
            Page::List(id) => self.send(Job::LoadList(id)),
            Page::Songs | Page::Albums | Page::Artists | Page::Group(..) => {
                self.send(Job::RefreshLibraryRoots)
            }
            Page::Settings => {
                self.send(Job::RefreshTimer);
                self.send(Job::RefreshQueue);
            }
            Page::Sources => {}
        }
    }

    fn go_back(&mut self) {
        if self.ui.stage {
            self.ui.stage = false;
            return;
        }
        if let Some(page) = self.ui.back.pop() {
            let current = std::mem::replace(&mut self.ui.page, page);
            self.ui.forward.push(current);
            self.enter_page();
        }
    }

    fn go_forward(&mut self) {
        if let Some(page) = self.ui.forward.pop() {
            let current = std::mem::replace(&mut self.ui.page, page);
            self.ui.back.push(current);
            self.ui.stage = false;
            self.enter_page();
        }
    }

    fn search(&mut self, keyword: String) {
        let keyword = keyword.trim().to_string();
        self.ui.query = keyword.clone();
        self.navigate(Page::Search);
        if keyword.is_empty() {
            return;
        }
        self.ui.scroll.insert("search".into(), 0.0);
        self.send(Job::Search {
            platform: self.ui.platform,
            keyword: keyword.clone(),
            page: 1,
        });
        if self.ui.search_tab == SearchTab::Lists {
            self.send(Job::SearchPlaylists {
                platform: self.ui.platform,
                keyword,
                page: 1,
            });
        }
    }

    fn snapshot_volume(&self) -> f32 {
        self.bridge.store().snapshot.playback.volume
    }

    fn set_volume(&self, volume: f32) {
        self.cmd(BackendCommand::Volume {
            volume: volume.clamp(0.0, 1.0),
        });
    }

    fn toggle_play(&self) {
        let store = self.bridge.store();
        let has_track = store.snapshot.now_playing.is_some()
            || store
                .queue
                .as_ref()
                .is_some_and(|queue| queue.current.is_some());
        let status = store.snapshot.playback.status;
        drop(store);
        if !has_track {
            self.play_page_from(0);
            return;
        }
        match status {
            PlaybackStatus::Playing | PlaybackStatus::Buffering | PlaybackStatus::Loading => {
                self.cmd(BackendCommand::Pause)
            }
            _ => self.cmd(BackendCommand::Resume),
        }
    }

    /// The page whose rows "play" buttons start: collection pages play
    /// themselves, every other page plays the local catalog.
    fn playable_page(&self) -> Page {
        if self.ui.page.is_collection() || self.ui.page == Page::Search {
            self.ui.page.clone()
        } else {
            Page::Songs
        }
    }

    /// Plays the current page starting at row `index`.
    fn play_page_from(&self, index: usize) {
        let page = self.playable_page();
        if let Page::List(id) = &page
            && self.ui.filter.is_empty()
            && index == 0
        {
            self.send(Job::PlayList(id.clone()));
            return;
        }
        let rows = self.rows.rows(self, &page);
        if let Some(row) = rows.items.get(index) {
            self.play_row(row, &rows.source);
        } else {
            self.bridge.toast("这里还没有可播放的歌曲", ToastKind::Info);
        }
    }

    /// Switches to random order and starts at a random row of the page.
    fn shuffle_page(&self) {
        let rows = self.rows.rows(self, &self.playable_page());
        if rows.items.is_empty() {
            self.bridge.toast("这里还没有可播放的歌曲", ToastKind::Info);
            return;
        }
        self.cmd(BackendCommand::SetPlayMode {
            mode: PlayMode::Random,
        });
        let index = (crate::fmt::phase_ms() as usize).wrapping_mul(2654435761) % rows.items.len();
        self.play_row(&rows.items[index], &rows.source);
        self.send(Job::RefreshQueue);
    }

    pub fn play_row(&self, row: &Row, source: &RowSource) {
        match source {
            RowSource::Catalog => self.cmd(BackendCommand::Play { id: row.id.clone() }),
            RowSource::List(list_id) => self.cmd(BackendCommand::PlayListTrack {
                list_id: list_id.clone(),
                music_id: row.id.clone(),
                quality: None,
            }),
            RowSource::Temp(tracks) => self.send(Job::PlayTracks {
                tracks: tracks.clone(),
                track_id: row.id.clone(),
            }),
        }
    }

    /// Plays every track of an album or artist, in order.
    pub fn play_group(&self, kind: GroupKind, name: &str) {
        let rows = self.rows.rows(self, &Page::Group(kind, name.to_string()));
        if let Some(row) = rows.items.first() {
            self.play_row(row, &rows.source);
        }
    }

    /// Plays a library collection from its first track.
    pub fn play_collection(&self, page: &Page) {
        match page {
            Page::List(id) => self.send(Job::PlayList(id.clone())),
            Page::Group(kind, name) => self.play_group(*kind, name),
            Page::Songs => {
                let rows = self.rows.rows(self, &Page::Songs);
                match rows.items.first() {
                    Some(row) => self.play_row(row, &rows.source),
                    None => self.bridge.toast("曲库还是空的", ToastKind::Info),
                }
            }
            _ => {
                let rows = self.rows.rows(self, page);
                match rows.items.first() {
                    Some(row) => self.play_row(row, &rows.source),
                    None => self.bridge.toast("还没有加载出歌曲", ToastKind::Info),
                }
            }
        }
    }

    pub fn record_for(&self, row: &Row, source: &RowSource) -> Option<TrackRecord> {
        match source {
            RowSource::Temp(tracks) => tracks.get(row.index).cloned(),
            RowSource::List(id) => self
                .bridge
                .store()
                .list_data
                .get(id)
                .and_then(|data| data.tracks.get(row.index).cloned()),
            RowSource::Catalog => {
                let store = self.bridge.store();
                store
                    .snapshot
                    .tracks
                    .get(row.index)
                    .map(crate::rows::catalog_record)
            }
        }
    }

    pub fn toggle_love(&self, row: &Row, source: &RowSource) {
        let love_id = self.bridge.store().love.get(&row.love_key).cloned();
        match love_id {
            Some(id) => self.send(Job::RemoveFromList {
                list_id: "love".into(),
                ids: vec![id],
            }),
            None => {
                if let Some(record) = self.record_for(row, source) {
                    self.send(Job::AddToList {
                        list_id: "love".into(),
                        tracks: vec![record],
                    });
                }
            }
        }
    }

    pub fn current_record(&self) -> Option<TrackRecord> {
        let store = self.bridge.store();
        let current = store.queue.as_ref()?.current.clone()?;
        if current.list_id == "catalog" {
            store
                .snapshot
                .tracks
                .iter()
                .find(|t| t.id == current.track_id)
                .map(crate::rows::catalog_record)
        } else {
            store.list_data.get(&current.list_id).and_then(|data| {
                data.tracks
                    .iter()
                    .find(|t| t.id == current.track_id)
                    .cloned()
            })
        }
    }

    fn toggle_current_love(&self) {
        let Some(record) = self.current_record() else {
            self.bridge
                .toast("无法收藏当前歌曲：找不到它的列表记录", ToastKind::Error);
            return;
        };
        let love_id = self.bridge.store().love.get(&love_key(&record)).cloned();
        match love_id {
            Some(id) => self.send(Job::RemoveFromList {
                list_id: "love".into(),
                ids: vec![id],
            }),
            None => self.send(Job::AddToList {
                list_id: "love".into(),
                tracks: vec![record],
            }),
        }
    }

    fn play_mode(&self) -> PlayMode {
        self.bridge
            .store()
            .queue
            .as_ref()
            .map(|q| q.mode)
            .unwrap_or_default()
    }

    fn set_mode(&self, mode: PlayMode) {
        self.cmd(BackendCommand::SetPlayMode { mode });
        self.send(Job::RefreshQueue);
        self.bridge.toast(
            format!("播放模式：{}", views::now::mode_name(mode)),
            ToastKind::Info,
        );
    }

    /// The mode button: steps through every play mode.
    fn cycle_mode(&self) {
        let next = match self.play_mode() {
            PlayMode::ListLoop => PlayMode::Random,
            PlayMode::Random => PlayMode::SingleLoop,
            PlayMode::SingleLoop => PlayMode::List,
            PlayMode::List => PlayMode::None,
            PlayMode::None => PlayMode::ListLoop,
        };
        self.set_mode(next);
    }

    fn import_audio(&self) {
        self.pick_file(crate::pickers::Kind::Audio);
    }

    fn add_folder(&self) {
        self.pick_file(crate::pickers::Kind::Folder);
    }

    fn import_source(&self) {
        self.pick_file(crate::pickers::Kind::Source);
    }

    fn pick_file(&self, kind: crate::pickers::Kind) {
        if let Err(error) = self.pickers.open(kind, self.bridge.sender()) {
            self.bridge.toast(error.to_string(), ToastKind::Error);
        }
    }

    fn list_name(&self, id: &str) -> String {
        self.bridge
            .store()
            .lists
            .iter()
            .find(|l| l.id == id)
            .map(crate::bridge::list_display_name)
            .unwrap_or_default()
    }

    /// Switches the platform the discover, playlist and chart pages browse.
    fn set_browse(&mut self, platform: Platform) {
        if self.ui.browse == platform {
            return;
        }
        self.ui.browse = platform;
        self.ui.tag.clear();
        self.save_prefs();
        self.enter_page();
    }

    /// Follows the panel's lyrics closely only while they are on screen.
    fn tick(&self) {
        let lyrics_shown = self.ui.stage || self.ui.panel_tab == PanelTab::Lyrics;
        let fine = (!self.ui.hidden && lyrics_shown) || self.ui.drag.is_some();
        self.bridge.set_tick(if fine {
            TickRate::Fine
        } else {
            TickRate::Coarse
        });
    }
}

pub fn update(state: &mut State, msg: Msg, cx: &mut AppCx) {
    if std::env::var_os("LX_DEBUG").is_some() {
        eprintln!("update: {msg:?}");
    }
    let had_modal = state.ui.modal.is_some();
    match msg {
        Msg::Hover(surface, region) => {
            match region {
                Some(region) => state.ui.hover.insert(surface, region),
                None => state.ui.hover.remove(&surface),
            };
        }
        Msg::Scroll(key, offset) => {
            state.ui.scroll.insert(key, offset.0);
        }
        Msg::Query(text) => {
            // Typing in the search field opens the search page.
            if !text.trim().is_empty() && state.ui.page != Page::Search {
                state.navigate(Page::Search);
            }
            state.ui.query = text;
        }
        Msg::Submit(text) => state.search(text),
        Msg::Filter(text) => {
            state.ui.filter = text;
        }
        Msg::ModalText(text) => match &mut state.ui.modal {
            Some(Modal::NewList { name }) | Some(Modal::RenameList { name, .. }) => *name = text,
            Some(Modal::ImportPlaylist { input, .. } | Modal::ApiPort { input }) => *input = text,
            _ => {}
        },
        Msg::ModalSubmit(text) => {
            if let Some(Modal::NewList { name } | Modal::RenameList { name, .. }) =
                &mut state.ui.modal
            {
                *name = text.clone();
            }
            if let Some(Modal::ImportPlaylist { input, .. } | Modal::ApiPort { input }) =
                &mut state.ui.modal
            {
                *input = text;
            }
            confirm_modal(state);
        }
        Msg::Ptr(surface, event) => pointer(state, surface, event, cx),
        Msg::Menu(command) => {
            let menu = state.ui.menu.take();
            if let (Command::Custom { id, .. }, Some(menu)) = (command, menu) {
                menu_command(state, &menu, &id);
            }
        }
        Msg::MenuOpen(open) => {
            if !open {
                state.ui.menu = None;
            }
        }
        Msg::Key(key) => keyboard(state, key, cx),
        Msg::WindowState(window) => {
            state.ui.maximized = window.maximized;
            if window.placement.is_some() && window.placement != state.ui.placement {
                state.ui.placement = window.placement;
                state.save_prefs();
            }
        }
        Msg::App(id) => app_message(state, &id, cx),
        Msg::Theme(mode) => {
            // The next rebuild draws with the new palette.
            crate::theme::follow_system_theme(mode);
        }
        Msg::Dropped(paths) => state.import_paths(paths),
        Msg::Relaunched(args) => {
            state.instance_inbox.handled();
            state.show_window(cx);
            let paths = launch_paths(&args);
            if !paths.is_empty() {
                state.import_paths(paths);
            }
        }
        Msg::Media(command) => state.transport(match command {
            ZsMediaCommand::Play => Transport::Play,
            ZsMediaCommand::Pause | ZsMediaCommand::Stop => Transport::Pause,
            ZsMediaCommand::TogglePlayPause => Transport::Toggle,
            ZsMediaCommand::Next => Transport::Next,
            ZsMediaCommand::Previous => Transport::Previous,
        }),
        Msg::SearchFocus(focused) => {
            // Entering the search field opens search history and trends.
            if focused && state.ui.page != Page::Search {
                state.navigate(Page::Search);
            }
        }
        Msg::Wheel(surface, event) => views::wheel(state, surface, event),
    }
    // A name dialog that just opened takes the keyboard.
    if !had_modal
        && matches!(
            state.ui.modal,
            Some(Modal::NewList { .. } | Modal::RenameList { .. })
        )
    {
        cx.focus(W_MODAL_INPUT);
    }
    state.tick();
}

fn app_message(state: &mut State, id: &str, cx: &mut AppCx) {
    match id {
        #[cfg(feature = "smoke")]
        "smoke.ready" => state.instance_inbox.acknowledge_smoke_ready(),
        #[cfg(feature = "smoke")]
        "smoke.native-close" => cx.window_command(ZsWindowCommand::Close),
        CMD_CLOSE => {
            if state.ui.close_to_tray {
                #[cfg(feature = "smoke")]
                state.instance_inbox.background_closed();
                state.ui.menu = None;
                if !state.has_tray {
                    state.ui.hidden = false;
                    cx.window_command(ZsWindowCommand::Minimize);
                    return;
                }
                state.ui.hidden = true;
                // The playing track is not news; the next one is.
                let playing = views::now::now(state).id;
                *state
                    .ui
                    .hidden_track
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(playing);
                cx.window_command(ZsWindowCommand::Hide);
            } else {
                state.instance_inbox.close();
                cx.quit();
            }
        }
        CMD_SHOW => state.show_window(cx),
        CMD_TOGGLE => state.transport(Transport::Toggle),
        CMD_NEXT => state.transport(Transport::Next),
        CMD_PREVIOUS => state.transport(Transport::Previous),
        CMD_QUIT => {
            state.instance_inbox.close();
            cx.quit();
        }
        crate::instance::RELAUNCH_COMMAND => {
            if let Some(paths) = state.instance_inbox.take() {
                state.instance_inbox.handled();
                state.show_window(cx);
                if !paths.is_empty() {
                    state.import_paths(paths);
                }
            }
        }
        _ => {}
    }
}

fn keyboard(state: &mut State, key: Key, cx: &mut AppCx) {
    match key {
        Key::PlayPause => state.transport(Transport::Toggle),
        Key::Next => state.transport(Transport::Next),
        Key::Previous => state.transport(Transport::Previous),
        Key::VolumeUp => state.set_volume(state.snapshot_volume() + 0.05),
        Key::VolumeDown => state.set_volume(state.snapshot_volume() - 0.05),
        Key::Search => {
            state.ui.stage = false;
            cx.focus(W_SEARCH)
        }
        Key::Escape => {
            if state.ui.modal.is_some() {
                state.ui.modal = None;
            } else if {
                let store = state.bridge.store();
                store.import_busy || store.playlist_preview.is_some()
            } {
                state.send(Job::CancelPlaylistImport);
            } else if state.ui.stage {
                state.ui.stage = false;
            } else if let Some(parent) = state.ui.page.parent() {
                state.navigate(parent);
            }
        }
        Key::Back => state.go_back(),
        Key::Forward => state.go_forward(),
        Key::Stage => state.ui.stage = !state.ui.stage,
        Key::SeekForward | Key::SeekBackward => {
            let position = state.bridge.store().snapshot.playback.position_seconds;
            let delta = if key == Key::SeekForward { 5.0 } else { -5.0 };
            state.cmd(BackendCommand::Seek {
                seconds: (position + delta).max(0.0),
            });
        }
    }
}

fn confirm_modal(state: &mut State) {
    let Some(modal) = state.ui.modal.take() else {
        return;
    };
    match modal {
        Modal::ImportPlaylist { platform, input } => {
            state.send(Job::ImportPlaylistUrl { platform, input })
        }
        Modal::ApiPort { input } => match input.trim().parse::<u16>().ok().filter(|p| *p > 0) {
            Some(port) => {
                state.ui.api_port = port;
                state.save_prefs();
                state.send(Job::SetOpenApi {
                    enabled: state.ui.api_enabled,
                    port,
                });
            }
            None => {
                state.bridge.toast("端口须为 1–65535", ToastKind::Error);
                state.ui.modal = Some(Modal::ApiPort { input });
            }
        },
        Modal::NewList { name } => {
            let name = name.trim().to_string();
            if name.is_empty() {
                state.ui.modal = Some(Modal::NewList { name });
                return;
            }
            state.send(Job::CreateList(name));
        }
        Modal::RenameList { id, name } => {
            let name = name.trim().to_string();
            if !name.is_empty() {
                state.send(Job::RenameList { id, name });
            }
        }
        Modal::DeleteList { id, .. } => {
            let page = Page::List(id.clone());
            state.ui.back.retain(|p| p != &page);
            state.ui.forward.retain(|p| p != &page);
            if state.ui.page == page {
                state.ui.page = Page::Discover;
                state.enter_page();
            }
            state.send(Job::DeleteList(id));
        }
        Modal::DeleteSource { id, .. } => {
            state.send(Job::CommandNotice(
                BackendCommand::DeleteSource { id },
                "音源已删除".into(),
            ));
        }
    }
}

fn pointer(state: &mut State, surface: Surface, event: ZsCanvasPointerEvent, cx: &mut AppCx) {
    let x = event.position.x.0;
    let y = event.position.y.0;
    match event.phase {
        ZsCanvasPointerPhase::Pressed => {
            state.ui.press = Some(Press {
                surface,
                region: event.region,
            });
            if event.button == ZsPointerButton::Primary
                && let Some(region) = event.region
                && let Some(kind) = drag_kind(surface, region)
            {
                let value = drag_value(state, surface, region, kind, x, y);
                state.ui.drag = Some(Drag {
                    kind,
                    value,
                    sent: Instant::now() - Duration::from_secs(1),
                });
                apply_drag(state, false);
            }
        }
        ZsCanvasPointerPhase::Moved => {
            if let Some(drag) = state.ui.drag {
                let pressed = state.ui.press.as_ref().map(|p| (p.surface, p.region));
                if let Some((surface, Some(region))) = pressed {
                    let value = drag_value(state, surface, region, drag.kind, x, y);
                    if let Some(drag) = state.ui.drag.as_mut() {
                        drag.value = value;
                    }
                    apply_drag(state, false);
                }
            }
        }
        ZsCanvasPointerPhase::Released => {
            let press = state.ui.press.take();
            if state.ui.drag.is_some() {
                apply_drag(state, true);
                state.ui.drag = None;
                return;
            }
            let Some(press) = press else { return };
            if !event.inside {
                return;
            }
            if press.surface != surface || press.region != event.region {
                return;
            }
            let Some(region) = event.region else { return };
            match event.button {
                ZsPointerButton::Primary => {
                    let now = Instant::now();
                    let double = state.ui.last_click.as_ref().is_some_and(|(s, r, at)| {
                        *s == surface && *r == region && at.elapsed() < Duration::from_millis(450)
                    });
                    state.ui.last_click = if double {
                        None
                    } else {
                        Some((surface, region, now))
                    };
                    click(state, surface, region, double, x, y, cx);
                }
                ZsPointerButton::Secondary => context_menu(state, surface, region, x, y),
                _ => {}
            }
        }
        ZsCanvasPointerPhase::Cancelled => {
            state.ui.press = None;
            state.ui.drag = None;
        }
    }
}

fn drag_kind(surface: Surface, region: u64) -> Option<DragKind> {
    use crate::paint::rsplit;
    use views::now::{R_SEEK, R_VOLUME};
    let (kind, index) = rsplit(region);
    match (surface, kind) {
        (Surface::Panel | Surface::Stage, R_SEEK) => Some(DragKind::Seek),
        (Surface::Panel | Surface::Stage, R_VOLUME) => Some(DragKind::Volume),
        (Surface::Settings, views::settings::R_BAND) => Some(DragKind::Band(index as usize)),
        (Surface::Settings, views::settings::R_RATE) => Some(DragKind::Rate),
        (Surface::Settings, views::settings::R_PITCH) => Some(DragKind::Pitch),
        _ => None,
    }
}

fn drag_value(state: &State, surface: Surface, region: u64, kind: DragKind, x: f32, y: f32) -> f64 {
    let Some([gx, gy, gw, gh]) = state.geo.get(surface, region) else {
        return 0.0;
    };
    let t = ((x - gx) / gw.max(1.0)).clamp(0.0, 1.0) as f64;
    match kind {
        DragKind::Seek => {
            let duration = state.bridge.store().snapshot.playback.duration_seconds;
            t * duration.max(0.0)
        }
        DragKind::Volume => t,
        DragKind::Band(_) => {
            // Vertical slider: top is the maximum gain.
            let t = (1.0 - (y - gy) / gh.max(1.0)).clamp(0.0, 1.0) as f64;
            EQUALIZER_MIN_GAIN_DB + (EQUALIZER_MAX_GAIN_DB - EQUALIZER_MIN_GAIN_DB) * t
        }
        DragKind::Rate => views::settings::rate_from_t(t),
        DragKind::Pitch => views::settings::pitch_from_t(t),
    }
}

fn apply_drag(state: &mut State, release: bool) {
    let Some(mut drag) = state.ui.drag else {
        return;
    };
    let throttle = drag.sent.elapsed() < Duration::from_millis(60);
    match drag.kind {
        DragKind::Seek => {
            if release {
                state.cmd(BackendCommand::Seek {
                    seconds: drag.value,
                });
            }
        }
        DragKind::Volume => {
            if release || !throttle {
                state.set_volume(drag.value as f32);
                drag.sent = Instant::now();
            }
        }
        DragKind::Band(index) => {
            if release {
                let mut settings = state.bridge.store().snapshot.equalizer.desired;
                settings.gains_db[index] = (drag.value * 2.0).round() / 2.0;
                state.cmd(BackendCommand::SetEqualizer { settings });
            }
        }
        DragKind::Rate => {
            if release {
                state.cmd(BackendCommand::SetPlaybackRate { rate: drag.value });
            }
        }
        DragKind::Pitch => {
            if release {
                state.cmd(BackendCommand::SetPitchRatio { ratio: drag.value });
            }
        }
    }
    state.ui.drag = Some(drag);
}

fn click(
    state: &mut State,
    surface: Surface,
    region: u64,
    double: bool,
    x: f32,
    y: f32,
    cx: &mut AppCx,
) {
    match surface {
        Surface::NavHead | Surface::Nav | Surface::NavFoot => {
            views::nav::click(state, surface, region, x, y, cx)
        }
        Surface::Toolbar => views::toolbar_click(state, region, cx),
        Surface::Discover => views::discover::click(state, region),
        Surface::Wall => views::walls::click(state, region, x, y),
        Surface::Hero => views::collection::click(state, region, x, y),
        Surface::List => views::collection::table_click(state, region, double, x, y),
        Surface::ChartIndex => views::charts::click(state, region),
        Surface::Search => views::search::click(state, region),
        Surface::Settings => views::settings::click(state, region),
        Surface::Sources => views::sources::click(state, region),
        Surface::PanelTop | Surface::Panel => {
            views::panel::click(state, surface, region, double, x, y, cx)
        }
        Surface::Stage => views::stage::click(state, surface, region, double, x, y, cx),
        Surface::Modal => modal_click(state, region),
        Surface::Scrim => state.ui.modal = None,
    }
}

fn modal_click(state: &mut State, region: u64) {
    if views::overlay::approval_click(state, region) {
        return;
    }
    match crate::paint::rsplit(region).0 {
        views::overlay::R_OK => confirm_modal(state),
        views::overlay::R_CANCEL => state.ui.modal = None,
        _ => {}
    }
}

fn context_menu(state: &mut State, surface: Surface, region: u64, x: f32, y: f32) {
    let kind = match surface {
        Surface::List => {
            let (kind, index) = crate::paint::rsplit(region);
            if !views::collection::is_row_region(kind) {
                return;
            }
            let rows = state.rows.rows(state, &state.ui.page);
            let Some(row) = rows.items.get(index as usize).cloned() else {
                return;
            };
            state.ui.selected = Some(row.id.clone());
            MenuKind::Track {
                row,
                context: rows.source.clone(),
            }
        }
        Surface::Nav => match views::nav::list_at(state, region) {
            Some(id) => MenuKind::List(id),
            None => return,
        },
        _ => return,
    };
    state.ui.menu = Some(Menu {
        kind,
        target: surface,
        x,
        y,
    });
}

fn menu_command(state: &mut State, menu: &Menu, id: &str) {
    // Import entries are shared by the navigation and collection menus.
    match id {
        "import_playlist_url" => {
            state.ui.modal = Some(Modal::ImportPlaylist {
                platform: state.ui.browse,
                input: String::new(),
            });
            return;
        }
        "import_playlist_file" => {
            state.pick_file(crate::pickers::Kind::Playlist);
            return;
        }
        _ => {}
    }
    match &menu.kind {
        MenuKind::Track { row, context } => match id {
            "play" => state.play_row(row, context),
            "next" => {
                if let Some(item) = queue_ref(state, row, context) {
                    state.cmd(BackendCommand::Enqueue {
                        items: vec![item],
                        front: true,
                    });
                    state.send(Job::RefreshQueue);
                    state.bridge.toast("已设为下一首播放", ToastKind::Success);
                }
            }
            "love" => state.toggle_love(row, context),
            "remove" => {
                if let RowSource::List(list_id) = context {
                    state.send(Job::RemoveFromList {
                        list_id: list_id.clone(),
                        ids: vec![row.id.clone()],
                    });
                }
            }
            "album" => {
                if !row.album.is_empty() {
                    state.navigate(Page::Group(GroupKind::Album, row.album.clone()));
                }
            }
            "artist" => {
                if !row.artist.is_empty() {
                    state.navigate(Page::Group(GroupKind::Artist, row.artist.clone()));
                }
            }
            "search_artist" => {
                if !row.artist.is_empty() {
                    let artist = row.artist.clone();
                    state.search(artist);
                }
            }
            other => {
                if let Some(list_id) = other.strip_prefix("add:")
                    && let Some(record) = state.record_for(row, context)
                {
                    state.send(Job::AddToList {
                        list_id: list_id.to_string(),
                        tracks: vec![record],
                    });
                }
            }
        },
        MenuKind::Create | MenuKind::Collection(Page::Songs) => match id {
            "new_list" => {
                state.ui.modal = Some(Modal::NewList {
                    name: String::new(),
                })
            }
            "import_files" => state.import_audio(),
            "import_playlist_url" => {
                state.ui.modal = Some(Modal::ImportPlaylist {
                    platform: state.ui.browse,
                    input: String::new(),
                })
            }
            "import_playlist_file" => state.pick_file(crate::pickers::Kind::Playlist),
            "add_folder" => state.add_folder(),
            "rescan" => state.send(Job::RescanLibrary),
            _ => {}
        },
        MenuKind::Collection(Page::Playlist(..)) | MenuKind::Collection(Page::Charts) => {
            if id == "save" {
                save_page_as_list(state);
            }
        }
        MenuKind::Collection(page) => {
            if let Page::List(list_id) = page {
                list_command(state, list_id, id);
            }
        }
        MenuKind::List(list_id) => list_command(state, list_id, id),
        MenuKind::NowPlaying => now_playing_command(state, id),
        MenuKind::Tags => {
            if let Some(tag) = id.strip_prefix("tag:") {
                act_set_tag(state, tag.to_string());
            }
        }
        MenuKind::Timer => timer_command(state, id),
    }
}

fn now_playing_command(state: &mut State, id: &str) {
    match id {
        "love" => state.toggle_current_love(),
        "stage" => state.ui.stage = true,
        "queue" => state.ui.panel_tab = PanelTab::Queue,
        "effects" => state.navigate(Page::Settings),
        "lyrics_earlier" | "lyrics_later" | "lyrics_reset" => {
            let offset = state.bridge.store().lyric_offset_ms;
            let next = match id {
                "lyrics_earlier" => offset - 500,
                "lyrics_later" => offset + 500,
                _ => 0,
            };
            state.send(Job::SetLyricOffset(next));
        }
        other => {
            if let Some(list_id) = other.strip_prefix("add:")
                && let Some(record) = state.current_record()
            {
                state.send(Job::AddToList {
                    list_id: list_id.to_string(),
                    tracks: vec![record],
                });
            } else if other.starts_with("timer:") {
                timer_command(state, other);
            }
        }
    }
}

fn timer_command(state: &mut State, id: &str) {
    match id {
        "timer:cancel" => state.send(Job::CancelTimer),
        other => {
            if let Some(minutes) = other
                .strip_prefix("timer:")
                .and_then(|m| m.parse::<f64>().ok())
            {
                state.send(Job::StartTimer {
                    seconds: minutes * 60.0,
                    wait_for_track_end: state.ui.timer_track_end,
                });
                state.bridge.toast(
                    format!("将在 {minutes:.0} 分钟后停止播放"),
                    ToastKind::Success,
                );
            }
        }
    }
}

/// Saves the loaded tracks of an online playlist or chart as a playlist of
/// the user.
fn save_page_as_list(state: &mut State) {
    let (name, tracks) = {
        let store = state.bridge.store();
        match &state.ui.page {
            Page::Playlist(..) => (
                store
                    .playlist
                    .info
                    .as_ref()
                    .map(|i| i.name.clone())
                    .unwrap_or_else(|| "收藏的歌单".into()),
                store.playlist.tracks.as_ref().clone(),
            ),
            Page::Charts => {
                let platform = store.chart.platform.unwrap_or(state.ui.browse);
                let name = store
                    .chart_list
                    .get(&platform)
                    .and_then(|list| list.iter().find(|c| c.id == store.chart.id))
                    .map(|c| format!("{} · {}", c.name, platform_name(platform)))
                    .unwrap_or_else(|| "收藏的榜单".into());
                (name, store.chart.tracks.as_ref().clone())
            }
            _ => return,
        }
    };
    if tracks.is_empty() {
        state.bridge.toast("还没有加载出歌曲", ToastKind::Info);
        return;
    }
    state.send(Job::SaveAsList { name, tracks });
}

fn list_command(state: &mut State, list_id: &str, id: &str) {
    let name = state.list_name(list_id);
    match id {
        "open" => state.navigate(Page::List(list_id.to_string())),
        "play" => state.send(Job::PlayList(list_id.to_string())),
        "clear" => state.send(Job::ClearList(list_id.to_string())),
        "rename" => {
            state.ui.modal = Some(Modal::RenameList {
                id: list_id.to_string(),
                name,
            })
        }
        "delete" => {
            state.ui.modal = Some(Modal::DeleteList {
                id: list_id.to_string(),
                name,
            })
        }
        _ => {}
    }
}

/// A queue reference for "play next": only list members can be queued.
fn queue_ref(state: &State, row: &Row, context: &RowSource) -> Option<QueueItemRef> {
    match context {
        RowSource::Catalog => Some(QueueItemRef {
            list_id: "catalog".into(),
            track_id: row.id.clone(),
        }),
        RowSource::List(list_id) => Some(QueueItemRef {
            list_id: list_id.clone(),
            track_id: row.id.clone(),
        }),
        RowSource::Temp(_) => {
            if row.source == "local" {
                return Some(QueueItemRef {
                    list_id: "catalog".into(),
                    track_id: row.id.clone(),
                });
            }
            // Online results must be saved first: add to the trial list.
            let record = state.record_for(row, context)?;
            state.send(Job::AddToList {
                list_id: "default".into(),
                tracks: vec![record],
            });
            Some(QueueItemRef {
                list_id: "default".into(),
                track_id: row.id.clone(),
            })
        }
    }
}

// --- Actions used by several views -----------------------------------------

pub fn act_toggle_play(state: &State) {
    state.toggle_play();
}

pub fn act_cycle_mode(state: &State) {
    state.cycle_mode();
}

pub fn act_toggle_current_love(state: &State) {
    state.toggle_current_love();
}

pub fn act_shuffle_page(state: &State) {
    state.shuffle_page();
}

pub fn act_add_folder(state: &State) {
    state.add_folder();
}

pub fn act_import_source(state: &State) {
    state.import_source();
}

pub fn act_command(state: &State, command: BackendCommand) {
    state.cmd(command);
}

pub fn act_job(state: &State, job: Job) {
    state.send(job);
}

pub fn act_go_back(state: &mut State) {
    state.go_back();
}

pub fn act_go_forward(state: &mut State) {
    state.go_forward();
}

pub fn act_search(state: &mut State, keyword: String) {
    state.search(keyword);
}

pub fn act_set_close_to_tray(state: &mut State, on: bool) {
    state.ui.close_to_tray = on;
    state.save_prefs();
}

pub fn act_set_notify_tracks(state: &mut State, on: bool) {
    state.ui.notify_tracks = on;
    state.save_prefs();
}

/// Allows or stops loading online covers; remembered across runs.
pub fn act_set_auto_art(state: &mut State, on: bool) {
    state.ui.auto_art = on;
    state.save_prefs();
    state.send(Job::ApproveAllArt(on));
}

pub fn act_set_platform(state: &mut State, platform: Platform) {
    state.ui.platform = platform;
    state.send(Job::HotSearch(platform));
    let keyword = state.ui.query.trim().to_string();
    if !keyword.is_empty() && state.ui.page == Page::Search {
        state.ui.scroll.insert("search".into(), 0.0);
        state.send(Job::Search {
            platform,
            keyword: keyword.clone(),
            page: 1,
        });
        if state.ui.search_tab == SearchTab::Lists {
            state.send(Job::SearchPlaylists {
                platform,
                keyword,
                page: 1,
            });
        }
    }
}

pub fn act_set_search_tab(state: &mut State, tab: SearchTab) {
    state.ui.search_tab = tab;
    state.ui.scroll.insert("search".into(), 0.0);
    let keyword = state.ui.query.trim().to_string();
    if tab == SearchTab::Lists && !keyword.is_empty() {
        state.send(Job::SearchPlaylists {
            platform: state.ui.platform,
            keyword,
            page: 1,
        });
    }
}

pub fn act_set_browse(state: &mut State, platform: Platform) {
    state.set_browse(platform);
}

/// Opens the playlist wall on category `tag`.
pub fn act_set_tag(state: &mut State, tag: String) {
    state.ui.tag = tag.clone();
    state.ui.scroll.insert(Page::Playlists.scroll_key(), 0.0);
    if state.ui.page != Page::Playlists {
        state.navigate(Page::Playlists);
    } else {
        state.send(Job::Playlists {
            platform: state.ui.browse,
            tag,
            page: 1,
        });
    }
}

/// Opens chart `index` of the browsing platform.
pub fn act_open_chart(state: &mut State, index: usize) {
    let platform = state.ui.browse;
    let id = state
        .bridge
        .store()
        .chart_list
        .get(&platform)
        .and_then(|list| list.get(index))
        .map(|c| c.id.clone());
    if let Some(id) = id {
        state.send(Job::ChartTracks {
            platform,
            id,
            page: 1,
        });
        state.ui.scroll.insert(Page::Charts.scroll_key(), 0.0);
        if state.ui.page != Page::Charts {
            state.navigate(Page::Charts);
        }
    }
}

/// Opens an online playlist.
pub fn act_open_playlist(state: &mut State, platform: Platform, id: String) {
    state.navigate(Page::Playlist(platform, id));
}

pub fn act_navigate(state: &mut State, page: Page) {
    state.navigate(page);
}

pub fn act_open_menu(state: &mut State, kind: MenuKind, target: Surface, x: f32, y: f32) {
    state.ui.menu = Some(Menu { kind, target, x, y });
}

pub fn act_output(state: &State, selection: OutputSelection) {
    state.cmd(BackendCommand::SetAudioOutputDevice { selection });
}

pub fn act_preset(state: &State, preset: EqualizerPreset) {
    state.cmd(BackendCommand::SetEqualizerPreset { preset });
}

pub fn act_set_volume(state: &State, volume: f32) {
    state.set_volume(volume);
}

pub fn act_volume(state: &State) -> f32 {
    state.snapshot_volume()
}

/// Saves the open online playlist or chart as a playlist of the user.
pub fn act_save_page(state: &mut State) {
    save_page_as_list(state);
}

pub fn view(state: &State) -> ViewNode<Msg> {
    views::root(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_paths_skip_options_and_their_values() {
        let dir = std::env::temp_dir();
        let text = dir.display().to_string();
        let args: Vec<String> = [
            "--data-dir",
            &text,
            "--audio",
            "memory",
            "--portable",
            &text,
            "no-such-file.mp3",
        ]
        .map(String::from)
        .to_vec();
        // The data folder after --data-dir is not a file to open; the
        // positional one is, and a missing file is dropped.
        assert_eq!(launch_paths(&args), vec![dir]);
    }
}

pub fn act_set_open_api(state: &mut State, enabled: bool) {
    state.ui.api_enabled = enabled;
    state.save_prefs();
    state.send(Job::SetOpenApi {
        enabled,
        port: state.ui.api_port,
    });
}

#[cfg(test)]
mod preference_tests {
    use super::*;

    #[test]
    fn platform_rename_preserves_legacy_selection_and_api_preferences() {
        let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        for value in ["QQ", "小Q", "tx"] {
            std::fs::write(
                Prefs::path(dir.path()),
                format!("browse={value}\napi_enabled=1\napi_port=24321\n"),
            )
            .unwrap();
            let prefs = Prefs::load(dir.path());
            assert_eq!(prefs.browse, Platform::Tx);
            assert!(prefs.api_enabled);
            assert_eq!(prefs.api_port, 24321);
        }
        std::fs::write(Prefs::path(dir.path()), "browse=wy\napi_port=0\n").unwrap();
        let prefs = Prefs::load(dir.path());
        assert_eq!(prefs.browse, Platform::Wy);
        assert!(!prefs.api_enabled);
        assert_eq!(prefs.api_port, 23330);
    }
}
