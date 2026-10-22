//! Picture information and verified encoded artwork, separate from audio consent.
use super::{
    media::{CachedMediaUrl, track_record_fingerprint},
    music::{LocalAsset, TrackRecord},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

pub const ARTWORK_VERSION: u32 = 1;
pub const MAX_ARTWORK_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_ARTWORK_INFO_ROWS: usize = 4096;
pub const MAX_ARTWORK_INFO_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_ARTWORK_BLOB_ROWS: usize = 1024;
pub const MAX_ARTWORK_BLOB_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_ARTWORK_PAGE_BYTES: usize = 64 * 1024;
pub const REMOTE_ARTWORK_TTL_MS: u64 = 15 * 60 * 1000;
pub const LOCAL_ARTWORK_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ArtworkUrl(String);
impl ArtworkUrl {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        CachedMediaUrl::new(value.clone())?;
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for ArtworkUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ArtworkUrl([redacted])")
    }
}
impl TryFrom<String> for ArtworkUrl {
    type Error = anyhow::Error;
    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}
impl From<ArtworkUrl> for String {
    fn from(value: ArtworkUrl) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ArtworkAuthority {
    Native,
    LocalAsset {
        asset_id: String,
        permission_sha256: String,
    },
    Source {
        source_sha256: String,
        policy_version: u64,
        local_permission_sha256: Option<String>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtworkKey {
    pub version: u32,
    pub original_track_id: String,
    pub original_platform: String,
    pub original_record_sha256: String,
    pub matched_record_sha256: Option<String>,
    pub authority: ArtworkAuthority,
}
impl ArtworkKey {
    pub fn new(
        original: &TrackRecord,
        matched: Option<&TrackRecord>,
        authority: ArtworkAuthority,
    ) -> Result<Self> {
        let value = Self {
            version: ARTWORK_VERSION,
            original_track_id: original.id.clone(),
            original_platform: original.platform.source.clone(),
            original_record_sha256: track_record_fingerprint(original)?,
            matched_record_sha256: matched.map(track_record_fingerprint).transpose()?,
            authority,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == ARTWORK_VERSION,
            "Unsupported artwork version"
        );
        text(&self.original_track_id, 1024)?;
        text(&self.original_platform, 64)?;
        digest(&self.original_record_sha256)?;
        if let Some(value) = &self.matched_record_sha256 {
            digest(value)?;
        }
        match &self.authority {
            ArtworkAuthority::Native => (),
            ArtworkAuthority::LocalAsset {
                asset_id,
                permission_sha256,
            } => {
                text(asset_id, 1024)?;
                digest(permission_sha256)?;
            }
            ArtworkAuthority::Source {
                source_sha256,
                policy_version,
                local_permission_sha256,
            } => {
                digest(source_sha256)?;
                ensure!(
                    *policy_version > 0 && *policy_version <= i64::MAX as u64,
                    "Invalid picture source policy"
                );
                if let Some(value) = local_permission_sha256 {
                    digest(value)?;
                }
            }
        }
        Ok(())
    }
    pub fn fingerprint(&self) -> Result<String> {
        self.validate()?;
        document_fingerprint(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ArtworkProvenance {
    Embedded {
        asset_id: String,
        permission_sha256: String,
    },
    Metadata {
        platform: String,
    },
    Provider {
        platform: String,
    },
    Source {
        source_sha256: String,
        policy_version: u64,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtworkBlobMeta {
    pub sha256: String,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    pub encoded_bytes: u64,
}
impl ArtworkBlobMeta {
    pub fn validate(&self) -> Result<()> {
        digest(&self.sha256)?;
        ensure!(
            matches!(
                self.mime_type.as_str(),
                "image/jpeg" | "image/png" | "image/gif" | "image/webp"
            ),
            "Unsupported picture MIME type"
        );
        ensure!(
            self.width > 0
                && self.width <= 4096
                && self.height > 0
                && self.height <= 4096
                && self.width as u64 * self.height as u64 <= 16 * 1024 * 1024,
            "Picture dimensions exceed their limit"
        );
        ensure!(
            (1..=64).contains(&self.frames),
            "Picture frame count exceeds its limit"
        );
        ensure!(
            self.encoded_bytes > 0 && self.encoded_bytes <= MAX_ARTWORK_BYTES as u64,
            "Picture encoded bytes exceed their limit"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtworkInfo {
    pub key: ArtworkKey,
    pub provenance: ArtworkProvenance,
    pub url: Option<ArtworkUrl>,
    pub blob: Option<ArtworkBlobMeta>,
    pub resolved_at_ms: u64,
    pub expire_at_ms: u64,
}
impl ArtworkInfo {
    pub fn new(
        key: ArtworkKey,
        provenance: ArtworkProvenance,
        url: Option<ArtworkUrl>,
        blob: Option<ArtworkBlobMeta>,
        now_ms: u64,
    ) -> Result<Self> {
        let ttl = if matches!(provenance, ArtworkProvenance::Embedded { .. }) {
            LOCAL_ARTWORK_TTL_MS
        } else {
            REMOTE_ARTWORK_TTL_MS
        };
        let value = Self {
            key,
            provenance,
            url,
            blob,
            resolved_at_ms: now_ms,
            expire_at_ms: now_ms
                .checked_add(ttl)
                .ok_or_else(|| anyhow::anyhow!("Picture expiry overflow"))?,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        self.key.validate()?;
        ensure!(
            self.resolved_at_ms <= i64::MAX as u64
                && self.expire_at_ms > self.resolved_at_ms
                && self.expire_at_ms <= i64::MAX as u64
                && self.expire_at_ms - self.resolved_at_ms <= LOCAL_ARTWORK_TTL_MS,
            "Invalid picture cache time window"
        );
        if let Some(url) = &self.url {
            ArtworkUrl::new(url.as_str())?;
        }
        if let Some(blob) = &self.blob {
            blob.validate()?;
        }
        ensure!(
            self.url.is_some() || self.blob.is_some(),
            "Picture information has no URL or verified blob"
        );
        match &self.provenance {
            ArtworkProvenance::Embedded {
                asset_id,
                permission_sha256,
            } => {
                text(asset_id, 1024)?;
                digest(permission_sha256)?;
                ensure!(
                    self.url.is_none() && self.blob.is_some(),
                    "Embedded artwork requires verified bytes"
                );
            }
            ArtworkProvenance::Metadata { platform } | ArtworkProvenance::Provider { platform } => {
                ensure!(
                    matches!(platform.as_str(), "kw" | "kg" | "tx" | "wy" | "mg"),
                    "Unsupported native artwork platform"
                );
                ensure!(
                    self.url.is_some(),
                    "Native artwork requires its URL provenance"
                );
            }
            ArtworkProvenance::Source {
                source_sha256,
                policy_version,
            } => {
                digest(source_sha256)?;
                ensure!(*policy_version > 0, "Invalid artwork source policy");
                ensure!(self.url.is_some(), "Source artwork requires URL provenance");
            }
        }
        match (&self.key.authority, &self.provenance) {
            (
                ArtworkAuthority::Native,
                ArtworkProvenance::Metadata { platform } | ArtworkProvenance::Provider { platform },
            ) => ensure!(
                platform == &self.key.original_platform || self.key.matched_record_sha256.is_some(),
                "Artwork native provenance does not match its request"
            ),
            (
                ArtworkAuthority::LocalAsset {
                    asset_id: a,
                    permission_sha256: p,
                },
                ArtworkProvenance::Embedded {
                    asset_id: b,
                    permission_sha256: q,
                },
            ) => ensure!(a == b && p == q, "Artwork local permission changed"),
            (
                ArtworkAuthority::Source {
                    source_sha256: a,
                    policy_version: p,
                    ..
                },
                ArtworkProvenance::Source {
                    source_sha256: b,
                    policy_version: q,
                },
            ) => ensure!(
                a == b && p == q,
                "Artwork source provenance does not match its authority"
            ),
            (
                ArtworkAuthority::Source {
                    local_permission_sha256: Some(a),
                    ..
                },
                ArtworkProvenance::Embedded {
                    permission_sha256: b,
                    ..
                },
            ) => ensure!(a == b, "Artwork fallback permission changed"),
            _ => anyhow::bail!("Artwork provenance does not match its authority"),
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum ArtworkLookup {
    Hit(Box<ArtworkInfo>),
    Miss,
    Expired,
    ClockRollback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PictureState {
    Resolving,
    AwaitingApproval,
    Fetching,
    Ready,
    Failed,
    Cancelled,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PictureOperationView {
    pub operation_id: String,
    pub original_track_id: String,
    pub state: PictureState,
    pub info: Option<ArtworkInfo>,
    pub cached: bool,
    pub error: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PictureBytePage {
    pub sha256: String,
    pub offset: u64,
    pub total: u64,
    pub bytes: Vec<u8>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageCacheStats {
    pub info_entries: usize,
    pub info_bytes: u64,
    pub blob_entries: usize,
    pub blob_bytes: u64,
    pub database_bytes: Option<u64>,
    pub wal_bytes: Option<u64>,
    pub shared_memory_bytes: Option<u64>,
    pub physical_observed_at_ms: Option<u64>,
}

pub fn local_artwork_permission(asset: &LocalAsset) -> Result<String> {
    ensure!(asset.available, "Local artwork asset is unavailable");
    document_fingerprint(&(
        asset.id.as_str(),
        &asset.path,
        asset.size_bytes,
        asset.modified_ms,
        &asset.fingerprint,
    ))
}
pub fn document_fingerprint(value: &impl Serialize) -> Result<String> {
    let mut value = serde_json::to_value(value)?;
    fn canonical(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(object) => {
                for value in object.values_mut() {
                    canonical(value);
                }
                let mut entries: Vec<_> = std::mem::take(object).into_iter().collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                object.extend(entries);
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    canonical(value);
                }
            }
            _ => (),
        }
    }
    canonical(&mut value);
    let bytes = serde_json::to_vec(&value)?;
    ensure!(
        bytes.len() <= MAX_ARTWORK_INFO_BYTES,
        "Artwork identity exceeds its byte limit"
    );
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
pub(crate) fn digest(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|ch| ch.is_ascii_digit() || (b'a'..=b'f').contains(&ch)),
        "Artwork identity requires a lowercase SHA-256 digest"
    );
    Ok(())
}
fn text(value: &str, limit: usize) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= limit && !value.chars().any(char::is_control),
        "Invalid artwork identity text"
    );
    Ok(())
}
