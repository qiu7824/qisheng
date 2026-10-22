use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LxMusicDtoVersion {
    Legacy,
    Modern,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PlatformTrackRef {
    /// Unknown/closed source identifiers are preserved for import compatibility.
    pub source: String,
    /// JSON keeps numeric platform IDs distinct from string platform IDs.
    pub song_id: Value,
    pub qualities: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LxWireData {
    pub version: LxMusicDtoVersion,
    pub original: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TrackRecord {
    pub id: String,
    pub lx_id: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album: String,
    pub duration_ms: Option<u64>,
    pub platform: PlatformTrackRef,
    pub local_asset_id: Option<String>,
    /// Normalized LX meta, including all platform-specific and unknown fields.
    pub metadata: Map<String, Value>,
    /// Retains absent/null fields and legacy quality names for original-format export.
    pub wire: Option<LxWireData>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalAsset {
    pub id: String,
    pub track_id: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub modified_ms: Option<u64>,
    pub fingerprint: Option<String>,
    pub extension: String,
    pub available: bool,
}

/// Pure lexical normalization; does not read files, resolve links, or grant access.
pub fn normalize_local_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    let windows_path = path.to_str().map(|text| {
        let text = text.replace('/', "\\");
        if let Some(unc) = text.strip_prefix("\\\\?\\UNC\\") {
            PathBuf::from(format!("\\\\{unc}"))
        } else if let Some(ordinary) = text.strip_prefix("\\\\?\\") {
            PathBuf::from(ordinary)
        } else {
            PathBuf::from(text)
        }
    });
    #[cfg(windows)]
    let path = windows_path.as_deref().unwrap_or(path);
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    normalized.pop();
                } else if !normalized.has_root() {
                    normalized.push(component.as_os_str());
                }
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

pub fn stable_id(namespace: &str, identity: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update((namespace.len() as u64).to_le_bytes());
    digest.update(namespace.as_bytes());
    digest.update(identity);
    format!("{namespace}:{:x}", digest.finalize())
}

/// The library supplies a canonical authorized path. This helper performs no I/O.
pub fn local_track_id(path: &Path) -> String {
    let normalized = normalize_local_path(path);
    stable_id("local", &local_identity_bytes(&normalized)).replacen("local:", "local-", 1)
}

fn local_identity_bytes(path: &Path) -> Vec<u8> {
    // Windows's default file semantics are case-insensitive. A case-sensitive
    // directory requires an identity policy override in the indexing service.
    #[cfg(windows)]
    if let Some(text) = path.to_str() {
        return text.to_lowercase().into_bytes();
    }
    path.as_os_str().as_encoded_bytes().to_vec()
}

impl LocalAsset {
    pub fn new(track_id: impl Into<String>, path: PathBuf, size_bytes: u64) -> Self {
        let path = normalize_local_path(&path);
        // OS encoding avoids collisions caused by lossy UTF-8 conversion.
        let id = stable_id("asset", &local_identity_bytes(&path));
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_lowercase();
        Self {
            id,
            track_id: track_id.into(),
            path,
            size_bytes,
            modified_ms: None,
            fingerprint: None,
            extension,
            available: true,
        }
    }
}

impl TrackRecord {
    pub fn from_lx_dto(value: Value, version: LxMusicDtoVersion) -> Result<Self> {
        let object = value.as_object().context("Music DTO must be an object")?;
        let source = string(object, "source")?;
        ensure!(!source.is_empty(), "Music source must not be empty");
        let title = string(object, "name")?;
        let singer = match object.get("singer") {
            Some(Value::String(value)) => value.clone(),
            Some(Value::Null) | None => String::new(),
            _ => anyhow::bail!("Music singer must be a string or null"),
        };
        let mut metadata = match version {
            LxMusicDtoVersion::Modern => object
                .get("meta")
                .and_then(Value::as_object)
                .context("Modern music meta must be an object")?
                .clone(),
            LxMusicDtoVersion::Legacy => legacy_metadata(object, &source),
        };
        let song_id = metadata
            .get("songId")
            .cloned()
            .context("Music song ID is missing")?;
        ensure!(
            song_id.is_string() || song_id.is_number(),
            "Music song ID must be a string or number"
        );
        let lx_id = match version {
            LxMusicDtoVersion::Modern => string(object, "id")?,
            LxMusicDtoVersion::Legacy if source == "kg" => format!(
                "{}_{}",
                id_text(&song_id),
                metadata.get("hash").map(id_text).unwrap_or_default()
            ),
            LxMusicDtoVersion::Legacy => format!("{source}_{}", id_text(&song_id)),
        };
        ensure!(!lx_id.is_empty(), "LX music ID must not be empty");
        migrate_qualitys(&mut metadata);
        let qualities = metadata
            .get("qualitys")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|quality| quality.get("type").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        let album = metadata
            .get("albumName")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let duration_ms = object
            .get("interval")
            .and_then(Value::as_str)
            .and_then(parse_interval);
        // Include platform-specific hash in LX identity (not merely song ID).
        let id = if source == "local" {
            let path = metadata
                .get("filePath")
                .and_then(Value::as_str)
                .context("Local music filePath must be a string")?;
            local_track_id(Path::new(path))
        } else {
            stable_id(
                "track",
                serde_json::to_string(&json!([source, song_id, lx_id]))?.as_bytes(),
            )
        };
        let local_asset_id = if source == "local" {
            metadata
                .get("filePath")
                .and_then(Value::as_str)
                .map(|path| LocalAsset::new(id.clone(), path.into(), 0).id)
        } else {
            None
        };
        Ok(Self {
            id,
            lx_id,
            title,
            artists: vec![singer],
            album,
            duration_ms,
            platform: PlatformTrackRef {
                source,
                song_id,
                qualities,
            },
            local_asset_id,
            metadata,
            wire: Some(LxWireData {
                version,
                original: value,
            }),
        })
    }

    /// Original-format export retains unknown fields and absent/null distinctions.
    /// Cross-format export uses LX's field mappings and keeps unknown extension fields.
    pub fn to_lx_dto(&self, version: LxMusicDtoVersion) -> Value {
        if let Some(wire) = &self.wire
            && wire.version == version
        {
            let mut value = wire.original.clone();
            if let Some(object) = value.as_object_mut() {
                object.insert("name".into(), self.title.clone().into());
                let singer = self.artists.join("、");
                // Preserve null/missing singer when the normalized value is empty.
                if !singer.is_empty() || object.get("singer").is_some_and(Value::is_string) {
                    object.insert("singer".into(), singer.into());
                }
                object.insert("source".into(), self.platform.source.clone().into());
                match version {
                    LxMusicDtoVersion::Legacy => {
                        object.insert("songmid".into(), self.platform.song_id.clone());
                        if self.platform.source == "local" {
                            for key in ["filePath", "ext"] {
                                if let Some(value) = self.metadata.get(key) {
                                    object.insert(key.into(), value.clone());
                                }
                            }
                        }
                        if object
                            .get("albumName")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            != self.album
                        {
                            object.insert("albumName".into(), self.album.clone().into());
                        }
                    }
                    LxMusicDtoVersion::Modern => {
                        object.insert("id".into(), self.lx_id.clone().into());
                        if let Some(meta) = object.get_mut("meta").and_then(Value::as_object_mut) {
                            meta.insert("songId".into(), self.platform.song_id.clone());
                            if self.platform.source == "local" {
                                for key in ["filePath", "ext"] {
                                    if let Some(value) = self.metadata.get(key) {
                                        meta.insert(key.into(), value.clone());
                                    }
                                }
                            }
                            if meta
                                .get("albumName")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                != self.album
                            {
                                meta.insert("albumName".into(), self.album.clone().into());
                            }
                        }
                    }
                }
                if object
                    .get("interval")
                    .and_then(Value::as_str)
                    .and_then(parse_interval)
                    != self.duration_ms
                {
                    object.insert(
                        "interval".into(),
                        self.duration_ms
                            .map(format_interval)
                            .map(Value::String)
                            .unwrap_or(Value::Null),
                    );
                }
            }
            return value;
        }
        self.project_current_dto(version, false)
    }

    /// The current list-item metadata passed to an LX resolver, independent of
    /// archival wire values retained for original-format export.
    pub fn to_source_music_info(&self) -> Value {
        self.project_current_dto(LxMusicDtoVersion::Legacy, true)
    }

    fn project_current_dto(&self, version: LxMusicDtoVersion, source_payload: bool) -> Value {
        let mut object = Map::new();
        if let Some(wire) = &self.wire
            && let Some(original) = wire.original.as_object()
            && (!source_payload || wire.version == LxMusicDtoVersion::Modern)
        {
            for (key, value) in original {
                if !is_known_wire_field(key) {
                    object.insert(key.clone(), value.clone());
                }
            }
        }
        object.insert("name".into(), self.title.clone().into());
        object.insert("singer".into(), self.artists.join("、").into());
        object.insert("source".into(), self.platform.source.clone().into());
        let interval = self
            .wire
            .as_ref()
            .and_then(|wire| wire.original.get("interval"))
            .filter(|value| value.as_str().and_then(parse_interval) == self.duration_ms)
            .cloned()
            .unwrap_or_else(|| {
                self.duration_ms
                    .map(format_interval)
                    .map(Value::String)
                    .unwrap_or(Value::Null)
            });
        object.insert("interval".into(), interval);
        let mut metadata = self.metadata.clone();
        if source_payload {
            migrate_qualitys(&mut metadata);
        }
        metadata.insert("songId".into(), self.platform.song_id.clone());
        metadata.insert("albumName".into(), self.album.clone().into());
        match version {
            LxMusicDtoVersion::Modern => {
                object.insert("id".into(), self.lx_id.clone().into());
                object.insert("meta".into(), Value::Object(metadata));
            }
            LxMusicDtoVersion::Legacy => {
                for (key, value) in metadata {
                    let key = match key.as_str() {
                        "songId" => "songmid",
                        "picUrl" => "img",
                        "qualitys" => "types",
                        "_qualitys" => "_types",
                        "id" if self.platform.source == "tx" => "songId",
                        _ => &key,
                    }
                    .to_owned();
                    object.insert(key, value);
                }
                object.entry("img").or_insert(json!(""));
                object.entry("typeUrl").or_insert(json!({}));
                if self.platform.source == "local" {
                    object.entry("albumId").or_insert(json!(""));
                    object.entry("types").or_insert(json!([]));
                    object.entry("_types").or_insert(json!({}));
                }
            }
        }
        Value::Object(object)
    }
}

fn string(object: &Map<String, Value>, key: &str) -> Result<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("Music {key} must be a string"))
}

fn id_text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn is_known_wire_field(key: &str) -> bool {
    matches!(
        key,
        "id" | "name"
            | "singer"
            | "source"
            | "interval"
            | "meta"
            | "songmid"
            | "albumName"
            | "img"
            | "typeUrl"
            | "types"
            | "_types"
            | "albumId"
            | "filePath"
            | "ext"
            | "hash"
            | "albumAudioId"
            | "strMediaMid"
            | "songId"
            | "albumMid"
            | "copyrightId"
            | "lrcUrl"
            | "mrcUrl"
            | "trcUrl"
    )
}

fn legacy_metadata(object: &Map<String, Value>, source: &str) -> Map<String, Value> {
    let mut meta = Map::new();
    for (key, value) in object {
        if !is_known_wire_field(key) {
            meta.insert(key.clone(), value.clone());
        }
    }
    for (old, new) in [
        ("songmid", "songId"),
        ("albumName", "albumName"),
        ("img", "picUrl"),
        ("types", "qualitys"),
        ("_types", "_qualitys"),
        ("albumId", "albumId"),
        ("filePath", "filePath"),
        ("ext", "ext"),
        ("hash", "hash"),
        ("albumAudioId", "albumAudioId"),
        ("strMediaMid", "strMediaMid"),
        ("albumMid", "albumMid"),
        ("copyrightId", "copyrightId"),
        ("lrcUrl", "lrcUrl"),
        ("mrcUrl", "mrcUrl"),
        ("trcUrl", "trcUrl"),
    ] {
        if let Some(value) = object.get(old) {
            meta.insert(new.into(), value.clone());
        }
    }
    if source == "tx"
        && let Some(value) = object.get("songId")
    {
        meta.insert("id".into(), value.clone());
    }
    if source == "local" {
        let path = meta
            .get("filePath")
            .filter(|v| !v.is_null())
            .cloned()
            .or_else(|| meta.get("songId").cloned())
            .unwrap_or(json!(""));
        let ext = meta
            .get("ext")
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or_else(|| {
                json!(
                    Path::new(path.as_str().unwrap_or_default())
                        .extension()
                        .and_then(|v| v.to_str())
                        .unwrap_or_default()
                )
            });
        meta.insert("filePath".into(), path);
        meta.insert("ext".into(), ext);
    }
    meta
}

fn migrate_qualitys(metadata: &mut Map<String, Value>) {
    let migrate = metadata
        .get_mut("_qualitys")
        .and_then(Value::as_object_mut)
        .is_some_and(|qualities| {
            if qualities.get("flac32bit").is_some_and(|v| !v.is_null())
                && !qualities.contains_key("flac24bit")
            {
                let old = qualities.remove("flac32bit").unwrap();
                qualities.insert("flac24bit".into(), old);
                true
            } else {
                false
            }
        });
    if migrate && let Some(qualities) = metadata.get_mut("qualitys").and_then(Value::as_array_mut) {
        for quality in qualities {
            if quality.get("type").and_then(Value::as_str) == Some("flac32bit") {
                quality["type"] = json!("flac24bit");
            }
        }
    }
}

fn parse_interval(value: &str) -> Option<u64> {
    let parts: Vec<_> = value.split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut seconds = 0u64;
    for part in parts {
        let number: u64 = part.parse().ok()?;
        seconds = seconds.checked_mul(60)?.checked_add(number)?;
    }
    seconds.checked_mul(1000)
}

fn format_interval(milliseconds: u64) -> String {
    let seconds = milliseconds / 1000;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}
