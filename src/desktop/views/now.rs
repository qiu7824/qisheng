//! The playing track, copied out of the store once per rebuild, plus the
//! helpers transport surfaces share (mode names and glyphs, the seek and
//! volume region ids).

use crate::{
    app::{DragKind, State},
    art,
    bridge::{ArtState, love_key},
    theme::glyph as g,
};
use lx_gpui_player::{
    audio::PlaybackStatus, backend::LyricSnapshot, lyrics::timeline::LyricFrame,
    playback::queue::PlayMode,
};
use std::{cell::RefCell, collections::HashMap, sync::Arc, time::Instant};

/// Slider regions of the panel and the stage.
pub const R_SEEK: u32 = 1;
pub const R_VOLUME: u32 = 2;

pub fn mode_name(mode: PlayMode) -> &'static str {
    match mode {
        PlayMode::ListLoop => "列表循环",
        PlayMode::Random => "随机播放",
        PlayMode::SingleLoop => "单曲循环",
        PlayMode::List => "顺序播放",
        PlayMode::None => "播完停止",
    }
}

pub fn mode_glyph(mode: PlayMode) -> char {
    match mode {
        PlayMode::ListLoop => g::REPEAT_ALL,
        PlayMode::Random => g::SHUFFLE,
        PlayMode::SingleLoop => g::REPEAT_ONE,
        PlayMode::List => g::ORDER,
        PlayMode::None => g::STOP_AFTER,
    }
}

pub fn is_active(status: PlaybackStatus) -> bool {
    matches!(
        status,
        PlaybackStatus::Playing | PlaybackStatus::Buffering | PlaybackStatus::Loading
    )
}

#[derive(Clone)]
pub enum Cover {
    Art(Arc<art::ArtImage>),
    /// Online artwork waiting for the user to allow its address.
    Pending,
    Missing,
}

impl Cover {
    pub fn art(&self) -> Option<&art::ArtImage> {
        match self {
            Cover::Art(image) => Some(image),
            _ => None,
        }
    }
}

/// Everything about the playing track, copied out of the store.
#[derive(Clone)]
pub struct Now {
    /// The playing track's id; empty without a track.
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub has_track: bool,
    pub status: PlaybackStatus,
    pub position: f64,
    pub duration: f64,
    pub volume: f32,
    pub muted: bool,
    pub mode: PlayMode,
    pub loved: bool,
    pub cover: Cover,
    pub rate: f64,
    pub busy: bool,
    pub lyrics: Option<Arc<LyricSnapshot>>,
    pub frame: Option<Arc<LyricFrame>>,
    pub lyrics_error: Option<String>,
    /// The track was restored from the last session and has not started;
    /// its lyrics load once it plays.
    pub restored: bool,
    /// Display name of the list the queue plays from.
    pub origin: String,
}

impl Now {
    pub fn fraction(&self) -> f32 {
        if self.duration > 0.0 {
            (self.position / self.duration).clamp(0.0, 1.0) as f32
        } else {
            0.0
        }
    }

    pub fn loading(&self) -> bool {
        matches!(
            self.status,
            PlaybackStatus::Loading | PlaybackStatus::Buffering
        ) || self.busy
    }

    pub fn volume_glyph(&self) -> char {
        if self.muted || self.volume <= 0.001 {
            g::MUTE
        } else if self.volume < 0.34 {
            g::VOLUME0
        } else if self.volume < 0.67 {
            g::VOLUME1
        } else {
            g::VOLUME
        }
    }

    /// The album, unless it is a placeholder the importer filled in.
    pub fn album_shown(&self) -> Option<&str> {
        let album = self.album.trim();
        (!album.is_empty() && !matches!(album, "用户导入" | "未知专辑" | "本地音频"))
            .then_some(album)
    }

    /// What a drawn cover for the playing track is named after.
    pub fn album_or_title(&self) -> String {
        self.album_shown().unwrap_or(&self.title).to_string()
    }

    /// What to show where lyrics would be.
    pub fn lyric_placeholder(&self) -> &'static str {
        if !self.has_track {
            "还没有播放"
        } else if self.lyrics.is_some() {
            "纯音乐，请欣赏"
        } else if self.lyrics_error.is_some() {
            "暂无歌词"
        } else if self.restored {
            "播放后显示歌词"
        } else {
            "正在查找歌词…"
        }
    }
}

pub fn now(state: &State) -> Now {
    let store = state.bridge.store();
    let snapshot = &store.snapshot;
    let playback = &snapshot.playback;
    let queue = store.queue.clone();
    let summary = snapshot
        .now_playing
        .clone()
        .or_else(|| queue.as_ref().and_then(|q| q.current_track.clone()));
    let (id, title, artist, album, has_track) = match &summary {
        Some(t) => (
            t.id.clone(),
            t.title.clone(),
            t.artist.clone(),
            t.album.clone(),
            true,
        ),
        None => Default::default(),
    };
    let find = |list_id: &str, track_id: &str| {
        if list_id == "catalog" {
            snapshot
                .tracks
                .iter()
                .find(|t| t.id == track_id)
                .map(crate::rows::catalog_record)
        } else {
            store
                .list_data
                .get(list_id)
                .and_then(|d| d.tracks.iter().find(|t| t.id == track_id).cloned())
        }
    };
    let current = queue.as_ref().and_then(|q| q.current.clone());
    let loved = current.as_ref().is_some_and(|entry| {
        find(&entry.list_id, &entry.track_id)
            .is_some_and(|r| store.love.contains_key(&love_key(&r)))
    });
    let cover = if let Some(image) = store.art.image(&id) {
        Cover::Art(image)
    } else {
        match store.art.get(&id) {
            Some(ArtState::AwaitingApproval { .. }) => Cover::Pending,
            _ => Cover::Missing,
        }
    };
    let position = match state.ui.drag {
        Some(drag) if drag.kind == DragKind::Seek => drag.value,
        _ => playback.position_seconds,
    };
    let volume = match state.ui.drag {
        Some(drag) if drag.kind == DragKind::Volume => drag.value as f32,
        _ => playback.volume,
    };
    let origin = queue
        .as_ref()
        .map(|q| {
            if q.origin_list == "catalog" {
                "本地音乐".to_string()
            } else {
                store
                    .lists
                    .iter()
                    .find(|l| l.id == q.origin_list)
                    .map(crate::bridge::list_display_name)
                    .unwrap_or_else(|| "播放队列".into())
            }
        })
        .unwrap_or_default();
    Now {
        title,
        artist,
        album,
        has_track,
        status: playback.status,
        position,
        duration: if playback.duration_seconds > 0.0 {
            playback.duration_seconds
        } else {
            summary.as_ref().map(|t| t.duration).unwrap_or(0.0)
        },
        volume,
        muted: snapshot.audio_output.preferences.muted,
        mode: queue.as_ref().map(|q| q.mode).unwrap_or_default(),
        loved,
        cover,
        rate: snapshot.playback_processing.desired.playback_rate,
        busy: snapshot.busy,
        lyrics: snapshot.lyrics.clone().filter(|l| l.track_id == id),
        frame: snapshot.lyric_frame.clone(),
        lyrics_error: snapshot.lyrics_error.clone(),
        restored: queue.as_ref().is_some_and(|q| q.restore_pending),
        origin,
        id,
    }
}

// --- The play queue ----------------------------------------------------------

#[derive(Clone)]
pub struct QueueRow {
    pub entry_id: String,
    pub list_id: String,
    pub track_id: String,
    pub title: String,
    pub artist: String,
    pub seconds: f64,
    /// Queued with "play next" ahead of the list.
    pub temporary: bool,
    pub image: Option<String>,
}

/// The queue rows built for a queue revision and list.
type QueueCache = Option<(u64, usize, Arc<Vec<QueueRow>>)>;
/// Per lyric view: the current line, since when, and where it scrolled from.
type FocusMap = HashMap<u8, (Option<usize>, Option<Instant>, f32)>;

thread_local! {
    static QUEUE_CACHE: RefCell<QueueCache> = const { RefCell::new(None) };
    /// Per surface: (line index, when it became current, the offset it
    /// scrolled from).
    static FOCUS: RefCell<FocusMap> = RefCell::new(HashMap::new());
}

/// The queue in play order — "play next" entries first — and the current
/// entry id.
pub fn queue_rows(state: &State) -> (Arc<Vec<QueueRow>>, Option<String>) {
    let store = state.bridge.store();
    let Some(queue) = store.queue.clone() else {
        return (Arc::new(Vec::new()), None);
    };
    let catalog = (queue.origin_list == "catalog").then(|| store.snapshot.tracks.clone());
    let list_tracks = store
        .list_data
        .get(&queue.origin_list)
        .map(|d| d.tracks.clone())
        .unwrap_or_default();
    let temp_tracks = store
        .list_data
        .get("temp")
        .map(|d| d.tracks.clone())
        .unwrap_or_default();
    drop(store);
    let ptr = match &catalog {
        Some(catalog) => Arc::as_ptr(catalog) as usize,
        None => Arc::as_ptr(&list_tracks) as usize,
    };
    let current = queue.current.as_ref().map(|c| c.entry_id.clone());
    let cached = QUEUE_CACHE.with(|cache| {
        cache
            .borrow()
            .as_ref()
            .filter(|(revision, p, _)| *revision == queue.revision && *p == ptr)
            .map(|(_, _, rows)| rows.clone())
    });
    if let Some(rows) = cached {
        return (rows, current);
    }
    type Known = (String, String, f64, Option<String>);
    let mut names: HashMap<&str, Known> = HashMap::new();
    if let Some(catalog) = &catalog {
        for t in catalog.iter() {
            names.insert(
                t.id.as_str(),
                (t.title.clone(), t.artist.clone(), t.duration, None),
            );
        }
    }
    for t in list_tracks.iter().chain(temp_tracks.iter()) {
        names.entry(t.id.as_str()).or_insert((
            t.title.clone(),
            t.artists.join(" / "),
            t.duration_ms.unwrap_or(0) as f64 / 1000.0,
            crate::rows::cover_url(t),
        ));
    }
    let mut rows = Vec::with_capacity(queue.temporary.len() + queue.entries.len());
    for (entry, temporary) in queue
        .temporary
        .iter()
        .map(|e| (e, true))
        .chain(queue.entries.iter().map(|e| (e, false)))
    {
        let (title, artist, seconds, image) = names
            .get(entry.track_id.as_str())
            .cloned()
            .unwrap_or_else(|| ("（不在当前列表中的歌曲）".into(), String::new(), 0.0, None));
        rows.push(QueueRow {
            entry_id: entry.entry_id.clone(),
            list_id: entry.list_id.clone(),
            track_id: entry.track_id.clone(),
            title,
            artist,
            seconds,
            temporary,
            image,
        });
    }
    let rows = Arc::new(rows);
    QUEUE_CACHE.with(|cache| *cache.borrow_mut() = Some((queue.revision, ptr, rows.clone())));
    (rows, current)
}

/// Plays a queue entry.
pub fn play_queue_row(state: &State, row: &QueueRow) {
    use lx_gpui_player::backend::BackendCommand;
    if row.list_id == "catalog" {
        crate::app::act_command(
            state,
            BackendCommand::Play {
                id: row.track_id.clone(),
            },
        );
    } else {
        crate::app::act_command(
            state,
            BackendCommand::PlayListTrack {
                list_id: row.list_id.clone(),
                music_id: row.track_id.clone(),
                quality: None,
            },
        );
    }
}

// --- Lyrics --------------------------------------------------------------------

pub fn ease(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// The eased scroll offset of a lyric view: moves smoothly to `tops[current]`
/// whenever the current line changes. `slot` keeps views apart.
pub fn lyric_shift(slot: u8, current: Option<usize>, tops: &[f32]) -> f32 {
    let target = tops.get(current.unwrap_or(0)).copied().unwrap_or(0.0);
    FOCUS.with(|focus| {
        let mut focus = focus.borrow_mut();
        let (line, since, from) = focus.get(&slot).copied().unwrap_or((None, None, target));
        if line != current {
            let start = match (line, since) {
                (Some(old), Some(at)) => {
                    let old_y = tops.get(old).copied().unwrap_or(target);
                    let t = (at.elapsed().as_secs_f32() / 0.45).min(1.0);
                    from + (old_y - from) * ease(t)
                }
                _ => target,
            };
            focus.insert(slot, (current, Some(Instant::now()), start));
            start
        } else {
            let t = since
                .map(|at| (at.elapsed().as_secs_f32() / 0.45).min(1.0))
                .unwrap_or(1.0);
            from + (target - from) * ease(t)
        }
    })
}

/// Width of the already-sung part of the current line drawn at `size`.
pub fn sung_width(
    cx: &zsui::ZsCanvasLayoutContext<'_>,
    line: &lx_gpui_player::lyrics::parser::LyricLine,
    frame: Option<&LyricFrame>,
    size: f32,
    max: f32,
) -> f32 {
    let Some(frame) = frame else {
        return max;
    };
    if line.words.is_empty() {
        // Line-timed lyrics: the whole current line is lit.
        return max;
    }
    let weight = zsui::TextWeight::Bold;
    let word = frame.word_index.unwrap_or(0).min(line.words.len());
    let before: String = line.words[..word].iter().map(|w| w.text.as_str()).collect();
    let mut width = crate::paint::measure(cx, &before, size, weight);
    if let Some(current) = line.words.get(word) {
        let current_w = crate::paint::measure(cx, &current.text, size, weight);
        width += current_w * frame.word_progress.clamp(0.0, 1.0) as f32;
    }
    width.min(max)
}

/// Seeks to lyric line `index`.
pub fn seek_to_line(state: &State, index: usize) {
    let start = {
        let store = state.bridge.store();
        store
            .snapshot
            .lyrics
            .as_ref()
            .and_then(|l| l.document.lines.get(index))
            .map(|line| line.start_ms)
    };
    if let Some(start) = start {
        let offset = state.bridge.store().lyric_offset_ms;
        let seconds = ((start - offset) as f64 / 1000.0).max(0.0);
        crate::app::act_command(
            state,
            lx_gpui_player::backend::BackendCommand::Seek { seconds },
        );
    }
}
