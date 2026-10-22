use super::{
    BackupEncoding, codec,
    settings::{LxSettings, SettingsPreview},
};
use crate::domain::{
    lists::{DEFAULT_LIST, LOVE_LIST, ListData, ListInfo, TEMP_LIST, UserList},
    music::{LxMusicDtoVersion, TrackRecord},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Map, Value};
use std::collections::BTreeSet;

pub const MAX_BACKUP_TRACKS: usize = 100_000;
pub const MAX_IMPORT_TRACKS: usize = 50_000;
pub const MAX_BACKUP_LISTS: usize = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LxBackupKind {
    AllData,
    Settings,
    PlayList,
    PlayListPart,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LxList {
    pub info: ListInfo,
    pub tracks: Vec<TrackRecord>,
    /// Original list fields, including absent/null distinctions and extensions.
    pub wire: Map<String, Value>,
}

impl LxList {
    pub fn from_domain(info: ListInfo, tracks: Vec<TrackRecord>) -> Result<Self> {
        let list = Self {
            info,
            tracks,
            wire: Map::new(),
        };
        list.validate()?;
        Ok(list)
    }

    fn parse(value: &Value, version: LxMusicDtoVersion) -> Result<Self> {
        let mut wire = value.as_object().context("List must be an object")?.clone();
        let id = text(&wire, "id")?;
        let name = text(&wire, "name")?;
        let info = ListInfo {
            id,
            name,
            source: optional_text(&wire, "source")?,
            source_list_id: optional_text(&wire, "sourceListId")?,
            location_update_time: match wire.get("locationUpdateTime") {
                None | Some(Value::Null) => None,
                Some(value) => Some(
                    value
                        .as_u64()
                        .context("List update time must be a nonnegative integer or null")?,
                ),
            },
            meta: wire.get("meta").cloned(),
        };
        let raw_tracks = wire
            .get("list")
            .and_then(Value::as_array)
            .context("List songs must be an array")?;
        ensure!(
            raw_tracks.len() <= MAX_BACKUP_TRACKS,
            "List has too many songs"
        );
        let tracks = raw_tracks
            .iter()
            .map(|raw| TrackRecord::from_lx_dto(raw.clone(), version))
            .collect::<Result<Vec<_>>>()?;
        wire.remove("list");
        let list = Self { info, tracks, wire };
        list.validate()?;
        Ok(list)
    }

    pub fn to_value(&self) -> Result<Value> {
        self.validate()?;
        let mut wire = self.wire.clone();
        wire.insert("id".into(), self.info.id.clone().into());
        wire.insert("name".into(), self.info.name.clone().into());
        wire.insert(
            "list".into(),
            self.tracks
                .iter()
                .map(|track| track.to_lx_dto(LxMusicDtoVersion::Modern))
                .collect(),
        );
        for (key, field) in [
            ("source", &self.info.source),
            ("sourceListId", &self.info.source_list_id),
        ] {
            if let Some(value) = field {
                wire.insert(key.into(), value.clone().into());
            } else if wire.get(key).is_some_and(|value| !value.is_null()) {
                wire.remove(key);
            }
        }
        if let Some(value) = self.info.location_update_time {
            wire.insert("locationUpdateTime".into(), value.into());
        } else if wire.contains_key("locationUpdateTime")
            || self.wire.is_empty() && !matches!(self.info.id.as_str(), DEFAULT_LIST | LOVE_LIST)
        {
            wire.insert("locationUpdateTime".into(), Value::Null);
        }
        if let Some(value) = &self.info.meta {
            wire.insert("meta".into(), value.clone());
        }
        Ok(Value::Object(wire))
    }

    fn validate(&self) -> Result<()> {
        validate_info(&self.info)?;
        validate_tracks(&self.tracks)?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LxBackup {
    pub kind: LxBackupKind,
    pub settings: Option<LxSettings>,
    pub lists: Vec<LxList>,
    pub extensions: Map<String, Value>,
    pub migrated_legacy: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConflictPolicy {
    ReplaceTracks,
    KeepExisting,
    /// A fresh repository-allocated ID must be supplied explicitly for a single list.
    Copy {
        new_list_id: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListImportDisposition {
    Added,
    ReplacedTracks,
    KeptExisting,
    Copied,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListImportChange {
    pub imported_id: String,
    pub destination_id: String,
    pub disposition: ListImportDisposition,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalReference {
    pub track_id: String,
    pub declared_path: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImportedListMetadata {
    pub info: ListInfo,
    pub wire: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImportPlan {
    pub lists: ListData,
    pub list_changes: Vec<ListImportChange>,
    /// Includes built-in list location/extension fields that ListData cannot
    /// represent. The transaction owner archives these without granting access.
    pub archived_list_metadata: Vec<ImportedListMetadata>,
    pub settings: Option<SettingsPreview>,
    /// Metadata only: the repository must resolve bindings and obtain fresh file
    /// authority before treating these references as available local media.
    pub local_references: Vec<LocalReference>,
}

impl LxBackup {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.lists.len() <= MAX_BACKUP_LISTS,
            "Backup has too many lists"
        );
        ensure!(
            self.lists
                .iter()
                .map(|list| list.tracks.len())
                .sum::<usize>()
                <= MAX_BACKUP_TRACKS,
            "Backup has too many songs"
        );
        let mut ids = BTreeSet::new();
        for list in &self.lists {
            list.validate()?;
            ensure!(ids.insert(&list.info.id), "Backup repeats a list identity");
        }
        match self.kind {
            LxBackupKind::Settings => ensure!(
                self.settings.is_some() && self.lists.is_empty(),
                "Settings backup shape is invalid"
            ),
            LxBackupKind::AllData => {
                ensure!(self.settings.is_some(), "All-data backup has no settings")
            }
            LxBackupKind::PlayList => {
                ensure!(self.settings.is_none(), "Playlist backup contains settings")
            }
            LxBackupKind::PlayListPart => ensure!(
                self.settings.is_none() && self.lists.len() == 1,
                "Single-list backup shape is invalid"
            ),
        }
        if let Some(settings) = &self.settings {
            settings.preview()?;
        }
        Ok(())
    }

    pub fn to_value(&self) -> Result<Value> {
        self.validate()?;
        let mut object = self.extensions.clone();
        let lists = self
            .lists
            .iter()
            .map(LxList::to_value)
            .collect::<Result<Vec<_>>>()?;
        let (kind, list_key) = match self.kind {
            LxBackupKind::AllData => ("allData_v2", "playList"),
            LxBackupKind::Settings => ("setting_v2", "data"),
            LxBackupKind::PlayList => ("playList_v2", "data"),
            LxBackupKind::PlayListPart => ("playListPart_v2", "data"),
        };
        object.insert("type".into(), kind.into());
        if let Some(settings) = &self.settings {
            object.insert(
                if self.kind == LxBackupKind::AllData {
                    "setting"
                } else {
                    "data"
                }
                .into(),
                Value::Object(settings.values.clone()),
            );
        }
        if self.kind != LxBackupKind::Settings {
            object.insert(
                list_key.into(),
                if self.kind == LxBackupKind::PlayListPart {
                    lists.into_iter().next().context("Single list is missing")?
                } else {
                    lists.into()
                },
            );
        }
        Ok(Value::Object(object))
    }

    pub fn preview(&self, current: &ListData, policy: &ConflictPolicy) -> Result<ImportPlan> {
        self.validate()?;
        validate_domain(current)?;
        ensure!(
            !matches!(policy, ConflictPolicy::Copy { .. })
                || self.kind == LxBackupKind::PlayListPart,
            "Copy policy requires a single-list backup"
        );
        let mut planned_count = all_tracks(current).count();
        for incoming in &self.lists {
            let previous = match incoming.info.id.as_str() {
                DEFAULT_LIST => Some(current.default_list.len()),
                LOVE_LIST => Some(current.love_list.len()),
                _ => current
                    .user_list
                    .iter()
                    .find(|list| list.info.id == incoming.info.id)
                    .map(|list| list.tracks.len()),
            };
            if matches!(policy, ConflictPolicy::Copy { .. }) || previous.is_none() {
                planned_count = planned_count.saturating_add(incoming.tracks.len());
            } else if *policy == ConflictPolicy::ReplaceTracks {
                planned_count = planned_count
                    .saturating_sub(previous.unwrap_or(0))
                    .saturating_add(incoming.tracks.len());
            }
        }
        ensure!(
            planned_count <= MAX_IMPORT_TRACKS,
            "Import exceeds the 50000-song transaction budget; select a smaller scope"
        );
        let mut lists = current.clone();
        let mut list_changes = Vec::new();
        for incoming in &self.lists {
            let imported_id = incoming.info.id.clone();
            if let ConflictPolicy::Copy { new_list_id } = policy {
                let mut info = incoming.info.clone();
                info.id = new_list_id.clone();
                validate_info(&info)?;
                ensure!(
                    !info.is_builtin()
                        && lists.user_list.iter().all(|list| list.info.id != info.id),
                    "Copy requires an unused user-list identity"
                );
                lists.user_list.push(UserList {
                    info,
                    tracks: incoming.tracks.clone(),
                });
                list_changes.push(ListImportChange {
                    imported_id,
                    destination_id: new_list_id.clone(),
                    disposition: ListImportDisposition::Copied,
                });
                continue;
            }
            let destination = match imported_id.as_str() {
                DEFAULT_LIST => Some(&mut lists.default_list),
                LOVE_LIST => Some(&mut lists.love_list),
                _ => lists
                    .user_list
                    .iter_mut()
                    .find(|list| list.info.id == imported_id)
                    .map(|list| &mut list.tracks),
            };
            let disposition = if let Some(tracks) = destination {
                if *policy == ConflictPolicy::KeepExisting {
                    ListImportDisposition::KeptExisting
                } else {
                    *tracks = incoming.tracks.clone();
                    ListImportDisposition::ReplacedTracks
                }
            } else {
                lists.user_list.push(UserList {
                    info: incoming.info.clone(),
                    tracks: incoming.tracks.clone(),
                });
                ListImportDisposition::Added
            };
            list_changes.push(ListImportChange {
                imported_id: imported_id.clone(),
                destination_id: imported_id,
                disposition,
            });
        }
        validate_domain(&lists)?;
        let mut local_references = Vec::new();
        let mut seen = BTreeSet::new();
        for track in all_tracks(&lists) {
            if track.platform.source == "local" && seen.insert(track.id.clone()) {
                local_references.push(LocalReference {
                    track_id: track.id.clone(),
                    declared_path: track
                        .metadata
                        .get("filePath")
                        .and_then(Value::as_str)
                        .context("Local path is missing")?
                        .into(),
                });
            }
        }
        Ok(ImportPlan {
            lists,
            list_changes,
            archived_list_metadata: self
                .lists
                .iter()
                .map(|list| ImportedListMetadata {
                    info: list.info.clone(),
                    wire: list.wire.clone(),
                })
                .collect(),
            settings: self
                .settings
                .as_ref()
                .map(LxSettings::preview)
                .transpose()?,
            local_references,
        })
    }
}

pub fn decode_lx(bytes: &[u8], encoding: BackupEncoding) -> Result<LxBackup> {
    let value = codec::decode(bytes, encoding)?;
    let object = value.as_object().context("Backup must be an object")?;
    let raw_kind = object
        .get("type")
        .and_then(Value::as_str)
        .context("Backup type is missing")?;
    let legacy = matches!(raw_kind, "allData" | "setting" | "playList" | "defautlList");
    let version = if legacy {
        LxMusicDtoVersion::Legacy
    } else {
        LxMusicDtoVersion::Modern
    };
    let mut extensions = object.clone();
    extensions.remove("type");
    let mut settings = None;
    let lists;
    let kind = match raw_kind {
        "allData_v2" | "allData" => {
            settings = Some(LxSettings::from_value(
                object
                    .get("setting")
                    .context("All-data settings are missing")?,
                legacy,
            )?);
            extensions.remove("setting");
            if legacy && object.contains_key("defaultList") {
                let mut old = object
                    .get("defaultList")
                    .and_then(Value::as_object)
                    .context("Legacy default list is invalid")?
                    .clone();
                old.insert("id".into(), DEFAULT_LIST.into());
                old.entry("name").or_insert("list__name_default".into());
                lists = vec![LxList::parse(&Value::Object(old), version)?];
                extensions.remove("defaultList");
            } else {
                lists = parse_lists(
                    object
                        .get("playList")
                        .context("All-data lists are missing")?,
                    version,
                )?;
                extensions.remove("playList");
            }
            LxBackupKind::AllData
        }
        "setting_v2" | "setting" => {
            settings = Some(LxSettings::from_value(
                object.get("data").context("Settings data is missing")?,
                legacy,
            )?);
            lists = Vec::new();
            extensions.remove("data");
            LxBackupKind::Settings
        }
        "playList_v2" | "playList" => {
            lists = parse_lists(object.get("data").context("List data is missing")?, version)?;
            extensions.remove("data");
            LxBackupKind::PlayList
        }
        "playListPart_v2" => {
            lists = vec![LxList::parse(
                object.get("data").context("Single list is missing")?,
                version,
            )?];
            extensions.remove("data");
            LxBackupKind::PlayListPart
        }
        "defautlList" => {
            let mut old = object
                .get("data")
                .and_then(Value::as_object)
                .context("Legacy default list is invalid")?
                .clone();
            old.insert("id".into(), DEFAULT_LIST.into());
            old.entry("name").or_insert("list__name_default".into());
            lists = vec![LxList::parse(&Value::Object(old), version)?];
            extensions.remove("data");
            LxBackupKind::PlayListPart
        }
        _ => anyhow::bail!("Unsupported LX backup type: {raw_kind}"),
    };
    let backup = LxBackup {
        kind,
        settings,
        lists,
        extensions,
        migrated_legacy: legacy,
    };
    backup.validate()?;
    Ok(backup)
}

pub fn encode_lx(backup: &LxBackup, encoding: BackupEncoding) -> Result<Vec<u8>> {
    codec::encode(&backup.to_value()?, encoding)
}

fn parse_lists(value: &Value, version: LxMusicDtoVersion) -> Result<Vec<LxList>> {
    let raw = value.as_array().context("Backup lists must be an array")?;
    ensure!(raw.len() <= MAX_BACKUP_LISTS, "Backup has too many lists");
    raw.iter()
        .map(|list| LxList::parse(list, version))
        .collect()
}
fn text(object: &Map<String, Value>, key: &str) -> Result<String> {
    Ok(object
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("List {key} must be a string"))?
        .into())
}
fn optional_text(object: &Map<String, Value>, key: &str) -> Result<Option<String>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        _ => anyhow::bail!("List {key} must be a string or null"),
    }
}
pub(super) fn validate_info(info: &ListInfo) -> Result<()> {
    ensure!(
        !info.id.is_empty()
            && info.id.len() <= 1024
            && !info.id.contains('\0')
            && info.id != TEMP_LIST,
        "List identity is invalid or reserved"
    );
    ensure!(
        info.name.len() <= 4096 && !info.name.contains('\0'),
        "List name is invalid"
    );
    for value in [&info.source, &info.source_list_id].into_iter().flatten() {
        ensure!(
            value.len() <= 4096 && !value.contains('\0'),
            "List source metadata is invalid"
        );
    }
    Ok(())
}
pub(super) fn validate_tracks(tracks: &[TrackRecord]) -> Result<()> {
    ensure!(tracks.len() <= MAX_BACKUP_TRACKS, "List has too many songs");
    let mut ids = BTreeSet::new();
    for track in tracks {
        ensure!(
            !track.id.is_empty()
                && track.id.len() <= 1024
                && !track.id.contains('\0')
                && !track.lx_id.is_empty()
                && track.lx_id.len() <= 1024
                && !track.lx_id.contains('\0'),
            "Track identity is invalid"
        );
        ensure!(ids.insert(&track.lx_id), "List repeats a music identity");
        ensure!(
            !track.title.is_empty()
                && track.title.len() <= 64 * 1024
                && !track.title.contains('\0'),
            "Track name is invalid"
        );
        // Reparse the public interchange fields rather than trusting internal JSON.
        TrackRecord::from_lx_dto(
            track.to_lx_dto(LxMusicDtoVersion::Modern),
            LxMusicDtoVersion::Modern,
        )?;
    }
    Ok(())
}
pub(super) fn all_tracks(data: &ListData) -> impl Iterator<Item = &TrackRecord> {
    data.default_list
        .iter()
        .chain(&data.love_list)
        .chain(data.user_list.iter().flat_map(|list| &list.tracks))
        .chain(data.temp_list.iter().flatten())
}
pub(super) fn validate_domain(data: &ListData) -> Result<()> {
    ensure!(
        data.user_list.len() + 2 <= MAX_BACKUP_LISTS,
        "Import has too many lists"
    );
    ensure!(
        all_tracks(data).count() <= MAX_IMPORT_TRACKS,
        "Import exceeds the 50000-song transaction budget; select a smaller scope"
    );
    validate_tracks(&data.default_list)?;
    validate_tracks(&data.love_list)?;
    if let Some(tracks) = &data.temp_list {
        validate_tracks(tracks)?;
    }
    let mut ids = BTreeSet::new();
    for list in &data.user_list {
        validate_info(&list.info)?;
        ensure!(
            !list.info.is_builtin() && ids.insert(&list.info.id),
            "User-list identity is repeated or reserved"
        );
        validate_tracks(&list.tracks)?;
    }
    codec::json_bytes(data)?;
    Ok(())
}
