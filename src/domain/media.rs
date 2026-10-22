//! Persistent resolution information and ordered source matches, independent of URL consent.
use super::music::TrackRecord;
use anyhow::{Context, Result, ensure};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fmt;

pub const MATCHING_ALGORITHM_VERSION: u32 = 1;
pub const MAX_MATCH_CANDIDATES: usize = 100;
pub const MAX_MEDIA_CACHE_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;
pub const DEFAULT_RESOLVED_MEDIA_TTL_MS: u64 = 15 * 60 * 1000;
pub const MAX_RESOLVED_MEDIA_TTL_MS: u64 = 24 * 60 * 60 * 1000;
pub const DEFAULT_SOURCE_MATCH_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;
pub const MAX_SOURCE_MATCH_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CachedMediaUrl(String);
impl CachedMediaUrl {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        ensure!(
            !value.is_empty() && value.len() <= 2048,
            "Cached media URL exceeds its byte limit"
        );
        ensure!(
            !value
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace()),
            "Cached media URL contains whitespace or control characters"
        );
        let parsed =
            Url::parse(&value).map_err(|_| anyhow::anyhow!("Cached media URL is invalid"))?;
        let user_info = value.split_once("://").is_some_and(|(_, tail)| {
            tail.split(['/', '?', '#'])
                .next()
                .is_some_and(|authority| authority.contains('@'))
        });
        ensure!(
            matches!(parsed.scheme(), "http" | "https")
                && parsed.host_str().is_some()
                && !user_info
                && parsed.username().is_empty()
                && parsed.password().is_none(),
            "Cached media URL must be HTTP(S), with a host and no user information"
        );
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for CachedMediaUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CachedMediaUrl([redacted])")
    }
}
impl TryFrom<String> for CachedMediaUrl {
    type Error = anyhow::Error;
    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}
impl From<CachedMediaUrl> for String {
    fn from(value: CachedMediaUrl) -> Self {
        value.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaInvalidationReason {
    Expired,
    ClockRollback,
    HttpRejected,
    MetadataChanged,
    SourceChanged,
    ResolutionFailed,
    UnsupportedVersion,
    Manual,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaFailureKind {
    HttpRejected,
    SourceFailed,
    InvalidUrl,
    NoPlayableMatch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedMediaKey {
    pub original_track_id: String,
    pub original_platform: String,
    pub actual_quality: String,
    pub source_sha256: String,
    pub source_policy_version: u64,
    pub music_info_sha256: String,
}
impl ResolvedMediaKey {
    pub fn new(
        original: &TrackRecord,
        actual_quality: &str,
        source_sha256: &str,
        source_policy_version: u64,
        music_info: &Value,
    ) -> Result<Self> {
        let key = Self {
            original_track_id: original.id.clone(),
            original_platform: original.platform.source.clone(),
            actual_quality: actual_quality.to_owned(),
            source_sha256: source_sha256.to_owned(),
            source_policy_version,
            music_info_sha256: music_info_fingerprint(music_info)?,
        };
        key.validate()?;
        Ok(key)
    }
    pub fn validate(&self) -> Result<()> {
        validate_text(&self.original_track_id, 1024, "Original track identity")?;
        validate_text(&self.original_platform, 64, "Original platform")?;
        validate_text(&self.actual_quality, 64, "Actual quality")?;
        validate_digest(&self.source_sha256)?;
        validate_digest(&self.music_info_sha256)?;
        ensure!(
            self.source_policy_version > 0 && self.source_policy_version <= i64::MAX as u64,
            "Source policy version is outside its supported range"
        );
        Ok(())
    }
    pub fn fingerprint(&self) -> Result<String> {
        self.validate()?;
        fingerprint(self)
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedMedia {
    pub key: ResolvedMediaKey,
    pub url: CachedMediaUrl,
    pub matched_track: Option<TrackRecord>,
    pub resolved_at_ms: u64,
    pub expire_at_ms: u64,
    pub invalidation: Option<MediaInvalidationReason>,
    pub failure: Option<MediaFailureKind>,
}
impl fmt::Debug for ResolvedMedia {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedMedia")
            .field("key", &self.key)
            .field("url", &self.url)
            .field(
                "matched_track_id",
                &self.matched_track.as_ref().map(|track| &track.id),
            )
            .field("resolved_at_ms", &self.resolved_at_ms)
            .field("expire_at_ms", &self.expire_at_ms)
            .field("invalidation", &self.invalidation)
            .field("failure", &self.failure)
            .finish()
    }
}
impl ResolvedMedia {
    pub fn new(
        key: ResolvedMediaKey,
        url: CachedMediaUrl,
        matched_track: Option<TrackRecord>,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<Self> {
        let value = Self {
            key,
            url,
            matched_track,
            resolved_at_ms: now_ms,
            expire_at_ms: expiry(now_ms, ttl_ms, MAX_RESOLVED_MEDIA_TTL_MS)?,
            invalidation: None,
            failure: None,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        self.key.validate()?;
        CachedMediaUrl::new(self.url.as_str())?;
        validate_window(
            self.resolved_at_ms,
            self.expire_at_ms,
            MAX_RESOLVED_MEDIA_TTL_MS,
        )?;
        if let Some(track) = &self.matched_track {
            validate_cached_track(track)?;
        }
        ensure!(
            self.failure.is_none() || self.invalidation.is_some(),
            "Failed media cache entry must be invalidated"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchReason {
    ExactTitle,
    TitleContains,
    ExactArtists,
    CompatibleArtists,
    ExactAlbum,
    DurationExact,
    DurationWithinFiveSeconds,
    DurationUnknown,
    SameVersion,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchCandidate {
    pub track: TrackRecord,
    pub score: u16,
    pub reasons: Vec<MatchReason>,
    pub actual_quality: Option<String>,
}
impl MatchCandidate {
    pub fn validate(&self) -> Result<()> {
        validate_cached_track(&self.track)?;
        ensure!(
            self.score <= 10000,
            "Match score exceeds its supported range"
        );
        ensure!(
            !self.reasons.is_empty() && self.reasons.len() <= 16,
            "Match reasons are empty or exceed their limit"
        );
        let mut reasons = Vec::new();
        for reason in &self.reasons {
            ensure!(
                !reasons.contains(reason),
                "Match reasons contain a duplicate"
            );
            reasons.push(*reason);
        }
        if let Some(quality) = &self.actual_quality {
            validate_text(quality, 64, "Matched actual quality")?;
            ensure!(
                self.track.platform.qualities.contains(quality),
                "Matched actual quality is absent from its track identity"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceMatchKey {
    pub original_track_id: String,
    pub original_platform: String,
    pub original_record_sha256: String,
    pub algorithm_version: u32,
}
impl SourceMatchKey {
    pub fn new(original: &TrackRecord) -> Result<Self> {
        let key = Self {
            original_track_id: original.id.clone(),
            original_platform: original.platform.source.clone(),
            original_record_sha256: track_record_fingerprint(original)?,
            algorithm_version: MATCHING_ALGORITHM_VERSION,
        };
        key.validate()?;
        Ok(key)
    }
    pub fn validate(&self) -> Result<()> {
        validate_text(&self.original_track_id, 1024, "Original track identity")?;
        validate_text(&self.original_platform, 64, "Original platform")?;
        validate_digest(&self.original_record_sha256)?;
        ensure!(
            self.algorithm_version == MATCHING_ALGORITHM_VERSION,
            "Unsupported source matching algorithm version"
        );
        Ok(())
    }
    pub fn fingerprint(&self) -> Result<String> {
        self.validate()?;
        fingerprint(self)
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceMatches {
    pub key: SourceMatchKey,
    pub candidates: Vec<MatchCandidate>,
    pub matched_at_ms: u64,
    pub expire_at_ms: u64,
    pub invalidation: Option<MediaInvalidationReason>,
    #[serde(default)]
    pub searched_platforms: Vec<String>,
    #[serde(default)]
    pub failed_platforms: Vec<String>,
}
impl SourceMatches {
    pub fn new(
        key: SourceMatchKey,
        candidates: Vec<MatchCandidate>,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<Self> {
        let value = Self {
            key,
            candidates,
            matched_at_ms: now_ms,
            expire_at_ms: expiry(now_ms, ttl_ms, MAX_SOURCE_MATCH_TTL_MS)?,
            invalidation: None,
            searched_platforms: Vec::new(),
            failed_platforms: Vec::new(),
        };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        self.key.validate()?;
        let mut searched = std::collections::BTreeSet::new();
        for platform in &self.searched_platforms {
            ensure!(
                matches!(platform.as_str(), "kw" | "kg" | "tx" | "wy" | "mg")
                    && searched.insert(platform),
                "Source match search diagnostics are invalid"
            );
        }
        let mut failed = std::collections::BTreeSet::new();
        for platform in &self.failed_platforms {
            ensure!(
                searched.contains(platform) && failed.insert(platform),
                "Source match failure diagnostics are invalid"
            );
        }
        validate_window(
            self.matched_at_ms,
            self.expire_at_ms,
            MAX_SOURCE_MATCH_TTL_MS,
        )?;
        ensure!(
            self.candidates.len() <= MAX_MATCH_CANDIDATES,
            "Source match candidate count exceeds 100"
        );
        let mut identities = std::collections::BTreeSet::new();
        for candidate in &self.candidates {
            candidate.validate()?;
            ensure!(
                identities.insert((
                    candidate.track.platform.source.clone(),
                    candidate.track.id.clone()
                )),
                "Source match candidates contain a duplicate identity"
            );
        }
        ensure!(
            self.candidates
                .windows(2)
                .all(|pair| pair[0].score >= pair[1].score),
            "Source match candidates are not in score order"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum MediaCacheLookup<T> {
    Hit(T),
    Miss,
    Invalidated(MediaInvalidationReason),
}
impl<T> MediaCacheLookup<T> {
    pub fn hit(self) -> Option<T> {
        match self {
            Self::Hit(value) => Some(value),
            _ => None,
        }
    }
}

pub fn track_record_fingerprint(record: &TrackRecord) -> Result<String> {
    validate_cached_track(record)?;
    fingerprint(record)
}
pub fn music_info_fingerprint(music_info: &Value) -> Result<String> {
    fingerprint(music_info)
}
fn fingerprint(value: &impl Serialize) -> Result<String> {
    let mut value = serde_json::to_value(value).context("Cannot encode media cache identity")?;
    canonicalize(&mut value);
    let bytes = serde_json::to_vec(&value)?;
    ensure!(
        bytes.len() <= MAX_MEDIA_CACHE_DOCUMENT_BYTES,
        "Media cache identity exceeds 8 MiB"
    );
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
fn canonicalize(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (_, value) in object.iter_mut() {
                canonicalize(value);
            }
            let mut entries: Vec<_> = std::mem::take(object).into_iter().collect();
            entries.sort_by(|(first, _), (second, _)| first.cmp(second));
            object.extend(entries);
        }
        Value::Array(items) => {
            for item in items {
                canonicalize(item);
            }
        }
        _ => (),
    }
}
fn validate_text(value: &str, maximum: usize, name: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_control),
        "{name} is empty, too long, or contains control characters"
    );
    Ok(())
}
fn validate_digest(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "Media cache identity requires a lowercase SHA-256 digest"
    );
    Ok(())
}
fn validate_cached_track(track: &TrackRecord) -> Result<()> {
    validate_text(&track.id, 1024, "Matched track identity")?;
    validate_text(&track.platform.source, 64, "Matched platform")?;
    ensure!(
        track.platform.song_id.is_string() || track.platform.song_id.is_number(),
        "Matched platform song identity is invalid"
    );
    ensure!(
        track.platform.qualities.len() <= 32,
        "Matched track has too many qualities"
    );
    for quality in &track.platform.qualities {
        validate_text(quality, 64, "Matched quality")?;
    }
    Ok(())
}
fn expiry(now_ms: u64, ttl_ms: u64, maximum: u64) -> Result<u64> {
    ensure!(
        ttl_ms > 0 && ttl_ms <= maximum,
        "Media cache TTL is outside its supported range"
    );
    let end = now_ms
        .checked_add(ttl_ms)
        .context("Media cache expiry overflow")?;
    validate_window(now_ms, end, maximum)?;
    Ok(end)
}
fn validate_window(start: u64, end: u64, maximum: u64) -> Result<()> {
    ensure!(
        start <= i64::MAX as u64 && end <= i64::MAX as u64 && end > start && end - start <= maximum,
        "Media cache time window is invalid or unbounded"
    );
    Ok(())
}
