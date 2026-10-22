#![cfg(windows)]

use crate::{
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, CommandEnvelope, CommandReceipt, EffectsBackendState,
        EffectsPreferences, ReceiptState,
    },
    backup::{BackupEncoding, ConflictPolicy, NativeBackup, NativeBackupOptions, encode_native},
    domain::{audio_effects::EffectsSettings, lists::ListData},
    repository::{
        ImpulseImportFact, ImpulseImportPhase, StoredImpulseResource, local_track_from_path,
    },
    store::ProjectStore,
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

const WAIT: Duration = Duration::from_secs(30);

struct Running {
    directory: Option<tempfile::TempDir>,
    backend: Option<BackendHandle>,
}
impl Running {
    fn new() -> Self {
        let root = std::env::temp_dir();
        let directory = tempfile::Builder::new()
            .prefix("owned-profile-")
            .tempdir_in(&root)
            .unwrap();
        let backend =
            BackendHandle::start(BackendConfig::headless(directory.path().join("profile")))
                .unwrap();
        Self {
            directory: Some(directory),
            backend: Some(backend),
        }
    }
    fn root(&self) -> &Path {
        self.directory.as_ref().unwrap().path()
    }
    fn profile(&self) -> PathBuf {
        self.root().join("profile")
    }
    fn backend(&self) -> &BackendHandle {
        self.backend.as_ref().unwrap()
    }
    fn shutdown(&mut self) {
        if let Some(backend) = self.backend.take() {
            if let Err(error) = backend.shutdown(WAIT) {
                self.backend = Some(backend);
                panic!("Actual backend retirement failed: {error}");
            }
            let retired = backend.retirement_state();
            assert!(
                retired.actor_joined && retired.runtime_retired,
                "{retired:?}"
            );
        }
    }
    fn restart(&mut self) {
        self.shutdown();
        self.backend = Some(BackendHandle::start(BackendConfig::headless(self.profile())).unwrap());
    }
    fn complete(&self, id: &str, command: BackendCommand) -> CommandReceipt {
        let backend = self.backend();
        let cursor = backend.event_cursor();
        let receipt = backend
            .execute(CommandEnvelope {
                request_id: id.into(),
                origin: "effects-lifecycle-test".into(),
                expected_revision: None,
                command,
            })
            .unwrap();
        if receipt.state == ReceiptState::Accepted {
            operation_finished(backend, cursor, &receipt.operation_id);
        }
        receipt
    }
    fn import(&self, id: &str, name: &str, first: i16) -> (PathBuf, StoredImpulseResource) {
        let selected = self.root().join(name);
        fs::write(&selected, wave(8000, &[first, 0, 0, 0])).unwrap();
        let expected_sha = format!("{:x}", Sha256::digest(fs::read(&selected).unwrap()));
        let before = preferences(self.backend()).resources;
        let receipt = self.complete(
            id,
            BackendCommand::ImportImpulseResource {
                path: selected.clone(),
                file_read_approved: true,
                timeout_ms: Some(30_000),
            },
        );
        assert_eq!(receipt.state, ReceiptState::Accepted);
        let admitted = preferences(self.backend());
        admitted.validate().unwrap();
        assert!(admitted.vault.is_some());
        assert!(admitted.imports.is_empty());
        let resource = admitted
            .resources
            .into_iter()
            .find(|resource| {
                !before
                    .iter()
                    .any(|old| old.resource_id == resource.resource_id)
            })
            .expect("Import did not durably register its actual representation");
        assert_eq!(resource.encoded_sha256, expected_sha);
        assert_eq!(
            fs::metadata(body_path(&self.profile(), &resource))
                .unwrap()
                .len(),
            resource.storage_bytes()
        );
        (selected, resource)
    }
    fn select(&self, id: &str, resource: &StoredImpulseResource) -> EffectsSettings {
        let settings = EffectsSettings {
            convolution: crate::domain::audio_effects::ConvolutionSettings {
                impulse: Some(resource.resource_id.clone()),
                main_gain: 0.5,
                send_gain: 0.25,
            },
            ..Default::default()
        };
        self.complete(
            id,
            BackendCommand::SetEffects {
                settings: settings.clone(),
            },
        );
        let ready = ready_state(self.backend());
        assert_eq!(ready.persisted, settings);
        assert_eq!(ready.audio.desired, settings);
        settings
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        if let Some(backend) = self.backend.take()
            && let Err(error) = backend.shutdown(WAIT)
        {
            if let Some(directory) = self.directory.take() {
                let _ = directory.keep();
            }
            if !thread::panicking() {
                panic!("Actual backend retirement failed: {error}");
            }
        }
    }
}

fn wave(rate: u32, samples: &[i16]) -> Vec<u8> {
    let length = u32::try_from(samples.len() * 2).unwrap();
    let mut bytes = Vec::with_capacity(44 + samples.len() * 2);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + length).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&rate.to_le_bytes());
    bytes.extend_from_slice(&(rate * 2).to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&length.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}
fn body_path(profile: &Path, resource: &StoredImpulseResource) -> PathBuf {
    let crate::domain::audio_effects::ImpulseResourceId::Imported(token) = &resource.resource_id
    else {
        panic!("A selected native import must have an actual imported identity")
    };
    profile
        .join(".lx-impulse-vault")
        .join(format!(".lx-profile-{token}-impulse.bin"))
}
fn preferences(backend: &BackendHandle) -> EffectsPreferences {
    match backend.query(BackendQuery::ImpulseResources).unwrap() {
        BackendQueryResult::ImpulseResources(value) => value,
        value => panic!("Impulse inventory response differs: {value:?}"),
    }
}
fn state(backend: &BackendHandle) -> EffectsBackendState {
    match backend.query(BackendQuery::EffectsState).unwrap() {
        BackendQueryResult::EffectsState(value) => *value,
        value => panic!("Effects state response differs: {value:?}"),
    }
}
fn ready_state(backend: &BackendHandle) -> EffectsBackendState {
    let deadline = Instant::now() + WAIT;
    loop {
        let current = state(backend);
        assert!(current.resource_error.is_none(), "{current:?}");
        if current.resource_ready && !current.resource_preparing {
            return current;
        }
        assert!(
            Instant::now() < deadline,
            "Actual owned IR preparation did not finish: {current:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
fn presented_state(backend: &BackendHandle) -> EffectsBackendState {
    let deadline = Instant::now() + WAIT;
    loop {
        let current = state(backend);
        assert!(
            current.resource_error.is_none() && current.audio.error.is_none(),
            "{current:?}"
        );
        if current.audio.applied_sequence == current.audio.requested_sequence
            && current.audio.applied_sequence > 0
        {
            let BackendQueryResult::RenderedPcm(Some(pcm)) =
                backend.query(BackendQuery::RenderedPcm).unwrap()
            else {
                panic!("Presentation requires real Memory render observations")
            };
            assert!(pcm.batches > 0 && pcm.peak > 0.0001, "{pcm:?}");
            return current;
        }
        assert!(
            Instant::now() < deadline,
            "Requested graph never reached actual PCM presentation: {current:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
fn operation_finished(backend: &BackendHandle, mut cursor: u64, operation: &str) {
    let deadline = Instant::now() + WAIT;
    loop {
        for event in backend.events_after(cursor).unwrap() {
            cursor = event.event_id;
            if let BackendEvent::OperationFinished {
                operation_id,
                success,
                error,
            } = event.payload
                && operation_id == operation
            {
                assert!(success, "{operation} failed: {error:?}");
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "{operation} did not publish an actual completion"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
fn inventory(preferences: &EffectsPreferences) -> serde_json::Value {
    let serialized = serde_json::to_value(preferences).unwrap();
    serde_json::json!({
        "vault": serialized.get("vault"),
        "creation": serialized.get("creation"),
        "resources": serialized.get("resources"),
        "imports": serialized.get("imports"),
        "retirements": serialized.get("retirements"),
    })
}

#[test]
fn actual_import_survives_source_removal_restart_presentation_reset_and_retirement() {
    let mut running = Running::new();
    let (selected, resource) = running.import("ir-import", "selected-response.wav", 16384);
    let body = body_path(&running.profile(), &resource);
    let actual_body = fs::read(&body).unwrap();
    let settings = running.select("ir-select", &resource);
    assert_eq!(state(running.backend()).audio.applied_sequence, 0);
    fs::remove_file(&selected).unwrap();
    assert!(!selected.exists());
    running.restart();
    let restored = ready_state(running.backend());
    assert_eq!(restored.persisted, settings);
    assert_eq!(restored.audio.desired, settings);
    assert_eq!(restored.audio.applied_sequence, 0);
    assert_eq!(
        preferences(running.backend()).resources.as_slice(),
        std::slice::from_ref(&resource)
    );
    assert_eq!(fs::read(&body).unwrap(), actual_body);

    let track_path = running.root().join("playback-tone.wav");
    let samples: Vec<i16> = (0..22050 * 16)
        .map(|index| {
            ((index as f64 * 440.0 * std::f64::consts::TAU / 22050.0).sin() * 8192.0) as i16
        })
        .collect();
    let track_bytes = wave(22050, &samples);
    fs::write(&track_path, &track_bytes).unwrap();
    let (track, _) = local_track_from_path(&track_path).unwrap();
    running.complete(
        "ir-import-playback",
        BackendCommand::ImportAudio {
            paths: vec![track_path.clone()],
        },
    );
    running.complete("ir-play", BackendCommand::Play { id: track.id });
    let applied = presented_state(running.backend());
    assert_eq!(applied.persisted, settings);
    assert!(applied.audio.owned_bytes > 0);
    let generation = running.backend().snapshot().playback.generation;
    running.complete("ir-reset", BackendCommand::ResetEffects);
    let reset = presented_state(running.backend());
    assert_eq!(reset.persisted, EffectsSettings::default());
    assert!(reset.audio.requested_sequence > applied.audio.requested_sequence);
    assert_eq!(running.backend().snapshot().playback.generation, generation);
    running.complete(
        "ir-retire",
        BackendCommand::RetireImpulseResource {
            resource_id: resource.resource_id.clone(),
        },
    );
    let after = preferences(running.backend());
    assert!(after.resources.is_empty() && after.imports.is_empty());
    assert!(after.retirements.iter().all(|record| record.removed()));
    assert!(!body.exists());
    assert_eq!(fs::read(&track_path).unwrap(), track_bytes);
    running.shutdown();
    let saved = ProjectStore::open(running.profile())
        .unwrap()
        .load()
        .unwrap();
    let saved = saved.effects.unwrap();
    assert!(saved.resources.is_empty() && saved.imports.is_empty());
    assert_eq!(saved.settings, EffectsSettings::default());
}

#[test]
fn native_backup_projection_and_actual_restore_preserve_the_destination_vault() {
    let mut source = Running::new();
    let (_, source_resource) = source.import("source-ir-import", "source-ir.wav", 8192);
    source.select("source-ir-select", &source_resource);
    let mut source_settings = preferences(source.backend()).settings;
    source_settings.spatial.enabled = true;
    source_settings.spatial.radius = 0.6;
    source_settings.spatial.speed = 12;
    source.complete(
        "source-spatial",
        BackendCommand::SetEffects {
            settings: source_settings,
        },
    );
    source.complete("source-volume", BackendCommand::Volume { volume: 0.23 });
    source.shutdown();
    let source_store = ProjectStore::open(source.profile())
        .unwrap()
        .load()
        .unwrap();
    let backup = NativeBackup::from_snapshot(
        &source_store,
        &ListData::default(),
        "",
        &[],
        &[],
        NativeBackupOptions::default(),
    )
    .unwrap();

    let mut destination = Running::new();
    let (selected, local_resource) =
        destination.import("destination-ir-import", "destination-ir.wav", 16384);
    destination.select("destination-ir-select", &local_resource);
    let destination_store = ProjectStore::open(destination.profile()).unwrap();
    let baseline = destination_store.load().unwrap();
    let baseline_effects = baseline.effects.as_ref().unwrap();
    assert_ne!(source_resource.namespace, local_resource.namespace);
    let body = body_path(&destination.profile(), &local_resource);
    let body_bytes = fs::read(&body).unwrap();
    let source_body = body_path(&source.profile(), &source_resource);
    let source_body_bytes = fs::read(&source_body).unwrap();
    let plan = backup
        .preview(&ListData::default(), &ConflictPolicy::ReplaceTracks)
        .unwrap();
    let projected = plan.project_store(&baseline);
    let projected_effects = projected.effects.as_ref().unwrap();
    assert_eq!(inventory(projected_effects), inventory(baseline_effects));
    assert_eq!(
        projected_effects.settings.convolution,
        baseline_effects.settings.convolution
    );
    assert_eq!(projected_effects.settings.spatial, source_store.spatial);

    let selected_backup = destination.root().join("selected-native-backup.json");
    fs::write(
        &selected_backup,
        encode_native(&backup, BackupEncoding::Json).unwrap(),
    )
    .unwrap();
    destination.complete(
        "native-ir-preview",
        BackendCommand::PreviewNativeBackup {
            path: selected_backup,
            replace_tracks: false,
        },
    );
    assert_eq!(
        inventory(&preferences(destination.backend())),
        inventory(baseline_effects)
    );
    let BackendQueryResult::NativeBackupPreview(Some(preview)) = destination
        .backend()
        .query(BackendQuery::NativeBackupPreview)
        .unwrap()
    else {
        panic!("A real selected native archive did not produce its checked preview")
    };
    destination.complete(
        "native-ir-commit",
        BackendCommand::CommitNativeBackup {
            preview_operation_id: preview.operation_id,
            input_digest: preview.input_digest.clone(),
        },
    );
    let BackendQueryResult::NativeBackupImportResult(Some(imported)) = destination
        .backend()
        .query(BackendQuery::NativeBackupImportResult {
            operation_id: "native-ir-commit".into(),
        })
        .unwrap()
    else {
        panic!("Native restoration did not durably publish its receipt")
    };
    assert!(imported.preferences_applied);
    assert!(
        destination_store
            .backup_preferences_applied(&imported.operation_id, &imported.input_digest)
            .unwrap()
    );
    let restored = destination_store.load().unwrap();
    let restored_effects = restored.effects.as_ref().unwrap();
    assert_eq!(inventory(restored_effects), inventory(baseline_effects));
    assert_eq!(
        restored_effects.settings.convolution,
        baseline_effects.settings.convolution
    );
    assert_eq!(restored_effects.settings.spatial, source_store.spatial);
    assert!((restored.volume - source_store.volume).abs() < 0.0001);
    assert_eq!(fs::read(&body).unwrap(), body_bytes);
    assert_eq!(fs::read(&source_body).unwrap(), source_body_bytes);
    fs::remove_file(selected).unwrap();
    destination.restart();
    let ready = ready_state(destination.backend());
    assert_eq!(ready.persisted, restored_effects.settings);
    assert_eq!(
        inventory(&preferences(destination.backend())),
        inventory(baseline_effects)
    );
    assert_eq!(fs::read(&body).unwrap(), body_bytes);
    destination.shutdown();
}

#[test]
fn both_backup_store_entries_reject_changes_to_an_actually_imported_vault_without_writing() {
    let mut running = Running::new();
    let (_, resource) = running.import(
        "backup-authority-ir-import",
        "backup-authority-ir.wav",
        16384,
    );
    running.select("backup-authority-ir-select", &resource);
    running.shutdown();
    let store = ProjectStore::open(running.profile()).unwrap();
    let current = store.load().unwrap();
    let state_path = running.profile().join("player-state.json");
    let original_state = fs::read(&state_path).unwrap();
    let body = body_path(&running.profile(), &resource);
    let original_body = fs::read(&body).unwrap();
    for change in ["vault", "resource", "imports", "clear"] {
        let mut candidate = current.clone();
        let effects = candidate.effects.as_mut().unwrap();
        match change {
            "vault" => effects
                .vault
                .as_mut()
                .unwrap()
                .owner_identity
                .object
                .push('0'),
            "resource" => {
                let digest = &mut effects.resources[0].encoded_sha256;
                let replacement = if digest.starts_with('0') { "1" } else { "0" };
                digest.replace_range(..1, replacement);
            }
            "imports" => {
                let crate::domain::audio_effects::ImpulseResourceId::Imported(token) =
                    &resource.resource_id
                else {
                    unreachable!()
                };
                effects.imports.push(ImpulseImportFact {
                    namespace: resource.namespace.clone(),
                    token: token.clone(),
                    body_identity: Some(resource.body_identity.clone()),
                    reserved_bytes: resource.storage_bytes(),
                    expected_sha256: resource.encoded_sha256.clone(),
                    phase: ImpulseImportPhase::Sealed,
                });
            }
            "clear" => candidate.effects = None,
            _ => unreachable!(),
        }
        if let Some(effects) = &candidate.effects {
            effects.validate().unwrap();
        }
        let digest = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&candidate).unwrap())
        );
        for native in [false, true] {
            let operation = format!(
                "reject-{change}-{}",
                if native { "native" } else { "logical" }
            );
            assert!(
                !store
                    .backup_preferences_applied(&operation, &digest)
                    .unwrap()
            );
            let rejected = if native {
                assert!(
                    store
                        .validate_native_backup_preferences_candidate(
                            &operation, &digest, &candidate
                        )
                        .is_err()
                );
                store.apply_native_backup_preferences(&operation, &digest, &candidate)
            } else {
                store.apply_backup_preferences(&operation, &digest, &candidate)
            };
            assert!(
                rejected.is_err(),
                "{operation} accepted changed actual impulse inventory"
            );
            assert_eq!(
                fs::read(&state_path).unwrap(),
                original_state,
                "{operation} modified durable player-state bytes"
            );
            assert!(
                !store
                    .backup_preferences_applied(&operation, &digest)
                    .unwrap(),
                "{operation} left an application marker"
            );
            assert_eq!(
                serde_json::to_value(store.load().unwrap()).unwrap(),
                serde_json::to_value(&current).unwrap()
            );
            assert_eq!(fs::read(&body).unwrap(), original_body);
        }
    }
    running.restart();
    assert_eq!(
        ready_state(running.backend()).persisted,
        current.effects.unwrap().settings
    );
    assert_eq!(preferences(running.backend()).resources, [resource]);
    running.shutdown();
}
