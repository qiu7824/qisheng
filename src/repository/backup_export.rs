use super::{
    Repository,
    backup::{StoredListMetadata, StoredSettings},
    decode_document, download, lists, schema,
};
use crate::{
    backup::{
        BackupEncoding, LxBackup, LxBackupKind, NativeBackup, NativeBackupOptions, lx::LxList,
        native::NativeEditedLyrics, settings::LxSettings,
    },
    domain::{
        lists::{DEFAULT_LIST, LOVE_LIST, ListData, ListInfo, TEMP_LIST, UserList},
        lyric::{LyricBundle, LyricField, LyricKind},
    },
    store::StoreData,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Map, Value};

pub(crate) enum LogicalBackupKind {
    Lx(LxBackupKind, Option<String>),
    Native(NativeBackupOptions),
    Csv(String),
}

impl Repository {
    pub(crate) fn encode_backup_snapshot(
        &self,
        expected_revision: u64,
        store: &StoreData,
        kind: &LogicalBackupKind,
        encoding: BackupEncoding,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<Vec<u8>> {
        check()?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        let tx = connection.transaction()?;
        let revision: i64 = tx.query_row(
            "SELECT revision FROM repository_state WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        let revision = u64::try_from(revision).context("Stored backup revision is negative")?;
        ensure!(
            revision == expected_revision,
            "Backup snapshot revision changed; retry the export"
        );
        check()?;
        let mut budget = CaptureBudget::default();
        if let LogicalBackupKind::Csv(id) = kind {
            ensure!(
                encoding == BackupEncoding::Json,
                "CSV export does not accept gzip encoding"
            );
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM music_lists WHERE id=?1)",
                [id],
                |row| row.get(0),
            )?;
            ensure!(exists, "Export list does not exist");
            let tracks = capture_tracks(&tx, id, &mut budget, check)?;
            check()?;
            tx.commit()?;
            drop(connection);
            let bytes = crate::backup::csv::export_music_csv(
                &tracks,
                ["歌曲".into(), "歌手".into(), "专辑".into()],
            )?;
            check()?;
            return Ok(bytes);
        }
        let data = match kind {
            LogicalBackupKind::Lx(LxBackupKind::Settings, _) => ListData::default(),
            LogicalBackupKind::Lx(LxBackupKind::PlayListPart, Some(id)) => {
                let info: String = tx
                    .query_row(
                        "SELECT document FROM music_lists WHERE id=?1",
                        [id],
                        |row| row.get(0),
                    )
                    .context("Export list does not exist")?;
                budget.add(info.len(), false)?;
                let info: ListInfo = decode_document(&info)?;
                let tracks = capture_tracks(&tx, id, &mut budget, check)?;
                match id.as_str() {
                    DEFAULT_LIST => ListData {
                        default_list: tracks,
                        ..ListData::default()
                    },
                    LOVE_LIST => ListData {
                        love_list: tracks,
                        ..ListData::default()
                    },
                    TEMP_LIST => anyhow::bail!("LX single-list backup excludes the temporary list"),
                    _ => ListData {
                        user_list: vec![UserList { info, tracks }],
                        ..ListData::default()
                    },
                }
            }
            _ => capture_lists(
                &tx,
                &mut budget,
                matches!(kind, LogicalBackupKind::Native(_)),
                check,
            )?,
        };
        let prepared = match kind {
            LogicalBackupKind::Lx(kind, selected) => {
                let mut all = vec![
                    LxList::from_domain(
                        ListInfo::user(DEFAULT_LIST, "list__name_default"),
                        data.default_list,
                    )?,
                    LxList::from_domain(
                        ListInfo::user(LOVE_LIST, "list__name_love"),
                        data.love_list,
                    )?,
                ];
                for list in data.user_list {
                    check()?;
                    all.push(LxList::from_domain(list.info, list.tracks)?);
                }
                let mut selected_lists = match kind {
                    LxBackupKind::Settings => Vec::new(),
                    LxBackupKind::PlayListPart => {
                        let id = selected
                            .as_ref()
                            .context("A single-list export requires list_id")?;
                        vec![
                            all.into_iter()
                                .find(|list| &list.info.id == id)
                                .context("Export list does not exist")?,
                        ]
                    }
                    _ => all,
                };
                for list in &mut selected_lists {
                    check()?;
                    let row:Option<(i64,String,i64)>=tx.query_row("SELECT revision,document,byte_size FROM backup_list_metadata WHERE list_id=?1",[&list.info.id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
                    if let Some((revision, document, bytes)) = row {
                        ensure!(
                            revision > 0 && bytes == document.len() as i64,
                            "List archive SQL identity is invalid"
                        );
                        budget.add(document.len(), false)?;
                        let archive: StoredListMetadata = decode_document(&document)?;
                        ensure!(
                            archive.info.id == list.info.id
                                && archive.wire.get("id").and_then(Value::as_str)
                                    == Some(list.info.id.as_str()),
                            "List archive identity differs"
                        );
                        list.wire = archive.wire;
                    }
                }
                let settings = matches!(kind, LxBackupKind::AllData | LxBackupKind::Settings)
                    .then(|| player_settings(&tx, store, &mut budget))
                    .transpose()?;
                CapturedBackup::Lx(LxBackup {
                    kind: *kind,
                    settings,
                    lists: selected_lists,
                    extensions: Map::new(),
                    migrated_legacy: false,
                })
            }
            LogicalBackupKind::Native(options) => {
                let rules: String = tx.query_row(
                    "SELECT rules FROM dislike_state WHERE singleton=1",
                    [],
                    |row| row.get(0),
                )?;
                budget.add(rules.len(), false)?;
                let downloads = if options.include_downloads {
                    let (count,bytes):(i64,i64)=tx.query_row("SELECT count(*),coalesce(sum(length(CAST(document AS BLOB))),0) FROM download_tasks",[],|row|Ok((row.get(0)?,row.get(1)?)))?;
                    ensure!(
                        count <= 100 && bytes >= 0,
                        "Download backup capture is invalid"
                    );
                    budget.add(usize::try_from(bytes)?, false)?;
                    download::tasks_at(&tx)?
                } else {
                    Vec::new()
                };
                let mut lyrics = Vec::new();
                if options.include_edited_lyrics {
                    let mut statement = tx.prepare("SELECT track_id FROM lyric_bundles WHERE origin='edited' ORDER BY track_id LIMIT 50001")?;
                    let ids = statement
                        .query_map([], |row| row.get::<_, String>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    ensure!(
                        ids.len() <= crate::backup::lx::MAX_IMPORT_TRACKS,
                        "Too many edited lyric records"
                    );
                    for track_id in ids {
                        check()?;
                        let mut bundle = LyricBundle::default();
                        for (kind, name) in [
                            (LyricKind::Lyric, "lyric"),
                            (LyricKind::Tlyric, "tlyric"),
                            (LyricKind::Rlyric, "rlyric"),
                            (LyricKind::Lxlyric, "lxlyric"),
                        ] {
                            let row: Option<(String, Option<String>)> = tx.query_row("SELECT state,text FROM lyric_fields WHERE track_id=?1 AND origin='edited' AND kind=?2", rusqlite::params![track_id,name], |row| Ok((row.get(0)?,row.get(1)?))).optional()?;
                            let (state, text) = row.context("Edited lyric channel is missing")?;
                            let field = match (state.as_str(), text) {
                                ("missing", None) => LyricField::Missing,
                                ("null", None) => LyricField::Null,
                                ("text", Some(text)) => {
                                    budget.add(text.len(), false)?;
                                    LyricField::Text(text)
                                }
                                _ => anyhow::bail!("Edited lyric channel is invalid"),
                            };
                            bundle.set_field(kind, field);
                        }
                        lyrics.push(NativeEditedLyrics { track_id, bundle });
                    }
                }
                CapturedBackup::Native {
                    data,
                    rules,
                    downloads,
                    lyrics,
                    options: *options,
                }
            }
            LogicalBackupKind::Csv(_) => unreachable!(),
        };
        check()?;
        tx.commit()?;
        drop(connection);
        check()?;
        let bytes = match prepared {
            CapturedBackup::Lx(backup) => crate::backup::encode_lx(&backup, encoding)?,
            CapturedBackup::Native {
                data,
                rules,
                downloads,
                lyrics,
                options,
            } => crate::backup::encode_native(
                &NativeBackup::from_snapshot(store, &data, &rules, &downloads, &lyrics, options)?,
                encoding,
            )?,
        };
        check()?;
        Ok(bytes)
    }
}

enum CapturedBackup {
    Lx(LxBackup),
    Native {
        data: ListData,
        rules: String,
        downloads: Vec<crate::download::task_model::DownloadTask>,
        lyrics: Vec<NativeEditedLyrics>,
        options: NativeBackupOptions,
    },
}

#[derive(Default)]
struct CaptureBudget {
    bytes: usize,
    tracks: usize,
}
impl CaptureBudget {
    fn add(&mut self, bytes: usize, track: bool) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .context("Backup capture byte count overflow")?;
        self.tracks += usize::from(track);
        ensure!(
            self.bytes <= crate::backup::MAX_BACKUP_BYTES
                && self.tracks <= crate::backup::lx::MAX_BACKUP_TRACKS,
            "Backup capture exceeds its resource budget"
        );
        Ok(())
    }
}
fn capture_tracks(
    connection: &Connection,
    id: &str,
    budget: &mut CaptureBudget,
    check: &dyn Fn() -> Result<()>,
) -> Result<Vec<crate::domain::music::TrackRecord>> {
    let mut statement = connection.prepare(
        "SELECT document FROM music_list_items WHERE list_id=?1 ORDER BY position,music_id",
    )?;
    let mut rows = statement.query([id])?;
    let mut tracks = Vec::new();
    while let Some(row) = rows.next()? {
        check()?;
        let document: String = row.get(0)?;
        budget.add(document.len(), true)?;
        tracks.push(decode_document(&document)?);
    }
    Ok(tracks)
}
fn capture_lists(
    connection: &Connection,
    budget: &mut CaptureBudget,
    temporary: bool,
    check: &dyn Fn() -> Result<()>,
) -> Result<ListData> {
    let (count,bytes):(i64,i64)=connection.query_row("SELECT count(*),coalesce(sum(length(CAST(document AS BLOB))),0) FROM music_lists WHERE kind='user'",[],|row|Ok((row.get(0)?,row.get(1)?)))?;
    ensure!(
        count <= crate::backup::lx::MAX_BACKUP_LISTS as i64 && bytes >= 0,
        "Too many backup lists"
    );
    budget.add(usize::try_from(bytes)?, false)?;
    let infos = lists::query_infos(connection, true)?;
    ensure!(
        infos.len() <= crate::backup::lx::MAX_BACKUP_LISTS,
        "Too many backup lists"
    );
    let mut user_list = Vec::new();
    for info in infos {
        check()?;
        user_list.push(UserList {
            tracks: capture_tracks(connection, &info.id, budget, check)?,
            info,
        });
    }
    Ok(ListData {
        default_list: capture_tracks(connection, DEFAULT_LIST, budget, check)?,
        love_list: capture_tracks(connection, LOVE_LIST, budget, check)?,
        temp_list: if temporary {
            Some(capture_tracks(connection, TEMP_LIST, budget, check)?)
        } else {
            None
        },
        user_list,
    })
}

fn player_settings(
    connection: &Connection,
    store: &StoreData,
    budget: &mut CaptureBudget,
) -> Result<LxSettings> {
    let row: Option<(i64, String, i64)> = connection
        .query_row(
            "SELECT revision,document,byte_size FROM compat_settings WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let mut values = match row {
        Some((revision, document, bytes)) => {
            ensure!(
                revision > 0 && bytes == document.len() as i64,
                "Settings archive SQL identity is invalid"
            );
            budget.add(document.len(), false)?;
            decode_document::<StoredSettings>(&document)?
                .checked_preview()?
                .archived
        }
        None => Map::new(),
    };
    for key in [
        "sourceApproval",
        "sourceApprovals",
        "urlApprovals",
        "localFileGrants",
        "downloadGrants",
        "sync.authKey",
        "sync.client.key",
        "openAPI.token",
        "openApi.token",
    ] {
        values.remove(key);
    }
    values.insert("version".into(), "2.1.0".into());
    values.insert("player.volume".into(), store.volume.into());
    values.insert("player.isMute".into(), store.audio_preferences.muted.into());
    values.insert(
        "player.waitPlayEndStop".into(),
        store.timeout_stop.wait_for_track_end.into(),
    );
    let minutes = store
        .timeout_stop
        .last_seconds
        .map(|seconds| seconds / 60.0)
        .filter(|minutes| minutes.fract() == 0.0 && (1.0..=1440.0).contains(minutes))
        .map(|minutes| (minutes as u64).to_string())
        .unwrap_or_default();
    values.insert("player.waitPlayEndStopTime".into(), minutes.into());
    values.insert(
        "player.soundEffect.panner.enable".into(),
        store.spatial.enabled.into(),
    );
    values.insert(
        "player.soundEffect.panner.soundR".into(),
        (store.spatial.radius * 10.0).into(),
    );
    values.insert(
        "player.soundEffect.panner.speed".into(),
        store.spatial.speed.into(),
    );
    values.insert(
        "player.isMediaDeviceRemovedStopPlay".into(),
        store.audio_preferences.pause_on_device_change.into(),
    );
    values.insert(
        "player.playbackRate".into(),
        store.playback_processing.playback_rate.into(),
    );
    values.insert(
        "player.preservesPitch".into(),
        store.playback_processing.preserves_pitch.into(),
    );
    values.insert(
        "player.soundEffect.pitchShifter.playbackRate".into(),
        store.playback_processing.pitch_ratio.into(),
    );
    for (band, gain) in [
        "hz31", "hz62", "hz125", "hz250", "hz500", "hz1000", "hz2000", "hz4000", "hz8000",
        "hz16000",
    ]
    .iter()
    .zip(store.equalizer.gains_db)
    {
        values.insert(
            format!("player.soundEffect.biquadFilter.{band}"),
            gain.into(),
        );
    }
    LxSettings::from_value(&Value::Object(values), false)
}
