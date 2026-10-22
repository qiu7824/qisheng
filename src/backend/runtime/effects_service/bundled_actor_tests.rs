#![cfg(windows)]
use crate::{
    audio::bundled_impulses,
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, CommandEnvelope, CommandReceipt, EffectsBackendState,
        EffectsPreferences, ReceiptState,
    },
    domain::audio_effects::{EffectsSettings, ImpulseResourceId, SpatialSettings},
    repository::local_track_from_path,
    store::ProjectStore,
};
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};
const WAIT: Duration = Duration::from_secs(30);
#[derive(Default)]
struct Gate {
    operation: Mutex<Option<String>>,
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}
static GATES: OnceLock<Mutex<HashMap<PathBuf, Arc<Gate>>>> = OnceLock::new();
pub(in crate::backend::runtime) fn after_result(operation: &str) {
    let gate = GATES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .values()
        .find(|gate| {
            gate.operation
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_deref()
                == Some(operation)
        })
        .cloned();
    if let Some(gate) = gate {
        let mut state = gate.state.lock().unwrap_or_else(|error| error.into_inner());
        state.0 = true;
        gate.changed.notify_all();
        while !state.1 {
            state = gate
                .changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
    }
}
struct Case {
    temp: Option<tempfile::TempDir>,
    backend: Option<BackendHandle>,
    gate: Arc<Gate>,
}
impl Case {
    fn new() -> Self {
        let temporary_root = std::env::temp_dir().join("lx-native-fixtures");
        let root = temporary_root.as_path();
        fs::create_dir_all(root).unwrap();
        let temp = tempfile::Builder::new()
            .prefix("owned-")
            .tempdir_in(root)
            .unwrap();
        let profile = temp.path().join("profile");
        let gate = Arc::new(Gate::default());
        GATES
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert(profile.clone(), gate.clone());
        let backend = BackendHandle::start(BackendConfig::headless(profile)).unwrap();
        Self {
            temp: Some(temp),
            backend: Some(backend),
            gate,
        }
    }
    fn root(&self) -> &Path {
        self.temp.as_ref().unwrap().path()
    }
    fn profile(&self) -> PathBuf {
        self.root().join("profile")
    }
    fn backend(&self) -> &BackendHandle {
        self.backend.as_ref().unwrap()
    }
    fn state(&self) -> EffectsBackendState {
        let BackendQueryResult::EffectsState(state) =
            self.backend().query(BackendQuery::EffectsState).unwrap()
        else {
            panic!("Wrong effects query");
        };
        *state
    }
    fn preferences(&self) -> EffectsPreferences {
        let BackendQueryResult::ImpulseResources(preferences) = self
            .backend()
            .query(BackendQuery::ImpulseResources)
            .unwrap()
        else {
            panic!("Wrong resources query");
        };
        preferences
    }
    fn execute(&self, id: &str, command: BackendCommand) -> CommandReceipt {
        self.backend()
            .execute(CommandEnvelope {
                request_id: id.into(),
                origin: "bundled-impulse-test".into(),
                expected_revision: None,
                command,
            })
            .unwrap()
    }
    fn outcome(&self, id: &str, mut cursor: u64) -> (bool, Option<String>) {
        let deadline = Instant::now() + WAIT;
        loop {
            for event in self.backend().events_after(cursor).unwrap() {
                cursor = event.event_id;
                if let BackendEvent::OperationFinished {
                    operation_id,
                    success,
                    error,
                } = event.payload
                    && operation_id == id
                {
                    return (success, error.map(|error| error.message));
                }
            }
            assert!(Instant::now() < deadline, "{id} has no terminal result");
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn complete(&self, id: &str, command: BackendCommand) -> CommandReceipt {
        let cursor = self.backend().event_cursor();
        let receipt = self.execute(id, command);
        if receipt.state == ReceiptState::Accepted {
            let (success, error) = self.outcome(&receipt.operation_id, cursor);
            assert!(success, "{id}: {error:?}");
        }
        receipt
    }
    fn select(&self, operation: &str, id: &str) -> EffectsSettings {
        self.complete(
            operation,
            BackendCommand::ApplyBundledImpulsePreset { id: id.into() },
        );
        self.wait_ready();
        let state = self.state();
        assert_eq!(
            state.persisted.convolution,
            bundled_impulses::preset(id).unwrap().convolution_settings()
        );
        assert_eq!(state.audio.desired, state.persisted);
        state.persisted
    }
    fn wait_ready(&self) {
        let deadline = Instant::now() + WAIT;
        loop {
            let state = self.state();
            assert!(state.resource_error.is_none(), "{state:?}");
            if state.resource_ready && !state.resource_preparing {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Restore did not finish: {state:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn wait_presented(&self) -> EffectsBackendState {
        let deadline = Instant::now() + WAIT;
        loop {
            let state = self.state();
            assert!(
                state.resource_error.is_none() && state.audio.error.is_none(),
                "{state:?}"
            );
            if state.audio.applied_sequence == state.audio.requested_sequence
                && state.audio.applied_sequence > 0
            {
                let BackendQueryResult::RenderedPcm(Some(pcm)) =
                    self.backend().query(BackendQuery::RenderedPcm).unwrap()
                else {
                    panic!("No real Memory PCM");
                };
                if pcm.batches > 0 && pcm.peak > 0.0001 && pcm.rms > 0.00001 {
                    return state;
                }
            }
            assert!(
                Instant::now() < deadline,
                "Graph not presented in actual Memory PCM: {state:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn assert_no_private_impulse_inventory(&self) {
        let preferences = self.preferences();
        preferences.validate().unwrap();
        assert!(preferences.vault.is_none() && preferences.creation.is_none());
        assert!(
            preferences.resources.is_empty()
                && preferences.imports.is_empty()
                && preferences.retirements.is_empty()
        );
        assert!(!self.profile().join(".lx-impulse-vault").exists());
    }
    fn restart(&mut self) {
        self.shutdown();
        self.backend = Some(BackendHandle::start(BackendConfig::headless(self.profile())).unwrap());
    }
    fn shutdown(&mut self) {
        if let Some(backend) = self.backend.take() {
            if let Err(error) = backend.shutdown(WAIT) {
                self.backend = Some(backend);
                panic!("Actual bundled resource retirement failed: {error}");
            }
            let retired = backend.retirement_state();
            assert!(
                retired.actor_joined && retired.runtime_retired,
                "{retired:?}"
            );
        }
    }
    fn gate_operation(&self, operation: &str) {
        *self.gate.state.lock().unwrap() = (false, false);
        *self.gate.operation.lock().unwrap() = Some(operation.into());
    }
    fn wait_result_while_worker_owned(&self) {
        let deadline = Instant::now() + WAIT;
        let mut state = self.gate.state.lock().unwrap();
        while !state.0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "Actual resource producer never published its result"
            );
            state = self.gate.changed.wait_timeout(state, remaining).unwrap().0;
        }
        assert!(!state.1);
    }
    fn release_worker(&self) {
        self.gate.state.lock().unwrap().1 = true;
        self.gate.changed.notify_all();
    }
}
impl Drop for Case {
    fn drop(&mut self) {
        self.release_worker();
        if let Some(backend) = self.backend.take()
            && let Err(error) = backend.shutdown(WAIT)
        {
            if let Some(temp) = self.temp.take() {
                let _ = temp.keep();
            }
            if !thread::panicking() {
                panic!("Actual bundled resource retirement failed: {error}");
            }
            return;
        }
        GATES
            .get()
            .unwrap()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.profile());
    }
}
fn wave() -> Vec<u8> {
    let rate = 22050u32;
    let samples: Vec<i16> = (0..rate * 30)
        .map(|index| {
            ((index as f64 * 440.0 * std::f64::consts::TAU / rate as f64).sin() * 8192.0) as i16
        })
        .collect();
    let length = (samples.len() * 2) as u32;
    let mut bytes = b"RIFF".to_vec();
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

#[test]
fn catalog_application_restart_real_memory_presentation_and_hot_swap_keep_spatial_and_no_vault() {
    let mut case = Case::new();
    let BackendQueryResult::BundledImpulsePresets(catalog) = case
        .backend()
        .query(BackendQuery::BundledImpulsePresets)
        .unwrap()
    else {
        panic!("Wrong catalog query");
    };
    assert_eq!(catalog, bundled_impulses::presets());
    assert_eq!(catalog.len(), 13);
    case.assert_no_private_impulse_inventory();
    let spatial = SpatialSettings {
        enabled: true,
        radius: 0.7,
        speed: 31,
    };
    case.complete(
        "bundled-spatial",
        BackendCommand::SetSpatialSettings { settings: spatial },
    );
    let selected = case.select("bundled-telephone", "telephone");
    assert_eq!(selected.spatial, spatial);
    assert_eq!(case.state().audio.applied_sequence, 0);
    case.assert_no_private_impulse_inventory();
    case.restart();
    case.wait_ready();
    assert_eq!(case.state().persisted, selected);
    assert_eq!(case.state().audio.desired, selected);
    assert_eq!(case.state().audio.applied_sequence, 0);
    case.assert_no_private_impulse_inventory();

    let track_path = case.root().join("playback.wav");
    fs::write(&track_path, wave()).unwrap();
    let (track, _) = local_track_from_path(&track_path).unwrap();
    case.complete(
        "bundled-media",
        BackendCommand::ImportAudio {
            paths: vec![track_path],
        },
    );
    case.complete("bundled-play", BackendCommand::Play { id: track.id });
    let first = case.wait_presented();
    let generation = case.backend().snapshot().playback.generation;
    assert!(first.audio.owned_bytes > 0);
    let changed = case.select("bundled-four-channel", "spreader50_65ms");
    assert_eq!(changed.spatial, spatial);
    let second = case.wait_presented();
    assert!(second.audio.requested_sequence > first.audio.requested_sequence);
    assert_eq!(case.backend().snapshot().playback.generation, generation);
    assert!(second.audio.owned_bytes > 0);
    // The generic settings route also decodes an uncached original preset.
    let settings = EffectsSettings {
        convolution: bundled_impulses::preset("s2_r4_bd")
            .unwrap()
            .convolution_settings(),
        spatial,
    };
    case.complete(
        "bundled-generic",
        BackendCommand::SetEffects {
            settings: settings.clone(),
        },
    );
    let third = case.wait_presented();
    assert_eq!(third.persisted, settings);
    assert!(third.audio.requested_sequence > second.audio.requested_sequence);
    assert_eq!(case.backend().snapshot().playback.generation, generation);
    case.complete("bundled-reset", BackendCommand::ResetEffects);
    let reset = case.wait_presented();
    assert_eq!(reset.persisted, EffectsSettings::default());
    assert!(reset.audio.requested_sequence > third.audio.requested_sequence);
    case.assert_no_private_impulse_inventory();
    case.shutdown();
}

#[test]
fn published_bundled_result_waits_for_actual_join_and_cancel_cannot_apply_it() {
    let mut case = Case::new();
    let previous = case.select("bundled-cancel-baseline", "telephone");
    let previous_audio = case.state().audio;
    case.gate_operation("bundled-cancel-prepared");
    let cursor = case.backend().event_cursor();
    let receipt = case.execute(
        "bundled-cancel-prepared",
        BackendCommand::ApplyBundledImpulsePreset {
            id: "bright_hall".into(),
        },
    );
    assert_eq!(receipt.state, ReceiptState::Accepted);
    case.wait_result_while_worker_owned();
    assert_eq!(case.state().persisted, previous);
    assert_eq!(
        case.state().audio.requested_sequence,
        previous_audio.requested_sequence
    );
    assert!(!case.backend().events_after(cursor).unwrap().iter().any(|event| {
        matches!(&event.payload, BackendEvent::OperationFinished { operation_id, .. } if operation_id == "bundled-cancel-prepared")
    }));
    case.complete(
        "bundled-cancel",
        BackendCommand::CancelOperation {
            operation_id: receipt.operation_id.clone(),
        },
    );
    let (success, _) = case.outcome(&receipt.operation_id, cursor);
    assert!(!success);
    case.release_worker();
    case.shutdown();
    case.restart();
    case.wait_ready();
    assert_eq!(case.state().persisted, previous);
    assert_eq!(case.state().audio.desired, previous);
    case.assert_no_private_impulse_inventory();
}

#[test]
fn actual_store_replace_failure_preserves_settings_and_audio_then_retry_succeeds() {
    let case = Case::new();
    let previous = case.select("bundled-store-baseline", "telephone");
    let previous_audio = case.state().audio;
    let state_path = case.profile().join("player-state.json");
    let previous_bytes = fs::read(&state_path).unwrap();
    case.gate_operation("bundled-store-failed");
    let cursor = case.backend().event_cursor();
    let receipt = case.execute(
        "bundled-store-failed",
        BackendCommand::ApplyBundledImpulsePreset {
            id: "bright_hall".into(),
        },
    );
    assert_eq!(receipt.state, ReceiptState::Accepted);
    case.wait_result_while_worker_owned();
    let deny_replace = OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&state_path)
        .unwrap();
    case.release_worker();
    let (success, error) = case.outcome(&receipt.operation_id, cursor);
    assert!(!success);
    assert!(error.is_some_and(|error| error.contains("preserve existing player state")));
    let failed = case.state();
    assert_eq!(failed.persisted, previous);
    assert_eq!(failed.audio.desired, previous);
    assert_eq!(
        failed.audio.requested_sequence,
        previous_audio.requested_sequence
    );
    assert_eq!(fs::read(&state_path).unwrap(), previous_bytes);
    assert_eq!(
        ProjectStore::open(case.profile())
            .unwrap()
            .load()
            .unwrap()
            .effects
            .unwrap()
            .settings,
        previous
    );
    drop(deny_replace);
    let retried = case.select("bundled-store-retry", "bright_hall");
    assert_eq!(retried.spatial, previous.spatial);
    assert!(case.state().audio.requested_sequence > previous_audio.requested_sequence);
    case.assert_no_private_impulse_inventory();
}

#[test]
fn unknown_bundled_ids_reject_before_workers_or_durable_settings_change() {
    let case = Case::new();
    let before = case.state();
    let before_preferences = serde_json::to_value(case.preferences()).unwrap();
    let result = case.backend().execute(CommandEnvelope {
        request_id: "bundled-invalid".into(),
        origin: "bundled-impulse-test".into(),
        expected_revision: None,
        command: BackendCommand::ApplyBundledImpulsePreset {
            id: "medium_room1".into(),
        },
    });
    assert!(result.is_err());
    let result = case.backend().execute(CommandEnvelope {
        request_id: "bundled-invalid-generic".into(),
        origin: "bundled-impulse-test".into(),
        expected_revision: None,
        command: BackendCommand::SetEffects {
            settings: EffectsSettings {
                convolution: crate::domain::audio_effects::ConvolutionSettings {
                    impulse: Some(ImpulseResourceId::Bundled("unknown".into())),
                    main_gain: 1.0,
                    send_gain: 1.0,
                },
                ..Default::default()
            },
        },
    });
    assert!(result.is_err());
    assert_eq!(case.state().persisted, before.persisted);
    assert_eq!(
        case.state().audio.requested_sequence,
        before.audio.requested_sequence
    );
    assert_eq!(
        serde_json::to_value(case.preferences()).unwrap(),
        before_preferences
    );
    case.assert_no_private_impulse_inventory();
}
