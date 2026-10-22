//! Versioned player state in its own application directory.
//! No existing LX Music configuration, browser profile or credentials are read.

use crate::{
    domain::{
        audio_output::AudioPreferences, equalizer::EqualizerSettings,
        playback_processing::PlaybackProcessingSettings,
    },
    source::{CURRENT_APPROVAL_POLICY_VERSION, ImportedSource},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

const STORE_VERSION: u32 = 1;
const MAX_STORE_BYTES: u64 = 8 * 1024 * 1024;
const STATE_NAME: &str = "player-state.json";
const BACKUP_NAME: &str = "player-state.json.bak";
const MAX_APPLIED_BACKUP_PREFERENCES: usize = 64;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoreData {
    pub sources: Vec<ImportedSource>,
    pub selected: Option<String>,
    pub local_files: Vec<PathBuf>,
    pub volume: f32,
    #[serde(default)]
    pub audio_preferences: AudioPreferences,
    #[serde(default)]
    pub equalizer: EqualizerSettings,
    #[serde(default)]
    pub playback_processing: PlaybackProcessingSettings,
    #[serde(default)]
    pub audio_analysis_enabled: bool,
    #[serde(default)]
    pub spatial: crate::audio::spatial::SpatialSettings,
    #[serde(default)]
    pub effects: Option<crate::backend::EffectsPreferences>,
    #[serde(default)]
    pub timeout_stop: crate::playback::timeout_stop::TimeoutStopPreferences,
    #[serde(default)]
    pub system_control: crate::system_control::SystemSettings,
}

impl Default for StoreData {
    fn default() -> Self {
        Self {
            sources: Vec::new(),
            selected: None,
            local_files: Vec::new(),
            volume: 0.7,
            audio_preferences: AudioPreferences::default(),
            equalizer: EqualizerSettings::default(),
            playback_processing: PlaybackProcessingSettings::default(),
            audio_analysis_enabled: false,
            spatial: Default::default(),
            effects: None,
            timeout_stop: Default::default(),
            system_control: Default::default(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    version: u32,
    data: StoreData,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    applied_backup_preferences: Vec<AppliedBackupPreferences>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AppliedBackupPreferences {
    operation_id: String,
    input_digest: String,
}

#[derive(Debug)]
pub struct BackupPreferencesConflict;

impl std::fmt::Display for BackupPreferencesConflict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Backup preference operation was reused with a different digest")
    }
}
impl std::error::Error for BackupPreferencesConflict {}

pub struct ProjectStore {
    data_dir: PathBuf,
    access: Mutex<()>,
}

impl ProjectStore {
    pub fn open(data_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&data_dir).with_context(|| {
            format!("Cannot create player data directory {}", data_dir.display())
        })?;
        ensure!(
            data_dir.is_dir(),
            "Player data path is not a directory: {}",
            data_dir.display()
        );
        Ok(Self {
            data_dir,
            access: Mutex::new(()),
        })
    }

    pub fn load(&self) -> Result<StoreData> {
        let _access = self
            .access
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(envelope) = self.read_envelope()? else {
            return Ok(StoreData::default());
        };
        let mut data = envelope.data;
        revoke_unbound_approval(&mut data);
        Ok(data)
    }

    pub fn save(&self, data: &StoreData) -> Result<()> {
        let _access = self
            .access
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        validate_data(data)?;
        let existing = self.read_envelope().context("Refusing to overwrite damaged player state; restore or explicitly reset the data file first")?;
        let markers = existing
            .map(|envelope| envelope.applied_backup_preferences)
            .unwrap_or_default();
        self.save_envelope(data, markers)
    }

    pub fn backup_preferences_applied(
        &self,
        operation_id: &str,
        input_digest: &str,
    ) -> Result<bool> {
        validate_backup_receipt(operation_id, input_digest)?;
        let _access = self
            .access
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(envelope) = self.read_envelope()? else {
            return Ok(false);
        };
        applied_receipt(
            &envelope.applied_backup_preferences,
            operation_id,
            input_digest,
        )
    }

    /// Saves a safe preference candidate and its application marker in the same
    /// envelope. The repository import is a separate durable commit.
    pub fn apply_backup_preferences(
        &self,
        operation_id: &str,
        input_digest: &str,
        candidate: &StoreData,
    ) -> Result<()> {
        self.apply_preferences(operation_id, input_digest, candidate, false)
    }

    /// Restores preferences and appends syntax-checked source archives without
    /// importing execution approval or changing existing machine permissions.
    pub fn apply_native_backup_preferences(
        &self,
        operation_id: &str,
        input_digest: &str,
        candidate: &StoreData,
    ) -> Result<()> {
        self.apply_preferences(operation_id, input_digest, candidate, true)
    }

    /// Checks the exact safe candidate and marker envelope before a repository
    /// commit. The application repeats these checks against current state.
    pub fn validate_native_backup_preferences_candidate(
        &self,
        operation_id: &str,
        input_digest: &str,
        candidate: &StoreData,
    ) -> Result<()> {
        validate_backup_receipt(operation_id, input_digest)?;
        let _access = self
            .access
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let existing = self.read_envelope()?;
        let (mut current, mut markers) = match existing {
            Some(envelope) => (envelope.data, envelope.applied_backup_preferences),
            None => (StoreData::default(), Vec::new()),
        };
        if applied_receipt(&markers, operation_id, input_digest)? {
            return Ok(());
        }
        revoke_unbound_approval(&mut current);
        validate_data(candidate)?;
        validate_native_authority_records(&current, candidate)?;
        markers.push(AppliedBackupPreferences {
            operation_id: operation_id.into(),
            input_digest: input_digest.into(),
        });
        if markers.len() > MAX_APPLIED_BACKUP_PREFERENCES {
            markers.remove(0);
        }
        let bytes = serde_json::to_vec_pretty(&Envelope {
            version: STORE_VERSION,
            data: candidate.clone(),
            applied_backup_preferences: markers,
        })?;
        ensure!(
            bytes.len() as u64 <= MAX_STORE_BYTES,
            "Player state exceeds the 8 MiB storage limit"
        );
        Ok(())
    }

    fn apply_preferences(
        &self,
        operation_id: &str,
        input_digest: &str,
        candidate: &StoreData,
        native_sources: bool,
    ) -> Result<()> {
        validate_backup_receipt(operation_id, input_digest)?;
        let _access = self
            .access
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let existing = self.read_envelope().context("Refusing to overwrite damaged player state; restore or explicitly reset the data file first")?;
        let (mut current, mut markers) = match existing {
            Some(envelope) => (envelope.data, envelope.applied_backup_preferences),
            None => (StoreData::default(), Vec::new()),
        };
        if applied_receipt(&markers, operation_id, input_digest)? {
            return Ok(());
        }
        revoke_unbound_approval(&mut current);
        validate_data(candidate)?;
        if native_sources {
            validate_native_authority_records(&current, candidate)?;
        } else {
            ensure!(
                same_authority_records(&current, candidate),
                "Backup preferences cannot alter current source approval, selection, local-file or impulse records"
            );
        }
        markers.push(AppliedBackupPreferences {
            operation_id: operation_id.into(),
            input_digest: input_digest.into(),
        });
        if markers.len() > MAX_APPLIED_BACKUP_PREFERENCES {
            markers.remove(0);
        }
        self.save_envelope(candidate, markers)
    }

    fn read_envelope(&self) -> Result<Option<Envelope>> {
        let state = self.data_dir.join(STATE_NAME);
        let backup = self.data_dir.join(BACKUP_NAME);
        // A missing primary plus a backup indicates an interrupted replacement;
        // a corrupt existing primary is never replaced by a fallback/default.
        let path = if state.try_exists().context("Cannot inspect player state")? {
            state
        } else if backup
            .try_exists()
            .context("Cannot inspect player state backup")?
        {
            backup
        } else {
            return Ok(None);
        };
        let envelope: Envelope =
            serde_json::from_slice(&read_bounded_file(&path)?).with_context(|| {
                format!(
                    "Player state is damaged or invalid JSON: {}",
                    path.display()
                )
            })?;
        ensure!(
            envelope.version == STORE_VERSION,
            "Unsupported player state version {}; expected {}",
            envelope.version,
            STORE_VERSION
        );
        validate_data(&envelope.data)?;
        validate_backup_markers(&envelope.applied_backup_preferences)?;
        Ok(Some(envelope))
    }

    fn save_envelope(
        &self,
        data: &StoreData,
        markers: Vec<AppliedBackupPreferences>,
    ) -> Result<()> {
        validate_backup_markers(&markers)?;
        let state = self.data_dir.join(STATE_NAME);
        let backup = self.data_dir.join(BACKUP_NAME);
        let mut data = data.clone();
        revoke_unbound_approval(&mut data);
        let bytes = serde_json::to_vec_pretty(&Envelope {
            version: STORE_VERSION,
            data,
            applied_backup_preferences: markers,
        })
        .context("Cannot encode player state")?;
        ensure!(
            bytes.len() as u64 <= MAX_STORE_BYTES,
            "Player state exceeds the 8 MiB storage limit"
        );
        let temp = self.data_dir.join(format!(
            ".player-state-{}-{}-{}.tmp",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let mut temp_created = false;
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .context("Cannot create temporary player state")?;
            temp_created = true;
            file.write_all(&bytes)
                .context("Cannot write temporary player state")?;
            file.sync_all()
                .context("Cannot flush temporary player state")?;
            drop(file);
            if state
                .try_exists()
                .context("Cannot inspect existing player state")?
            {
                if backup
                    .try_exists()
                    .context("Cannot inspect previous state backup")?
                {
                    fs::remove_file(&backup).context("Cannot replace previous state backup")?;
                }
                fs::rename(&state, &backup)
                    .context("Cannot preserve existing player state before replacement")?;
            }
            if let Err(error) = fs::rename(&temp, &state) {
                // Restore the previous complete file on replacement failure.
                let restore = if backup.exists() && !state.exists() {
                    fs::rename(&backup, &state)
                } else {
                    Ok(())
                };
                return Err(error).with_context(|| match restore {
                    Ok(()) => "Cannot install new player state; previous state was preserved".into(),
                    Err(error) => format!("Cannot install new player state; previous state remains in backup ({error})"),
                });
            }
            Ok(())
        })();
        if temp_created && temp.exists() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}

fn validate_data(data: &StoreData) -> Result<()> {
    validate_volume(data.volume)?;
    data.audio_preferences.validate()?;
    data.equalizer.validate()?;
    data.playback_processing.validate()?;
    data.spatial.validate()?;
    if let Some(effects) = &data.effects {
        effects.validate()?;
        ensure!(
            effects.settings.spatial == data.spatial,
            "Effect spatial preferences differ from their compatibility setting"
        );
    }
    data.timeout_stop.validate()?;
    data.system_control.validate()?;
    Ok(())
}

fn validate_backup_receipt(operation_id: &str, digest: &str) -> Result<()> {
    ensure!(
        !operation_id.is_empty()
            && operation_id.len() <= 256
            && !operation_id.chars().any(char::is_control),
        "Backup preference operation identity is invalid"
    );
    ensure!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "Backup preference input digest is invalid"
    );
    Ok(())
}

fn validate_backup_markers(markers: &[AppliedBackupPreferences]) -> Result<()> {
    ensure!(
        markers.len() <= MAX_APPLIED_BACKUP_PREFERENCES,
        "Player state has too many backup preference markers"
    );
    let mut seen = std::collections::BTreeSet::new();
    for marker in markers {
        validate_backup_receipt(&marker.operation_id, &marker.input_digest)?;
        ensure!(
            seen.insert(&marker.operation_id),
            "Player state repeats a backup preference operation identity"
        );
    }
    Ok(())
}

fn applied_receipt(
    markers: &[AppliedBackupPreferences],
    operation_id: &str,
    digest: &str,
) -> Result<bool> {
    if let Some(marker) = markers
        .iter()
        .find(|marker| marker.operation_id == operation_id)
    {
        if marker.input_digest != digest {
            return Err(BackupPreferencesConflict.into());
        }
        return Ok(true);
    }
    Ok(false)
}

fn same_authority_records(current: &StoreData, candidate: &StoreData) -> bool {
    current.selected == candidate.selected
        && current.local_files == candidate.local_files
        && same_effect_authority_records(current, candidate)
        && current.sources.len() == candidate.sources.len()
        && current
            .sources
            .iter()
            .zip(&candidate.sources)
            .all(|(left, right)| {
                left.metadata == right.metadata
                    && left.script == right.script
                    && left.sha256 == right.sha256
                    && left.approved == right.approved
                    && left.approval_policy_version == right.approval_policy_version
            })
}

fn validate_native_authority_records(current: &StoreData, candidate: &StoreData) -> Result<()> {
    ensure!(
        current.selected == candidate.selected
            && current.local_files == candidate.local_files
            && same_effect_authority_records(current, candidate)
            && candidate.sources.len() >= current.sources.len()
            && candidate.sources.len() - current.sources.len() <= 64
            && current
                .sources
                .iter()
                .zip(&candidate.sources)
                .all(|(left, right)| {
                    left.metadata == right.metadata
                        && left.script == right.script
                        && left.sha256 == right.sha256
                        && left.approved == right.approved
                        && left.approval_policy_version == right.approval_policy_version
                }),
        "Native backup cannot change existing source, selection, local-file or impulse authority"
    );
    let mut hashes: std::collections::BTreeSet<_> = current
        .sources
        .iter()
        .map(|source| source.sha256.as_str())
        .collect();
    for source in &candidate.sources[current.sources.len()..] {
        ensure!(
            !source.approved && source.approval_policy_version == 0,
            "Native source archives cannot carry execution approval"
        );
        let parsed = crate::source::import_script(&source.script)?;
        ensure!(
            source.sha256 == parsed.sha256
                && source.metadata == parsed.metadata
                && hashes.insert(source.sha256.as_str()),
            "Native source archive is altered or repeats an existing source"
        );
    }
    Ok(())
}

fn same_effect_authority_records(current: &StoreData, candidate: &StoreData) -> bool {
    let authority = |data: &StoreData| {
        let mut effects = data.effects.clone().unwrap_or_default();
        effects.settings = Default::default();
        serde_json::to_value(effects)
    };
    match (authority(current), authority(candidate)) {
        (Ok(current), Ok(candidate)) => current == candidate,
        _ => false,
    }
}

fn validate_volume(volume: f32) -> Result<()> {
    ensure!(
        volume.is_finite() && (0.0..=1.0).contains(&volume),
        "Stored volume must be a finite value between 0 and 1"
    );
    Ok(())
}

fn revoke_unbound_approval(data: &mut StoreData) {
    for source in &mut data.sources {
        if source.script.len() > crate::source::MAX_SCRIPT_BYTES
            || source.verify_integrity().is_err()
            || (source.approved
                && source.approval_policy_version != CURRENT_APPROVAL_POLICY_VERSION)
        {
            source.approved = false;
        }
    }
    if data.selected.as_ref().is_some_and(|selected| {
        !data
            .sources
            .iter()
            .any(|source| &source.sha256 == selected && source.require_approval().is_ok())
    }) {
        data.selected = None;
    }
}

fn read_bounded_file(path: &Path) -> Result<Vec<u8>> {
    let file =
        File::open(path).with_context(|| format!("Cannot read player state {}", path.display()))?;
    ensure!(
        file.metadata()?.len() <= MAX_STORE_BYTES,
        "Player state exceeds the 8 MiB storage limit"
    );
    let mut bytes = Vec::new();
    file.take(MAX_STORE_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("Cannot read player state bytes")?;
    ensure!(
        bytes.len() as u64 <= MAX_STORE_BYTES,
        "Player state grew beyond the 8 MiB storage limit while being read"
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::import_script;

    fn approved_source() -> ImportedSource {
        let mut source =
            import_script("/* @name Storage fixture */\nglobalThis.fixture = 1;").unwrap();
        source.approve().unwrap();
        source
    }

    #[test]
    fn state_restores_exact_source_approval_selection_and_library_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let source = approved_source();
        let data = StoreData {
            selected: Some(source.sha256.clone()),
            sources: vec![source],
            local_files: vec![PathBuf::from("legal-audio.wav")],
            volume: 0.42,
            audio_preferences: AudioPreferences::default(),
            equalizer: EqualizerSettings::default(),
            playback_processing: PlaybackProcessingSettings::default(),
            audio_analysis_enabled: false,
            spatial: Default::default(),
            effects: None,
            timeout_stop: Default::default(),
            system_control: Default::default(),
        };
        ProjectStore::open(dir.path().into())
            .unwrap()
            .save(&data)
            .unwrap();
        let restored = ProjectStore::open(dir.path().into())
            .unwrap()
            .load()
            .unwrap();
        assert_eq!(restored.selected, data.selected);
        assert!(restored.sources[0].approved);
        assert_eq!(
            restored.sources[0].approval_policy_version,
            CURRENT_APPROVAL_POLICY_VERSION
        );
        restored.sources[0].require_approval().unwrap();
        assert_eq!(restored.sources[0].script, data.sources[0].script);
        assert_eq!(restored.local_files, data.local_files);
        assert_eq!(restored.volume, data.volume);
    }

    #[test]
    fn damaged_json_is_reported_and_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(STATE_NAME);
        fs::write(&file, b"{ damaged").unwrap();
        let error = ProjectStore::open(dir.path().into())
            .unwrap()
            .load()
            .unwrap_err();
        assert!(error.to_string().contains("damaged"));
        assert!(
            ProjectStore::open(dir.path().into())
                .unwrap()
                .save(&StoreData::default())
                .is_err()
        );
        assert_eq!(fs::read(&file).unwrap(), b"{ damaged");
    }

    #[test]
    fn legacy_loopback_approval_is_revoked_before_restart_execution() {
        let dir = tempfile::tempdir().unwrap();
        let source = approved_source();
        let store = ProjectStore::open(dir.path().into()).unwrap();
        store
            .save(&StoreData {
                selected: Some(source.sha256.clone()),
                sources: vec![source],
                ..Default::default()
            })
            .unwrap();
        let file = dir.path().join(STATE_NAME);
        let mut envelope: serde_json::Value =
            serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        envelope["data"]["sources"][0]
            .as_object_mut()
            .unwrap()
            .remove("approval_policy_version");
        let legacy: ImportedSource =
            serde_json::from_value(envelope["data"]["sources"][0].clone()).unwrap();
        assert!(legacy.approved);
        assert_eq!(legacy.approval_policy_version, 0);
        assert!(
            legacy
                .require_approval()
                .unwrap_err()
                .to_string()
                .contains("permissions changed")
        );
        // The older record still says approved=true and has a matching script hash.
        let restored = {
            fs::write(&file, serde_json::to_vec(&envelope).unwrap()).unwrap();
            store.load().unwrap()
        };
        assert!(restored.selected.is_none());
        assert!(!restored.sources[0].approved);
        assert_eq!(restored.sources[0].approval_policy_version, 0);
        restored.sources[0].verify_integrity().unwrap();
        assert!(restored.sources[0].require_approval().is_err());

        let mut reapproved = restored;
        reapproved.sources[0].approve().unwrap();
        reapproved.selected = Some(reapproved.sources[0].sha256.clone());
        store.save(&reapproved).unwrap();
        let reopened = ProjectStore::open(dir.path().into())
            .unwrap()
            .load()
            .unwrap();
        assert_eq!(reopened.selected, reapproved.selected);
        reopened.sources[0].require_approval().unwrap();
        assert_eq!(
            reopened.sources[0].approval_policy_version,
            CURRENT_APPROVAL_POLICY_VERSION
        );
    }

    #[test]
    fn modified_script_revokes_approval_and_selection() {
        let dir = tempfile::tempdir().unwrap();
        let source = approved_source();
        let store = ProjectStore::open(dir.path().into()).unwrap();
        store
            .save(&StoreData {
                selected: Some(source.sha256.clone()),
                sources: vec![source],
                ..Default::default()
            })
            .unwrap();
        let file = dir.path().join(STATE_NAME);
        let mut envelope: serde_json::Value =
            serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        envelope["data"]["sources"][0]["script"] =
            "/* @name Replaced */\nglobalThis.changed = true;".into();
        fs::write(&file, serde_json::to_vec(&envelope).unwrap()).unwrap();
        let mut restored = store.load().unwrap();
        assert!(!restored.sources[0].approved);
        assert!(restored.selected.is_none());
        assert!(restored.sources[0].verify_integrity().is_err());
        assert!(restored.sources[0].approve().is_err());
    }

    #[test]
    fn modified_hash_cannot_reuse_current_network_approval() {
        let dir = tempfile::tempdir().unwrap();
        let source = approved_source();
        let store = ProjectStore::open(dir.path().into()).unwrap();
        store
            .save(&StoreData {
                selected: Some(source.sha256.clone()),
                sources: vec![source],
                ..Default::default()
            })
            .unwrap();
        let file = dir.path().join(STATE_NAME);
        let mut envelope: serde_json::Value =
            serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        envelope["data"]["sources"][0]["sha256"] = "0".repeat(64).into();
        fs::write(&file, serde_json::to_vec(&envelope).unwrap()).unwrap();
        let restored = store.load().unwrap();
        assert!(restored.selected.is_none());
        assert!(!restored.sources[0].approved);
        assert!(restored.sources[0].require_approval().is_err());
    }

    #[test]
    fn interrupted_replacement_recovers_backup() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProjectStore::open(dir.path().into()).unwrap();
        store
            .save(&StoreData {
                volume: 0.33,
                ..Default::default()
            })
            .unwrap();
        fs::rename(dir.path().join(STATE_NAME), dir.path().join(BACKUP_NAME)).unwrap();
        assert_eq!(store.load().unwrap().volume, 0.33);
    }

    #[test]
    fn replacement_preserves_a_complete_previous_version() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProjectStore::open(dir.path().into()).unwrap();
        store
            .save(&StoreData {
                volume: 0.25,
                ..Default::default()
            })
            .unwrap();
        store
            .save(&StoreData {
                volume: 0.75,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(store.load().unwrap().volume, 0.75);
        let previous: Envelope =
            serde_json::from_slice(&fs::read(dir.path().join(BACKUP_NAME)).unwrap()).unwrap();
        assert_eq!(previous.version, STORE_VERSION);
        assert_eq!(previous.data.volume, 0.25);
    }

    #[test]
    fn oversized_state_and_unknown_version_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(STATE_NAME);
        let store = ProjectStore::open(dir.path().into()).unwrap();
        fs::write(&file, b"{\"version\":999,\"data\":{\"sources\":[],\"selected\":null,\"local_files\":[],\"volume\":0.5}}").unwrap();
        assert!(store.load().unwrap_err().to_string().contains("version"));
        File::create(file)
            .unwrap()
            .set_len(MAX_STORE_BYTES + 1)
            .unwrap();
        assert!(store.load().unwrap_err().to_string().contains("8 MiB"));
    }
}
