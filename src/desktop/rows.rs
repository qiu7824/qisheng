//! Uniform track rows for every page, plus album/artist groups of the local
//! catalog. Both are cached by source identity so rebuilding a view does not
//! re-scan large collections.

use crate::{
    app::{GroupKind, Page, State},
    bridge::love_key,
};
use lx_gpui_player::{
    backend::TrackSummary,
    domain::music::{PlatformTrackRef, TrackRecord},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug)]
pub struct Row {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub seconds: f64,
    pub source: String,
    pub love_key: String,
    /// Index into the page's source collection.
    pub index: usize,
    /// The cover address an online result carries, if any.
    pub image: Option<String>,
}

/// Where a page's rows come from and therefore how they play.
#[derive(Clone, Debug)]
pub enum RowSource {
    /// The whole local catalog, queued in catalog order.
    Catalog,
    /// A persistent list.
    List(String),
    /// Records that play through the temporary list: provider results, or
    /// one album or artist of the catalog.
    Temp(Arc<Vec<TrackRecord>>),
}

#[derive(Clone, Debug)]
pub struct Rows {
    pub source: RowSource,
    pub items: Arc<Vec<Row>>,
    /// Rows before filtering.
    pub total: usize,
}

impl Rows {
    fn empty(source: RowSource) -> Self {
        Self {
            source,
            items: Arc::new(Vec::new()),
            total: 0,
        }
    }

    pub fn seconds(&self) -> f64 {
        self.items.iter().map(|r| r.seconds).sum()
    }
}

#[derive(Default)]
pub struct RowCache {
    entry: Mutex<Option<(String, usize, String, Rows)>>,
}

enum Records {
    Catalog(Arc<Vec<TrackSummary>>),
    Records(Arc<Vec<TrackRecord>>),
}

impl RowCache {
    /// The rows of `page` after the current filter.
    pub fn rows(&self, state: &State, page: &Page) -> Rows {
        let store = state.bridge.store();
        let (key, ptr, source, records): (String, usize, RowSource, Records) = match page {
            Page::Songs => {
                let tracks = store.snapshot.tracks.clone();
                (
                    "catalog".into(),
                    Arc::as_ptr(&tracks) as usize,
                    RowSource::Catalog,
                    Records::Catalog(tracks),
                )
            }
            Page::Group(kind, name) => {
                let tracks = store.snapshot.tracks.clone();
                let ptr = Arc::as_ptr(&tracks) as usize;
                drop(store);
                let records: Vec<TrackRecord> = tracks
                    .iter()
                    .filter(|t| match kind {
                        GroupKind::Album => album_of(t) == name.as_str(),
                        GroupKind::Artist => artist_of(t) == name.as_str(),
                    })
                    .map(catalog_record)
                    .collect();
                let records = Arc::new(records);
                return self.build(
                    state,
                    format!("group:{kind:?}:{name}"),
                    ptr,
                    RowSource::Temp(records.clone()),
                    Records::Records(records),
                );
            }
            Page::List(id) => match store.list_data.get(id) {
                Some(data) => (
                    format!("list:{id}"),
                    Arc::as_ptr(&data.tracks) as usize,
                    RowSource::List(id.clone()),
                    Records::Records(data.tracks.clone()),
                ),
                None => return Rows::empty(RowSource::List(id.clone())),
            },
            Page::Search => {
                let tracks = store.search.tracks.clone();
                (
                    "search".into(),
                    Arc::as_ptr(&tracks) as usize,
                    RowSource::Temp(tracks.clone()),
                    Records::Records(tracks),
                )
            }
            Page::Charts => {
                let tracks = store.chart.tracks.clone();
                (
                    "charts".into(),
                    Arc::as_ptr(&tracks) as usize,
                    RowSource::Temp(tracks.clone()),
                    Records::Records(tracks),
                )
            }
            Page::Playlist(_, id) => {
                if &store.playlist.id != id {
                    return Rows::empty(RowSource::Temp(Arc::new(Vec::new())));
                }
                let tracks = store.playlist.tracks.clone();
                (
                    format!("playlist:{id}"),
                    Arc::as_ptr(&tracks) as usize,
                    RowSource::Temp(tracks.clone()),
                    Records::Records(tracks),
                )
            }
            _ => return Rows::empty(RowSource::Catalog),
        };
        drop(store);
        self.build(state, key, ptr, source, records)
    }

    fn build(
        &self,
        state: &State,
        key: String,
        ptr: usize,
        source: RowSource,
        records: Records,
    ) -> Rows {
        let filter = state.ui.filter.trim().to_lowercase();
        let mut entry = self.entry.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((k, p, f, rows)) = entry.as_ref()
            && *k == key
            && *p == ptr
            && *f == filter
        {
            return rows.clone();
        }
        let mut items = Vec::new();
        let total;
        match &records {
            Records::Catalog(tracks) => {
                total = tracks.len();
                for (index, track) in tracks.iter().enumerate() {
                    if matches(&filter, &track.title, &track.artist, &track.album) {
                        items.push(catalog_row(track, index));
                    }
                }
            }
            Records::Records(tracks) => {
                total = tracks.len();
                for (index, track) in tracks.iter().enumerate() {
                    let artist = track.artists.join(" / ");
                    if matches(&filter, &track.title, &artist, &track.album) {
                        items.push(Row {
                            id: track.id.clone(),
                            title: track.title.clone(),
                            artist,
                            album: track.album.clone(),
                            seconds: track.duration_ms.unwrap_or(0) as f64 / 1000.0,
                            source: track.platform.source.clone(),
                            love_key: love_key(track),
                            index,
                            image: cover_url(track),
                        });
                    }
                }
            }
        }
        let rows = Rows {
            source,
            items: Arc::new(items),
            total,
        };
        *entry = Some((key, ptr, filter, rows.clone()));
        rows
    }
}

fn matches(filter: &str, title: &str, artist: &str, album: &str) -> bool {
    filter.is_empty()
        || [title, artist, album]
            .iter()
            .any(|field| field.to_lowercase().contains(filter))
}

fn catalog_row(track: &TrackSummary, index: usize) -> Row {
    Row {
        id: track.id.clone(),
        title: track.title.clone(),
        artist: track.artist.clone(),
        album: track.album.clone(),
        seconds: track.duration,
        source: "local".into(),
        love_key: format!("local|{}", serde_json::json!(track.id)),
        index,
        image: None,
    }
}

/// The cover address in an online record's metadata.
pub fn cover_url(track: &TrackRecord) -> Option<String> {
    ["img", "picUrl", "pic"]
        .iter()
        .filter_map(|key| track.metadata.get(*key))
        .filter_map(|value| value.as_str())
        .map(str::trim)
        .find(|url| url.starts_with("http"))
        .map(str::to_string)
}

/// The record the backend builds for a catalog track when it is queued, so
/// catalog tracks can be added to lists or played through the temp list.
pub fn catalog_record(track: &TrackSummary) -> TrackRecord {
    TrackRecord {
        id: track.id.clone(),
        lx_id: track.id.clone(),
        title: track.title.clone(),
        artists: track
            .artist
            .split(['、', '/'])
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect(),
        album: track.album.clone(),
        duration_ms: Some((track.duration * 1000.0) as u64),
        platform: PlatformTrackRef {
            source: "local".into(),
            song_id: serde_json::json!(track.id),
            qualities: Vec::new(),
        },
        local_asset_id: None,
        metadata: serde_json::Map::new(),
        wire: None,
    }
}

pub fn album_of(track: &TrackSummary) -> &str {
    if track.album.trim().is_empty() {
        "未知专辑"
    } else {
        track.album.trim()
    }
}

pub fn artist_of(track: &TrackSummary) -> &str {
    if track.artist.trim().is_empty() {
        "未知歌手"
    } else {
        track.artist.trim()
    }
}

#[derive(Clone, Debug)]
pub struct Group {
    pub name: String,
    /// The album's most frequent artist, or the artist's album count.
    pub detail: String,
    pub count: usize,
    /// The first catalog track of the group, whose artwork stands for it.
    pub first_id: String,
}

#[derive(Default)]
pub struct GroupCache {
    albums: Mutex<Option<(usize, Arc<Vec<Group>>)>>,
    artists: Mutex<Option<(usize, Arc<Vec<Group>>)>>,
}

impl GroupCache {
    pub fn groups(&self, state: &State, kind: GroupKind) -> Arc<Vec<Group>> {
        let tracks = state.bridge.store().snapshot.tracks.clone();
        let ptr = Arc::as_ptr(&tracks) as usize;
        let slot = match kind {
            GroupKind::Album => &self.albums,
            GroupKind::Artist => &self.artists,
        };
        let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((p, groups)) = slot.as_ref()
            && *p == ptr
        {
            return groups.clone();
        }
        let mut order: Vec<String> = Vec::new();
        #[allow(clippy::type_complexity)]
        let mut map: HashMap<String, (usize, HashMap<String, usize>, String)> = HashMap::new();
        for track in tracks.iter() {
            let (key, other) = match kind {
                GroupKind::Album => (album_of(track), artist_of(track)),
                GroupKind::Artist => (artist_of(track), album_of(track)),
            };
            let entry = map.entry(key.to_string()).or_insert_with(|| {
                order.push(key.to_string());
                (0, HashMap::new(), track.id.clone())
            });
            entry.0 += 1;
            *entry.1.entry(other.to_string()).or_default() += 1;
        }
        let groups: Vec<Group> = order
            .into_iter()
            .map(|name| {
                let (count, others, first_id) = &map[&name];
                let detail = match kind {
                    GroupKind::Album => others
                        .iter()
                        .max_by_key(|(_, n)| **n)
                        .map(|(a, _)| a.clone())
                        .unwrap_or_default(),
                    GroupKind::Artist => format!("{} 张专辑", others.len()),
                };
                Group {
                    name,
                    detail,
                    count: *count,
                    first_id: first_id.clone(),
                }
            })
            .collect();
        let groups = Arc::new(groups);
        *slot = Some((ptr, groups.clone()));
        groups
    }
}
