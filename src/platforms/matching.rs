//! Deterministic metadata matching; invoke searches from an owned blocking job.

use super::{
    Platform, ProviderContext, ProviderError, ProviderRegistry, ProviderRequest, ProviderResponse,
};
pub use crate::domain::media::MATCHING_ALGORITHM_VERSION;
use crate::domain::{
    media::{MatchCandidate, MatchReason, track_record_fingerprint},
    music::TrackRecord,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    io::{self, Write},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

pub const SEARCH_PAGE_SIZE: u32 = 25;
pub const MAX_SEARCH_RECORDS: usize = 100;
pub const MAX_MATCH_JSON_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_SEARCH_WORKERS: usize = 4;

const VERSION_WORDS: &[&str] = &[
    "instrumental",
    "inst",
    "karaoke",
    "backing track",
    "minus one",
    "minus vocal",
    "no vocal",
    "off vocal",
    "vocal removed",
    "伴奏",
    "伴唱",
    "卡拉ok",
    "纯音乐",
    "无人声",
    "去人声",
    "消音",
    "オフボーカル",
    "インスト",
    "반주",
    "노래방",
    "live",
    "concert",
    "tour",
    "festival",
    "stage",
    "session",
    "现场",
    "演唱会",
    "巡演",
    "音乐会",
    "ライブ",
    "공연",
    "라이브",
    "cover",
    "tribute",
    "rendition",
    "remake",
    "翻唱",
    "致敬",
    "改编",
    "カバー",
    "커버",
    "remix",
    "mix",
    "club",
    "dance",
    "extended",
    "house",
    "edm",
    "dj",
    "dub",
    "trap",
    "混音",
    "重混",
    "电音",
    "リミックス",
    "리믹스",
    "acoustic",
    "unplugged",
    "stripped",
    "原声",
    "不插电",
    "アコースティック",
    "어쿠스틱",
    "demo",
    "小样",
    "试听",
    "デモ",
    "데모",
    "slowed",
    "slowed reverb",
    "sped up",
    "speed up",
    "nightcore",
    "慢速",
    "加速",
    "变速",
    "ナイトコア",
    "piano",
    "guitar",
    "orchestra",
    "symphony",
    "violin",
    "钢琴",
    "吉他",
    "管弦乐",
    "小提琴",
    "ピアノ",
    "ギター",
    "피아노",
    "기타",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedMusic {
    pub title: String,
    pub artists: Vec<String>,
    pub album: String,
    pub duration_ms: Option<u64>,
    pub version_signature: Vec<bool>,
}

/// Punctuation and whitespace follow LX's metadata filter; artist order is
/// Unicode lexical order, independent of the operating system locale.
pub fn normalize_text(text: &str) -> String {
    text.chars()
        .filter(|ch| {
            !ch.is_whitespace()
                && !matches!(
                    ch,
                    '\'' | '.'
                        | ','
                        | '，'
                        | '&'
                        | '"'
                        | '、'
                        | '('
                        | ')'
                        | '（'
                        | '）'
                        | '`'
                        | '~'
                        | '-'
                        | '<'
                        | '>'
                        | '|'
                        | '/'
                        | ']'
                        | '['
                        | '!'
                        | '！'
                )
        })
        .flat_map(char::to_lowercase)
        .collect()
}

pub fn normalize(track: &TrackRecord) -> NormalizedMusic {
    let title = normalize_text(&track.title);
    let mut artists: Vec<_> = track
        .artists
        .iter()
        .flat_map(|artist| artist.split(['、', '&', ';', '；', '/', ',', '，', '|']))
        .map(normalize_text)
        .filter(|artist| !artist.is_empty())
        .collect();
    artists.sort_unstable();
    artists.dedup();
    let version_signature = VERSION_WORDS
        .iter()
        .map(|word| title.contains(&normalize_text(word)))
        .collect();
    NormalizedMusic {
        title,
        artists,
        album: normalize_text(&track.album),
        duration_ms: track.duration_ms.filter(|duration| *duration != 0),
        version_signature,
    }
}

fn platform(source: &str) -> Option<Platform> {
    match source {
        "kw" => Some(Platform::Kw),
        "kg" => Some(Platform::Kg),
        "tx" => Some(Platform::Tx),
        "wy" => Some(Platform::Wy),
        "mg" => Some(Platform::Mg),
        _ => None,
    }
}

fn has_identity(track: &TrackRecord) -> bool {
    !track.id.trim().is_empty()
        && match &track.platform.song_id {
            serde_json::Value::String(id) => !id.trim().is_empty(),
            serde_json::Value::Number(id) => id.as_u64().is_some_and(|id| id != 0),
            _ => false,
        }
        && (track.platform.source != "kg"
            || track
                .metadata
                .get("hash")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|hash| !hash.trim().is_empty()))
}

fn validate_original(original: &TrackRecord) -> Result<Platform, ProviderError> {
    let platform = platform(&original.platform.source).ok_or_else(|| {
        ProviderError::invalid("matching requires a registered original platform")
    })?;
    let normalized = normalize(original);
    if !has_identity(original) || normalized.title.is_empty() || normalized.artists.is_empty() {
        return Err(ProviderError::invalid(
            "matching requires title, artist and platform identity",
        ));
    }
    Ok(platform)
}

fn compatible_artists(a: &[String], b: &[String]) -> bool {
    !a.is_empty()
        && !b.is_empty()
        && (a.iter().all(|artist| b.binary_search(artist).is_ok())
            || b.iter().all(|artist| a.binary_search(artist).is_ok()))
}

fn candidate(original: &NormalizedMusic, track: &TrackRecord) -> Option<(MatchCandidate, u8)> {
    if !has_identity(track) {
        return None;
    }
    let found = normalize(track);
    if found.title.is_empty()
        || !compatible_artists(&original.artists, &found.artists)
        || found.version_signature != original.version_signature
    {
        return None;
    }
    let duration_exact = match (original.duration_ms, found.duration_ms) {
        (Some(a), Some(b)) if a.abs_diff(b) > 5000 => return None,
        (Some(a), Some(b)) => a == b,
        _ => false,
    };
    let title_exact = original.title == found.title;
    let artists_exact = original.artists == found.artists;
    let title_contains =
        original.title.contains(&found.title) || found.title.contains(&original.title);
    let album_exact = !original.album.is_empty() && original.album == found.album;
    let pass = if title_exact {
        0
    } else if artists_exact && title_contains {
        1
    } else if title_contains && (original.album.is_empty() || album_exact) {
        2
    } else {
        return None;
    };
    let score = if title_exact && artists_exact && duration_exact {
        10000
    } else if title_exact && artists_exact && album_exact {
        9000
    } else if title_exact && artists_exact {
        8000
    } else if title_exact && duration_exact {
        7000
    } else if artists_exact && duration_exact {
        6000
    } else if duration_exact {
        5000
    } else if title_exact {
        4000
    } else if artists_exact {
        3000
    } else if album_exact {
        2000
    } else {
        1000
    };
    let mut reasons = vec![
        if title_exact {
            MatchReason::ExactTitle
        } else {
            MatchReason::TitleContains
        },
        if artists_exact {
            MatchReason::ExactArtists
        } else {
            MatchReason::CompatibleArtists
        },
        MatchReason::SameVersion,
    ];
    if album_exact {
        reasons.push(MatchReason::ExactAlbum);
    }
    reasons.push(match (original.duration_ms, found.duration_ms) {
        (Some(_), Some(_)) if duration_exact => MatchReason::DurationExact,
        (Some(_), Some(_)) => MatchReason::DurationWithinFiveSeconds,
        _ => MatchReason::DurationUnknown,
    });
    Some((
        MatchCandidate {
            track: track.clone(),
            score,
            reasons,
            actual_quality: None,
        },
        pass,
    ))
}

struct JsonBudget {
    used: usize,
    limit: usize,
    exceeded: bool,
}
impl Write for JsonBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.used) {
            self.exceeded = true;
            return Err(io::Error::other("matching JSON budget exceeded"));
        }
        self.used += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn json_bytes(value: &impl Serialize, limit: usize) -> Result<usize, ProviderError> {
    let mut budget = JsonBudget {
        used: 0,
        limit,
        exceeded: false,
    };
    if let Err(error) = serde_json::to_writer(&mut budget, value) {
        return Err(if budget.exceeded {
            ProviderError::ResponseTooLarge {
                limit: MAX_MATCH_JSON_BYTES,
            }
        } else {
            ProviderError::parse(error.to_string())
        });
    }
    Ok(budget.used)
}

/// Known durations differ by at most 5,000 ms. None and zero are unknown;
/// unknown durations never earn an exact-duration score. Empty artists are
/// rejected and artist containment uses whole normalized artist tokens.
pub fn rank_candidates(
    original: &TrackRecord,
    records: &[TrackRecord],
) -> Result<Vec<MatchCandidate>, ProviderError> {
    let source = validate_original(original)?;
    if records.len() > MAX_SEARCH_RECORDS {
        return Err(ProviderError::invalid(
            "matching permits at most 100 records",
        ));
    }
    json_bytes(&records, MAX_MATCH_JSON_BYTES)?;
    let normalized = normalize(original);
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for record in records {
        if platform(&record.platform.source).is_none_or(|found| found == source) {
            continue;
        }
        if let Some((candidate, _)) = candidate(&normalized, record) {
            let fingerprint = track_record_fingerprint(record)
                .map_err(|error| ProviderError::invalid(error.to_string()))?;
            if seen.insert(fingerprint.clone()) {
                result.push((candidate, fingerprint));
            }
        }
    }
    result.sort_by(|(a, af), (b, bf)| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.track.platform.source.cmp(&b.track.platform.source))
            .then_with(|| a.track.id.cmp(&b.track.id))
            .then_with(|| af.cmp(bf))
    });
    let mut identities = BTreeSet::new();
    let result: Vec<_> = result
        .into_iter()
        .map(|(candidate, _)| candidate)
        .filter(|candidate| {
            identities.insert((
                candidate.track.platform.source.clone(),
                candidate.track.id.clone(),
            ))
        })
        .collect();
    json_bytes(&result, MAX_MATCH_JSON_BYTES)?;
    Ok(result)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PlatformMatchFailure {
    pub platform: Platform,
    pub error: ProviderError,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MatchSet {
    pub algorithm_version: u32,
    pub original_record_sha256: String,
    pub candidates: Vec<MatchCandidate>,
    pub failures: Vec<PlatformMatchFailure>,
    pub searched_platforms: Vec<Platform>,
    pub examined_records: usize,
    pub truncated_records: usize,
}

#[derive(Default)]
struct Admission {
    active: Mutex<usize>,
    changed: Condvar,
    waiting: AtomicUsize,
}
struct Permit<'a>(&'a Admission);
struct Waiting<'a>(&'a AtomicUsize);
impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
impl Admission {
    fn acquire(&self, context: &ProviderContext) -> Result<Permit<'_>, ProviderError> {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut waiting = None;
        loop {
            context.check()?;
            if *active < MAX_SEARCH_WORKERS {
                *active += 1;
                return Ok(Permit(self));
            }
            if waiting.is_none() {
                self.waiting.fetch_add(1, Ordering::AcqRel);
                waiting = Some(Waiting(&self.waiting));
            }
            active = self
                .changed
                .wait_timeout(active, context.remaining()?.min(Duration::from_millis(10)))
                .unwrap_or_else(|error| error.into_inner())
                .0;
        }
    }
}
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut active = self
            .0
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *active -= 1;
        self.0.changed.notify_one();
    }
}

#[derive(Clone)]
pub struct MatchingService {
    registry: Arc<ProviderRegistry>,
    admission: Arc<Admission>,
}
impl MatchingService {
    pub fn new(registry: Arc<ProviderRegistry>) -> Self {
        Self {
            registry,
            admission: Arc::new(Admission::default()),
        }
    }
    pub fn active_workers(&self) -> usize {
        *self
            .admission
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
    pub fn waiting_workers(&self) -> usize {
        self.admission.waiting.load(Ordering::Acquire)
    }
    /// Executes page-one metadata searches only. The caller owns this blocking
    /// invocation; every scoped worker is joined before it returns.
    pub fn search(
        &self,
        original: &TrackRecord,
        allowed: &[Platform],
        context: &ProviderContext,
    ) -> Result<MatchSet, ProviderError> {
        context.check()?;
        let source = validate_original(original)?;
        json_bytes(original, MAX_MATCH_JSON_BYTES)?;
        let original_record_sha256 = track_record_fingerprint(original)
            .map_err(|error| ProviderError::invalid(error.to_string()))?;
        if allowed.len() > 5
            || allowed.iter().copied().collect::<BTreeSet<_>>().len() != allowed.len()
        {
            return Err(ProviderError::invalid(
                "matching platforms must be distinct and contain at most five entries",
            ));
        }
        let keyword = format!(
            "{} {}",
            original.title.trim(),
            original.artists.join("、").trim()
        )
        .trim()
        .to_owned();
        if keyword.len() > 2048 {
            return Err(ProviderError::invalid(
                "matching search keyword exceeds 2048 bytes",
            ));
        }
        let platforms: Vec<_> = self
            .registry
            .platforms()
            .into_iter()
            .filter(|platform| *platform != source && allowed.contains(platform))
            .collect();
        let replies = thread::scope(|scope| {
            let mut workers = Vec::new();
            for platform in &platforms {
                let platform = *platform;
                let keyword = &keyword;
                let worker = thread::Builder::new()
                    .name(format!("music-match-{}", platform.as_str()))
                    .spawn_scoped(scope, move || {
                        let _permit = self.admission.acquire(context)?;
                        self.registry.execute(
                            platform,
                            ProviderRequest::Search {
                                keyword: keyword.clone(),
                                page: 1,
                                limit: SEARCH_PAGE_SIZE,
                            },
                            context,
                        )
                    });
                workers.push((platform, worker));
            }
            workers
                .into_iter()
                .map(|(platform, worker)| {
                    let result = match worker {
                        Ok(worker) => worker.join().unwrap_or_else(|_| {
                            Err(ProviderError::Network {
                                message: "matching provider worker panicked".into(),
                            })
                        }),
                        Err(error) => Err(ProviderError::Network {
                            message: error.to_string(),
                        }),
                    };
                    (platform, result)
                })
                .collect::<Vec<_>>()
        });
        context.check()?;
        let mut failures = Vec::new();
        let mut selected = Vec::new();
        let mut examined_records = 0;
        let mut truncated_records = 0;
        let mut used_json = 0;
        let normalized = normalize(original);
        for (platform, result) in replies {
            let result = result.and_then(|response| {
                let ProviderResponse::Tracks(page) = response else {
                    return Err(ProviderError::parse(
                        "matching search returned a non-track result",
                    ));
                };
                if page.page != 1
                    || page.limit != SEARCH_PAGE_SIZE
                    || (page.total == 0 && !page.items.is_empty())
                    || (platform != Platform::Kg && page.total < page.items.len() as u64)
                    || (platform != Platform::Kg && page.items.len() > SEARCH_PAGE_SIZE as usize)
                    || page
                        .items
                        .iter()
                        .any(|item| item.platform.source != platform.as_str())
                {
                    return Err(ProviderError::parse(
                        "matching search page metadata is inconsistent",
                    ));
                }
                used_json += json_bytes(&page, MAX_MATCH_JSON_BYTES.saturating_sub(used_json))?;
                truncated_records += page.items.len().saturating_sub(SEARCH_PAGE_SIZE as usize);
                let records: Vec<_> = page
                    .items
                    .into_iter()
                    .take(SEARCH_PAGE_SIZE as usize)
                    .collect();
                examined_records += records.len();
                let mut matches: Vec<_> = records
                    .iter()
                    .enumerate()
                    .filter_map(|(index, record)| {
                        candidate(&normalized, record)
                            .map(|(candidate, pass)| (pass, index, candidate))
                    })
                    .collect();
                matches.sort_by_key(|(pass, index, _)| (*pass, *index));
                Ok(matches
                    .into_iter()
                    .next()
                    .map(|(_, _, candidate)| candidate.track))
            });
            match result {
                Ok(Some(record)) => selected.push(record),
                Ok(None) => (),
                Err(ProviderError::ResponseTooLarge { .. }) => {
                    return Err(ProviderError::ResponseTooLarge {
                        limit: MAX_MATCH_JSON_BYTES,
                    });
                }
                Err(error) => failures.push(PlatformMatchFailure { platform, error }),
            }
        }
        context.check()?;
        let result = MatchSet {
            algorithm_version: MATCHING_ALGORITHM_VERSION,
            original_record_sha256,
            candidates: rank_candidates(original, &selected)?,
            failures,
            searched_platforms: platforms,
            examined_records,
            truncated_records,
        };
        json_bytes(&result, MAX_MATCH_JSON_BYTES)?;
        Ok(result)
    }
}
