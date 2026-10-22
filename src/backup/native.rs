//! Versioned project backup without executable or filesystem permissions.

use super::{
    BackupEncoding, codec,
    lx::{ConflictPolicy, ImportPlan, LxBackup, LxBackupKind, LxList, validate_tracks},
};
use crate::{
    domain::{
        lists::{DEFAULT_LIST, DislikeRules, LOVE_LIST, ListData, ListInfo},
        lyric::LyricBundle,
        music::TrackRecord,
    },
    download::{
        planning::DownloadQuality,
        task_model::{DownloadSettings, DownloadTask},
    },
    source::{ImportedSource, import_script},
    store::StoreData,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeSet;

const NATIVE_TYPE: &str = "lx_native_backup";
const NATIVE_VERSION: u32 = 1;
const MAX_NATIVE_SOURCES: usize = 64;
const MAX_NATIVE_DOWNLOADS: usize = 100;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NativeBackupOptions {
    pub include_sources: bool,
    pub include_downloads: bool,
    pub include_edited_lyrics: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeSourceArchive {
    pub script: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeEditedLyrics {
    pub track_id: String,
    pub bundle: LyricBundle,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeDownloadPlan {
    pub record: TrackRecord,
    pub requested_quality: DownloadQuality,
    pub list_id: Option<String>,
    pub list_name_snapshot: Option<String>,
    pub order: u32,
    pub settings: DownloadSettings,
}

impl NativeDownloadPlan {
    pub fn from_task(task: &DownloadTask) -> Result<Self> {
        task.validate()?;
        let mut settings = task.settings.clone();
        settings.enabled = false;
        settings.destination_grant_id = None;
        Ok(Self {
            record: task.original_record.clone(),
            requested_quality: task.requested_quality,
            list_id: task.list_id.clone(),
            list_name_snapshot: task.list_name_snapshot.clone(),
            order: task.order,
            settings,
        })
    }
    fn sanitize(&mut self) -> Result<()> {
        validate_tracks(std::slice::from_ref(&self.record))?;
        self.settings.validate()?;
        self.settings.enabled = false;
        self.settings.destination_grant_id = None;
        ensure!(
            self.list_id
                .as_ref()
                .is_none_or(|id| !id.is_empty() && id.len() <= 1024 && !id.contains('\0')),
            "Download list identity is invalid"
        );
        ensure!(
            self.list_name_snapshot
                .as_ref()
                .is_none_or(|name| name.len() <= 4096 && !name.contains('\0')),
            "Download list name is invalid"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeBackup {
    #[serde(rename = "type")]
    pub format: String,
    pub version: u32,
    /// Contains player preferences only. Sources and local-file paths are removed;
    /// source bytes are separately archived without approval records.
    pub store: StoreData,
    pub lists: ListData,
    pub dislike_rules: String,
    /// Unverified path metadata, requiring a new local-file selection on restore.
    #[serde(default)]
    pub declared_local_paths: Vec<String>,
    #[serde(default)]
    pub sources: Vec<NativeSourceArchive>,
    #[serde(default)]
    pub downloads: Option<Vec<NativeDownloadPlan>>,
    #[serde(default)]
    pub edited_lyrics: Option<Vec<NativeEditedLyrics>>,
    /// Compatibility data is archived separately and never used as permissions.
    #[serde(default)]
    pub extensions: Map<String, Value>,
}

#[derive(Clone, Debug)]
pub struct NativeImportPlan {
    pub list_plan: ImportPlan,
    pub store_preferences: StoreData,
    pub unapproved_sources: Vec<ImportedSource>,
    pub declared_local_paths: Vec<String>,
    pub dislike: DislikeRules,
    /// Plans require fresh destination selection, source approval and URL consent;
    /// the importer cannot construct or start a runnable task from these records.
    pub download_plans: Option<Vec<NativeDownloadPlan>>,
    pub edited_lyrics: Option<Vec<NativeEditedLyrics>>,
    pub extensions: Map<String, Value>,
}

impl NativeBackup {
    pub fn from_snapshot(
        store: &StoreData,
        lists: &ListData,
        dislike_rules: &str,
        downloads: &[DownloadTask],
        edited_lyrics: &[NativeEditedLyrics],
        options: NativeBackupOptions,
    ) -> Result<Self> {
        let mut preferences = store.clone();
        preferences.sources.clear();
        preferences.selected = None;
        preferences.local_files.clear();
        let sources = if options.include_sources {
            ensure!(
                store.sources.len() <= MAX_NATIVE_SOURCES,
                "Backup has too many sources"
            );
            store
                .sources
                .iter()
                .map(|source| {
                    source.verify_integrity()?;
                    Ok(NativeSourceArchive {
                        script: source.script.clone(),
                    })
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        let backup = Self {
            format: NATIVE_TYPE.into(),
            version: NATIVE_VERSION,
            store: preferences,
            lists: lists.clone(),
            dislike_rules: dislike_rules.into(),
            declared_local_paths: store
                .local_files
                .iter()
                .map(|path| {
                    path.to_str()
                        .map(str::to_owned)
                        .ok_or_else(|| anyhow::anyhow!("Local path is not valid UTF-8"))
                })
                .collect::<Result<Vec<_>>>()?,
            sources,
            downloads: if options.include_downloads {
                Some(
                    downloads
                        .iter()
                        .map(NativeDownloadPlan::from_task)
                        .collect::<Result<Vec<_>>>()?,
                )
            } else {
                None
            },
            edited_lyrics: options
                .include_edited_lyrics
                .then(|| edited_lyrics.to_vec()),
            extensions: Map::new(),
        };
        backup.validate()?;
        Ok(backup)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format == NATIVE_TYPE && self.version == NATIVE_VERSION,
            "Unsupported project backup format or version"
        );
        ensure!(
            self.store.sources.is_empty()
                && self.store.selected.is_none()
                && self.store.local_files.is_empty(),
            "Project settings cannot contain source or local-file authority records"
        );
        ensure!(
            self.store.volume.is_finite() && (0.0..=1.0).contains(&self.store.volume),
            "Player volume is invalid"
        );
        self.store.audio_preferences.validate()?;
        self.store.equalizer.validate()?;
        self.store.playback_processing.validate()?;
        self.store.spatial.validate()?;
        self.store.timeout_stop.validate()?;
        super::lx::validate_domain(&self.lists)?;
        ensure!(
            self.dislike_rules.len() <= 1024 * 1024 && !self.dislike_rules.contains('\0'),
            "Dislike rules are invalid"
        );
        ensure!(
            self.declared_local_paths.len() <= super::lx::MAX_IMPORT_TRACKS
                && self
                    .declared_local_paths
                    .iter()
                    .all(|path| !path.is_empty() && path.len() <= 32_000 && !path.contains('\0')),
            "Declared local paths are invalid"
        );
        ensure!(
            self.sources.len() <= MAX_NATIVE_SOURCES,
            "Backup has too many sources"
        );
        ensure!(
            self.sources
                .iter()
                .map(|source| source.script.len())
                .sum::<usize>()
                <= 8 * 1024 * 1024,
            "Backup source bytes exceed 8 MiB"
        );
        for source in &self.sources {
            ensure!(
                source.script.len() <= 1024 * 1024 && !source.script.contains('\0'),
                "Source archive is invalid"
            );
        }
        if let Some(downloads) = &self.downloads {
            ensure!(
                downloads.len() <= MAX_NATIVE_DOWNLOADS,
                "Backup has too many download plans"
            );
            let mut keys = BTreeSet::new();
            for download in downloads {
                let mut safe = download.clone();
                safe.sanitize()?;
                ensure!(
                    !download.settings.enabled && download.settings.destination_grant_id.is_none(),
                    "Backup download plan contains execution authority"
                );
                ensure!(
                    keys.insert((&download.record.lx_id, download.requested_quality)),
                    "Backup repeats a download plan"
                );
            }
        }
        if let Some(lyrics) = &self.edited_lyrics {
            ensure!(
                lyrics.len() <= super::lx::MAX_IMPORT_TRACKS,
                "Backup has too many lyric records"
            );
            let mut ids = BTreeSet::new();
            for lyric in lyrics {
                ensure!(
                    !lyric.track_id.is_empty()
                        && lyric.track_id.len() <= 1024
                        && !lyric.track_id.contains('\0')
                        && ids.insert(&lyric.track_id),
                    "Edited lyric identity is invalid or repeated"
                );
                lyric.bundle.validate()?;
            }
        }
        codec::json_bytes(self)?;
        Ok(())
    }

    pub fn preview(&self, current: &ListData, policy: &ConflictPolicy) -> Result<NativeImportPlan> {
        self.validate()?;
        ensure!(
            !matches!(policy, ConflictPolicy::Copy { .. }),
            "Project backup requires merge or keep-existing policy"
        );
        let mut lists = vec![
            LxList::from_domain(
                ListInfo::user(DEFAULT_LIST, "list__name_default"),
                self.lists.default_list.clone(),
            )?,
            LxList::from_domain(
                ListInfo::user(LOVE_LIST, "list__name_love"),
                self.lists.love_list.clone(),
            )?,
        ];
        for list in &self.lists.user_list {
            lists.push(LxList::from_domain(list.info.clone(), list.tracks.clone())?);
        }
        let backup = LxBackup {
            kind: LxBackupKind::PlayList,
            settings: None,
            lists,
            extensions: Map::new(),
            migrated_legacy: false,
        };
        let mut list_plan = backup.preview(current, policy)?;
        if let Some(temp) = &self.lists.temp_list {
            list_plan.lists.temp_list = Some(temp.clone());
        }
        super::lx::validate_domain(&list_plan.lists)?;
        list_plan.local_references.clear();
        let mut local_ids = BTreeSet::new();
        for track in super::lx::all_tracks(&list_plan.lists) {
            if track.platform.source == "local" && local_ids.insert(track.id.clone()) {
                list_plan.local_references.push(super::lx::LocalReference {
                    track_id: track.id.clone(),
                    declared_path: track
                        .metadata
                        .get("filePath")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow::anyhow!("Local path is missing"))?
                        .into(),
                });
            }
        }
        let unapproved_sources = self
            .sources
            .iter()
            .map(|source| import_script(&source.script))
            .collect::<Result<Vec<_>>>()?;
        Ok(NativeImportPlan {
            list_plan,
            store_preferences: self.store.clone(),
            unapproved_sources,
            declared_local_paths: self.declared_local_paths.clone(),
            dislike: DislikeRules::parse(&self.dislike_rules),
            download_plans: self.downloads.clone(),
            edited_lyrics: self.edited_lyrics.clone(),
            extensions: self.extensions.clone(),
        })
    }
}

impl NativeImportPlan {
    /// Existing machine permissions remain under the profile's authority owner;
    /// this projection changes only the validated player preference fields.
    pub fn project_store(&self, current: &StoreData) -> StoreData {
        let mut projected = current.clone();
        projected.volume = self.store_preferences.volume;
        projected.audio_preferences = self.store_preferences.audio_preferences.clone();
        projected.equalizer = self.store_preferences.equalizer;
        projected.playback_processing = self.store_preferences.playback_processing;
        projected.audio_analysis_enabled = self.store_preferences.audio_analysis_enabled;
        projected.spatial = self.store_preferences.spatial;
        if let Some(effects) = &mut projected.effects {
            effects.settings.spatial = projected.spatial;
        }
        projected.timeout_stop = self.store_preferences.timeout_stop;
        projected
    }
}

pub fn decode_native(bytes: &[u8], encoding: BackupEncoding) -> Result<NativeBackup> {
    let mut backup: NativeBackup = serde_json::from_value(codec::decode(bytes, encoding)?)?;
    // Uploaded records are data. Legacy or malicious approval flags cannot become
    // source, URL or filesystem capabilities in the restore projection.
    for path in &backup.store.local_files {
        backup.declared_local_paths.push(
            path.to_str()
                .ok_or_else(|| anyhow::anyhow!("Local path is not valid UTF-8"))?
                .into(),
        );
    }
    backup.store.sources.clear();
    backup.store.selected = None;
    backup.store.local_files.clear();
    if let Some(downloads) = &mut backup.downloads {
        for download in downloads {
            download.sanitize()?;
        }
    }
    backup.validate()?;
    Ok(backup)
}
pub fn encode_native(backup: &NativeBackup, encoding: BackupEncoding) -> Result<Vec<u8>> {
    backup.validate()?;
    codec::encode(backup, encoding)
}
