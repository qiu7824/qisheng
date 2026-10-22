use super::effects_control::{
    EffectsBackendState, EffectsControl, EffectsMessage, EffectsOperationState,
    EffectsOperationView, EffectsPreferences, EffectsPreparation, EffectsResourceInput,
    EffectsVaultHost, ImpulseAbortSelection, ImpulseNamespaceSelection,
};
use super::*;
use crate::audio::{
    effects_factory::OwnedImpulse,
    impulse_decode::ImpulseDecodeContext,
    worker_owner::{TrackedWorker, WorkerKind, WorkerPermit},
};
use crate::domain::audio_effects::{EffectsSettings, ImpulseResourceId};
use crate::repository::{
    ColdImpulseRetirement, ImpulseRetirementContext, ImpulseRetirementOwner,
    ImpulseRetirementRecord, StoredImpulseResource,
};
use std::path::PathBuf;

const MAX_RETAINED_RETIREMENTS: usize = 4;
#[derive(Default)]
struct CleanupInput {
    resource: Option<StoredImpulseResource>,
    record: Option<ImpulseRetirementRecord>,
    partial: Option<crate::repository::ImpulseImportFact>,
    selection: Option<ImpulseAbortSelection>,
}
enum RetainedRetirement {
    Warm(ImpulseRetirementOwner),
    Cold(ColdImpulseRetirement),
}
enum CleanupMessage {
    Inventory(
        Box<ImpulseRetirementRecord>,
        mpsc::SyncSender<std::result::Result<(), String>>,
    ),
    Finished {
        token: String,
        owner: Option<Box<RetainedRetirement>>,
        result: Result<()>,
    },
}
struct CleanupFlight {
    context: ImpulseRetirementContext,
    receiver: mpsc::Receiver<CleanupMessage>,
    worker: TrackedWorker,
    view: EffectsOperationView,
    completed: bool,
    result: Option<Result<()>>,
}
pub(super) struct EffectsRuntime {
    control: EffectsControl,
    profile: PathBuf,
    vault: Arc<Mutex<EffectsVaultHost>>,
    impulse: Option<Arc<OwnedImpulse>>,
    cached: Option<Arc<OwnedImpulse>>,
    restore_pending: bool,
    pub(super) resource_error: Option<String>,
    cleanup: Option<CleanupFlight>,
    retained: Vec<(String, RetainedRetirement)>,
    history: VecDeque<EffectsOperationView>,
    cleanup_revision: u64,
    closing: bool,
}
impl EffectsRuntime {
    pub(super) fn new(profile: PathBuf) -> Self {
        Self {
            control: EffectsControl::new(),
            profile,
            vault: Arc::new(Mutex::new(EffectsVaultHost::default())),
            impulse: None,
            cached: None,
            restore_pending: false,
            resource_error: None,
            cleanup: None,
            retained: Vec::new(),
            history: VecDeque::new(),
            cleanup_revision: 0,
            closing: false,
        }
    }
    fn require_idle(&self) -> Result<()> {
        ensure!(
            !self.closing
                && self.cleanup.is_none()
                && !self
                    .control
                    .views()
                    .iter()
                    .any(|view| view.state == EffectsOperationState::Preparing),
            "An effect resource operation is still running"
        );
        Ok(())
    }
    fn views(&self) -> Vec<EffectsOperationView> {
        self.control
            .views()
            .into_iter()
            .chain(self.cleanup.iter().map(|flight| flight.view.clone()))
            .chain(self.history.iter().cloned())
            .collect()
    }
}
impl Runtime {
    pub(super) fn effects_preferences(&self) -> EffectsPreferences {
        self.data
            .effects
            .clone()
            .unwrap_or_else(|| EffectsPreferences::from_legacy_spatial(self.data.spatial))
    }
    pub(super) fn effects_state(&self) -> EffectsBackendState {
        let settings = self.effects_preferences().settings;
        EffectsBackendState {
            resource_ready: settings.convolution.impulse.as_ref().is_none_or(|id| {
                self.effects
                    .impulse
                    .as_ref()
                    .is_some_and(|owner| owner.source_id() == id)
            }),
            persisted: settings,
            audio: self.audio.effects_snapshot(),
            resource_preparing: self.effects.restore_pending,
            resource_error: self.effects.resource_error.clone(),
        }
    }
    pub(super) fn effects_operations(&self) -> Vec<EffectsOperationView> {
        self.effects.views()
    }
    fn effect_vault(&self, create: bool) -> Result<Arc<Mutex<EffectsVaultHost>>> {
        ensure!(
            create || self.effects_preferences().vault.is_some(),
            "No durable impulse namespace is registered"
        );
        Ok(self.effects.vault.clone())
    }
    pub(super) fn initialize_effects(&mut self) -> Result<()> {
        let settings = self.effects_preferences().settings;
        if let Some(id) = &settings.convolution.impulse {
            match id {
                ImpulseResourceId::Bundled(_) => self.effects.control.begin_bundled_settings(
                    "effects-startup".into(),
                    settings,
                    Duration::from_secs(30),
                )?,
                ImpulseResourceId::Imported(_) => {
                    let resource = self.effects_preferences().resource(id)?.clone();
                    let vault = self.effect_vault(false)?;
                    self.effects.control.begin_settings(
                        "effects-startup".into(),
                        EffectsResourceInput {
                            host: vault,
                            profile: self.effects.profile.clone(),
                            preferences: self.effects_preferences(),
                        },
                        settings,
                        resource,
                        Duration::from_secs(30),
                    )?;
                }
            }
            self.effects.restore_pending = true;
        } else {
            self.audio.set_effects(settings, None)?;
        }
        Ok(())
    }
    pub(super) fn require_effect_resource(&self) -> Result<()> {
        let state = self.effects_state();
        ensure!(
            state.resource_ready && !state.resource_preparing,
            "Selected effect resource is unavailable or preparing: {}",
            state.resource_error.unwrap_or_default()
        );
        Ok(())
    }
    pub(super) fn commit_effect_settings(&mut self, settings: EffectsSettings) -> Result<bool> {
        self.effects.require_idle()?;
        settings.validate()?;
        self.system_control.require_store_mutation()?;
        let selected = settings.convolution.impulse.as_ref();
        let owner = selected.and_then(|id| {
            self.effects
                .impulse
                .iter()
                .chain(self.effects.cached.iter())
                .find(|owner| owner.source_id() == id)
                .cloned()
        });
        if let Some(selected) = selected.filter(|_| owner.is_none()) {
            if matches!(selected, ImpulseResourceId::Bundled(_)) {
                self.effects.control.begin_bundled_settings(
                    self.request_id.clone(),
                    settings,
                    Duration::from_secs(30),
                )?;
                self.async_started = true;
                return Ok(false);
            }
            let resource = self.effects_preferences().resource(selected)?.clone();
            ensure!(
                !self
                    .effects_preferences()
                    .retirements
                    .iter()
                    .any(|record| record.resource_id() == resource.resource_id),
                "Selected impulse is pending retirement"
            );
            let vault = self.effect_vault(false)?;
            self.effects.control.begin_settings(
                self.request_id.clone(),
                EffectsResourceInput {
                    host: vault,
                    profile: self.effects.profile.clone(),
                    preferences: self.effects_preferences(),
                },
                settings,
                resource,
                Duration::from_secs(30),
            )?;
            self.async_started = true;
            return Ok(false);
        }
        self.apply_effect_settings(settings, owner)?;
        Ok(true)
    }
    pub(super) fn apply_bundled_preset(&mut self, id: String) -> Result<bool> {
        let preset = crate::audio::bundled_impulses::preset(&id)?;
        let mut settings = self.effects_preferences().settings;
        settings.convolution = preset.convolution_settings();
        self.commit_effect_settings(settings)
    }
    fn apply_effect_settings(
        &mut self,
        settings: EffectsSettings,
        owner: Option<Arc<OwnedImpulse>>,
    ) -> Result<()> {
        self.audio
            .validate_effects_admission(&settings, owner.as_ref())?;
        let mut candidate = self.data.clone();
        let mut preferences = self.effects_preferences();
        preferences.settings = settings.clone();
        candidate.spatial = settings.spatial;
        candidate.effects = Some(preferences);
        self.commit_data(candidate)?;
        self.audio.set_effects(settings, owner.clone())?;
        self.effects.impulse = owner;
        self.effects.resource_error = None;
        Ok(())
    }
    pub(super) fn import_impulse(
        &mut self,
        path: PathBuf,
        approved: bool,
        timeout_ms: Option<u64>,
    ) -> Result<()> {
        ensure!(
            approved,
            "Impulse import requires explicit file-read approval"
        );
        self.effects.require_idle()?;
        self.system_control.require_store_mutation()?;
        crate::domain::library::validate_library_path(&path)?;
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .context("Impulse format is missing")?
            .to_ascii_lowercase();
        ensure!(
            matches!(extension.as_str(), "wav" | "flac"),
            "Impulse import supports WAV and FLAC only"
        );
        let timeout = effect_timeout(timeout_ms)?;
        let vault = self.effect_vault(true)?;
        self.effects.control.begin_import(
            self.request_id.clone(),
            EffectsResourceInput {
                host: vault,
                profile: self.effects.profile.clone(),
                preferences: self.effects_preferences(),
            },
            self.library.authority.clone(),
            path,
            extension,
            timeout,
        )?;
        self.async_started = true;
        Ok(())
    }
    pub(super) fn recover_impulse_import(
        &mut self,
        token: String,
        timeout_ms: Option<u64>,
    ) -> Result<()> {
        self.effects.require_idle()?;
        self.system_control.require_store_mutation()?;
        let fact = self
            .effects_preferences()
            .imports
            .into_iter()
            .find(|fact| fact.token == token)
            .context("Impulse import inventory is unknown")?;
        let vault = self.effect_vault(false)?;
        self.effects.control.begin_recovery(
            self.request_id.clone(),
            vault,
            self.effects.profile.clone(),
            self.effects_preferences(),
            fact,
            effect_timeout(timeout_ms)?,
        )?;
        self.async_started = true;
        Ok(())
    }
    pub(super) fn recover_impulse_namespace(
        &mut self,
        selection: ImpulseNamespaceSelection,
        timeout_ms: Option<u64>,
    ) -> Result<()> {
        selection.validate()?;
        self.effects.require_idle()?;
        self.system_control.require_store_mutation()?;
        let preferences = self.effects_preferences();
        ensure!(
            preferences.creation.is_some() && preferences.vault.is_none(),
            "No unfinished current impulse namespace creation obligation exists"
        );
        self.effects.control.begin_namespace_recovery(
            self.request_id.clone(),
            EffectsResourceInput {
                host: self.effects.vault.clone(),
                profile: self.effects.profile.clone(),
                preferences,
            },
            self.library.authority.clone(),
            selection,
            effect_timeout(timeout_ms)?,
        )?;
        self.async_started = true;
        Ok(())
    }
    pub(super) fn cancel_effect_operation(&mut self, operation: &str) -> bool {
        let cancelled = self.effects.control.cancel(operation);
        if cancelled {
            if operation == "effects-startup" {
                self.effects.restore_pending = false;
            }
            self.publish_finished(
                operation.into(),
                &Err(BackendError::new(ErrorKind::Cancelled, "Effect operation cancelled").into()),
            );
        }
        if let Some(flight) = &mut self.effects.cleanup
            && flight.view.operation_id == operation
        {
            flight.context.cancel();
            flight.view.state = EffectsOperationState::Cancelled;
            return true;
        }
        cancelled
    }
    pub(super) fn poll_effects(&mut self) {
        #[cfg(all(test, windows))]
        if abort_actor_tests::take_store_repair(&self.effects.profile) {
            self.system_control.test_set_store_frozen(false);
        }
        let messages = self.effects.control.poll();
        for message in messages {
            match message {
                EffectsMessage::Creation {
                    operation,
                    revision,
                    fact,
                    reply,
                } => {
                    let result = (|| -> Result<()> {
                        ensure!(
                            self.effects.control.current(&operation, revision),
                            "Namespace creation is stale or cancelled"
                        );
                        fact.validate()?;
                        #[cfg(all(test, windows))]
                        if namespace_actor_tests::reject_creation(&self.effects.profile, &fact) {
                            anyhow::bail!("Injected namespace actor creation ACK failure");
                        }
                        let mut candidate = self.data.clone();
                        let mut preferences = self.effects_preferences();
                        if let Some(previous) = &preferences.creation {
                            ensure!(
                                previous.namespace == fact.namespace
                                    && previous.profile_identity == fact.profile_identity
                                    && previous
                                        .directory_identity
                                        .as_ref()
                                        .is_none_or(
                                            |id| Some(id) == fact.directory_identity.as_ref()
                                        )
                                    && previous
                                        .owner_identity
                                        .as_ref()
                                        .is_none_or(|id| Some(id) == fact.owner_identity.as_ref()),
                                "Namespace creation ownership changed"
                            );
                        }
                        preferences.creation = Some(fact);
                        candidate.effects = Some(preferences);
                        self.commit_data(candidate)
                    })();
                    let _ = reply.try_send(result.map_err(|error| format!("{error:#}")));
                }
                EffectsMessage::Namespace {
                    operation,
                    revision,
                    receipt,
                    reply,
                } => {
                    let result = (|| -> Result<()> {
                        ensure!(
                            self.effects.control.current(&operation, revision),
                            "Namespace acknowledgement is stale or cancelled"
                        );
                        let mut candidate = self.data.clone();
                        let mut preferences = self.effects_preferences();
                        let creation = preferences
                            .creation
                            .as_ref()
                            .context("Namespace has no durable creation inventory")?;
                        ensure!(
                            creation.phase == crate::repository::ImpulseVaultCreationPhase::Ready
                                && creation.namespace == receipt.namespace
                                && creation.profile_identity == receipt.profile_identity
                                && creation.directory_identity.as_ref()
                                    == Some(&receipt.directory_identity)
                                && creation.owner_identity.as_ref()
                                    == Some(&receipt.owner_identity),
                            "Namespace receipt differs from its actual allocation"
                        );
                        preferences.vault = Some(receipt);
                        preferences.creation = None;
                        candidate.effects = Some(preferences);
                        self.commit_data(candidate)
                    })();
                    let _ = reply.try_send(result.map_err(|error| format!("{error:#}")));
                }
                EffectsMessage::Inventory {
                    operation,
                    revision,
                    fact,
                    reply,
                } => {
                    let result = (|| -> Result<()> {
                        ensure!(
                            self.effects.control.current(&operation, revision),
                            "Effect inventory is stale or cancelled"
                        );
                        let mut candidate = self.data.clone();
                        let mut preferences = self.effects_preferences();
                        preferences.record_import(fact)?;
                        candidate.effects = Some(preferences);
                        self.commit_data(candidate)
                    })();
                    let _ = reply.try_send(result.map_err(|error| format!("{error:#}")));
                }
                EffectsMessage::Prepared {
                    operation,
                    revision,
                    result,
                } => {
                    if !self.effects.control.current(&operation, revision) {
                        continue;
                    }
                    let mut selected = None;
                    let result = result.and_then(|prepared| {
                        match prepared {
                            EffectsPreparation::Imported { resource, impulse } => {
                                selected = Some(resource.resource_id.clone());
                                let mut candidate = self.data.clone();
                                let mut preferences = self.effects_preferences();
                                preferences.register_resource(resource)?;
                                candidate.effects = Some(preferences);
                                self.commit_data(candidate)?;
                                self.effects.cached = Some(impulse);
                            }
                            EffectsPreparation::Settings(change) => {
                                selected = change.settings.convolution.impulse.clone();
                                if operation == "effects-startup" {
                                    self.audio
                                        .set_effects(change.settings, change.impulse.clone())?;
                                    self.effects.impulse = change.impulse;
                                } else {
                                    self.apply_effect_settings(change.settings, change.impulse)?;
                                }
                            }
                            EffectsPreparation::NamespaceRecovered => {}
                        }
                        Ok(())
                    });
                    if operation == "effects-startup" {
                        self.effects.restore_pending = false;
                    }
                    self.effects.resource_error =
                        result.as_ref().err().map(|error| format!("{error:#}"));
                    let _ = self.effects.control.finish(
                        &operation,
                        revision,
                        selected,
                        result.as_ref().err().map(|error| format!("{error:#}")),
                    );
                    self.publish_finished(operation, &result);
                }
            }
        }
        self.poll_impulse_cleanup();
    }
    pub(super) fn retire_impulse_resource(&mut self, id: ImpulseResourceId) -> Result<()> {
        self.effects.require_idle()?;
        self.system_control.require_store_mutation()?;
        ensure!(
            self.effects_preferences()
                .settings
                .convolution
                .impulse
                .as_ref()
                != Some(&id),
            "Select another effect resource or reset effects before retirement"
        );
        let resource = self.effects_preferences().resource(&id)?.clone();
        let vault = self.effect_vault(false)?;
        self.begin_impulse_cleanup(
            vault,
            CleanupInput {
                resource: Some(resource),
                ..Default::default()
            },
            None,
        )
    }
    pub(super) fn abort_impulse_import(
        &mut self,
        token: String,
        selection: Option<ImpulseAbortSelection>,
    ) -> Result<()> {
        self.effects.require_idle()?;
        self.system_control.require_store_mutation()?;
        if let Some(selection) = &selection {
            selection.validate()?;
        }
        let preferences = self.effects_preferences();
        let partial = preferences
            .imports
            .iter()
            .find(|fact| fact.token == token)
            .cloned()
            .context("Impulse import inventory is unknown")?;
        let record = preferences
            .retirements
            .iter()
            .find(|record| record.token == token)
            .cloned();
        let retained = self
            .effects
            .retained
            .iter()
            .position(|(retained, _)| retained == &token)
            .map(|index| self.effects.retained.remove(index).1);
        self.begin_impulse_cleanup(
            self.effects.vault.clone(),
            CleanupInput {
                partial: Some(partial),
                record,
                selection,
                resource: None,
            },
            retained,
        )
    }
    pub(super) fn recover_impulse_retirement(&mut self, token: String) -> Result<()> {
        self.effects.require_idle()?;
        self.system_control.require_store_mutation()?;
        let record = self
            .effects_preferences()
            .retirements
            .into_iter()
            .find(|record| record.token == token)
            .context("Impulse retirement inventory is unknown")?;
        let vault = self.effect_vault(false)?;
        let retained = self
            .effects
            .retained
            .iter()
            .position(|(retained, _)| retained == &token)
            .map(|index| self.effects.retained.remove(index).1);
        let resource = self
            .effects_preferences()
            .resources
            .into_iter()
            .find(|resource| resource.resource_id == record.resource_id());
        let partial = self
            .effects_preferences()
            .imports
            .into_iter()
            .find(|fact| fact.token == token);
        self.begin_impulse_cleanup(
            vault,
            CleanupInput {
                resource,
                record: Some(record),
                partial,
                selection: None,
            },
            retained,
        )
    }
    fn begin_impulse_cleanup(
        &mut self,
        vault: Arc<Mutex<EffectsVaultHost>>,
        input: CleanupInput,
        retained: Option<RetainedRetirement>,
    ) -> Result<()> {
        let retained_cell = Arc::new(Mutex::new(retained));
        let CleanupInput {
            resource,
            record,
            partial,
            selection,
        } = input;
        let restore_token = resource
            .as_ref()
            .map(|resource| match &resource.resource_id {
                ImpulseResourceId::Imported(id) => id.clone(),
                _ => String::new(),
            })
            .or_else(|| record.as_ref().map(|record| record.token.clone()))
            .or_else(|| partial.as_ref().map(|fact| fact.token.clone()))
            .unwrap_or_default();
        let admission = (|| -> Result<CleanupFlight> {
            ensure!(
                self.effects.retained.len() < MAX_RETAINED_RETIREMENTS,
                "Native impulse owners are still retiring"
            );
            let token = resource
                .as_ref()
                .map(|resource| match &resource.resource_id {
                    ImpulseResourceId::Imported(id) => id.clone(),
                    _ => String::new(),
                })
                .or_else(|| record.as_ref().map(|record| record.token.clone()))
                .or_else(|| partial.as_ref().map(|fact| fact.token.clone()))
                .context("Impulse retirement identity is missing")?;
            let context = ImpulseRetirementContext::new(Duration::from_secs(30))?;
            let owned_context = context.clone();
            let operation = self.request_id.clone();
            let permit = WorkerPermit::reserve(WorkerKind::Library)?;
            let (sender, receiver) = mpsc::sync_channel(2);
            let revision = self
                .effects
                .cleanup_revision
                .checked_add(1)
                .context("Impulse cleanup revision exhausted")?;
            let worker_retained = retained_cell.clone();
            let profile = self.effects.profile.clone();
            let preferences = self.effects_preferences();
            let authority = self.library.authority.clone();
            let quiescing = self.effects.closing;
            let worker = std::thread::Builder::new()
                .name("lx-impulse-retirement".into())
                .spawn(move || {
                    let mut owner = worker_retained
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .take();
                    let result = (|| -> Result<()> {
                        let journal = |record: &ImpulseRetirementRecord| -> Result<()> {
                            let (reply, acknowledgement) = mpsc::sync_channel(1);
                            sender
                                .send(CleanupMessage::Inventory(Box::new(record.clone()), reply))
                                .map_err(|_| anyhow::anyhow!("Impulse cleanup actor stopped"))?;
                            loop {
                                owned_context.check()?;
                                match acknowledgement.recv_timeout(Duration::from_millis(10)) {
                                    Ok(Ok(())) => return Ok(()),
                                    Ok(Err(error)) => anyhow::bail!(error),
                                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                                        anyhow::bail!("Impulse cleanup actor stopped")
                                    }
                                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                                }
                            }
                        };
                        if owner.is_none() {
                            let mut host = vault
                                .lock()
                                .map_err(|_| anyhow::anyhow!("Impulse namespace was poisoned"))?;
                            let vault = host.existing(&profile, &preferences)?;
                            if let Some(record) =
                                record.as_ref().filter(|record| record.pending_zero_links())
                            {
                                owner = Some(RetainedRetirement::Cold(
                                    vault.prepare_cold_retirement(record)?,
                                ));
                            } else if let Some(fact) = &partial {
                                let selected = selection
                                    .as_ref()
                                    .map(|selection| {
                                        selection.validate()?;
                                        authority.open_selection(&selection.path)
                                    })
                                    .transpose()?;
                                owner =
                                    Some(RetainedRetirement::Warm(vault.prepare_import_abort(
                                        fact,
                                        selected,
                                        &ImpulseDecodeContext::new(Duration::from_secs(30))?,
                                        &owned_context,
                                    )?));
                            } else {
                                owner = Some(RetainedRetirement::Warm(
                                    vault.prepare_retirement(
                                        resource
                                            .as_ref()
                                            .context("Original impulse resource is unavailable")?,
                                        &ImpulseDecodeContext::new(Duration::from_secs(30))?,
                                        &owned_context,
                                    )?,
                                ));
                            }
                        }
                        match owner.as_mut().unwrap() {
                            RetainedRetirement::Warm(owner) => {
                                owner.attempt(&owned_context, &journal)?;
                            }
                            RetainedRetirement::Cold(owner) => {
                                journal(&owner.confirm(&owned_context)?)?;
                            }
                        }
                        Ok(())
                    })();
                    if result.is_ok() {
                        owner = None;
                    } else if quiescing {
                        let released = match owner.as_mut() {
                            Some(RetainedRetirement::Warm(owner)) => {
                                owner.release_for_recovery().unwrap_or(false)
                            }
                            Some(RetainedRetirement::Cold(owner)) => {
                                owner.can_release_for_recovery()
                            }
                            None => true,
                        };
                        if released {
                            owner = None;
                        }
                    }
                    let _ = sender.send(CleanupMessage::Finished {
                        token,
                        owner: owner.map(Box::new),
                        result,
                    });
                    #[cfg(all(test, windows))]
                    abort_actor_tests::after_cleanup_result(&profile);
                })?;
            Ok(CleanupFlight {
                context,
                receiver,
                worker: permit.attach(worker),
                completed: false,
                result: None,
                view: EffectsOperationView {
                    operation_id: operation,
                    revision,
                    state: EffectsOperationState::Preparing,
                    resource_id: None,
                    error: None,
                },
            })
        })();
        match admission {
            Ok(flight) => {
                self.effects.cleanup_revision = flight.view.revision;
                self.effects.cleanup = Some(flight);
                self.async_started = true;
                Ok(())
            }
            Err(error) => {
                if let Some(owner) = retained_cell
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take()
                {
                    self.effects.retained.push((restore_token, owner));
                }
                Err(error)
            }
        }
    }
    fn poll_impulse_cleanup(&mut self) {
        let mut messages = Vec::new();
        if let Some(flight) = &self.effects.cleanup {
            while let Ok(message) = flight.receiver.try_recv() {
                messages.push(message);
            }
        }
        for message in messages {
            match message {
                CleanupMessage::Inventory(record, reply) => {
                    let record = *record;
                    let result = (|| -> Result<()> {
                        #[cfg(all(test, windows))]
                        match abort_actor_tests::retirement_ack(&self.effects.profile, &record) {
                            abort_actor_tests::AckAction::Continue => {}
                            abort_actor_tests::AckAction::Reject => {
                                anyhow::bail!("Injected actor retirement ACK failure")
                            }
                            abort_actor_tests::AckAction::Freeze => {
                                self.system_control.test_set_store_frozen(true)
                            }
                        }
                        let mut candidate = self.data.clone();
                        let mut preferences = self.effects_preferences();
                        record.validate()?;
                        ensure!(
                            preferences
                                .vault
                                .as_ref()
                                .is_some_and(|vault| vault.namespace == record.namespace),
                            "Impulse cleanup namespace changed"
                        );
                        if let Some(fact) = preferences
                            .imports
                            .iter_mut()
                            .find(|fact| fact.token == record.token)
                        {
                            let actual = record.body_identity();
                            ensure!(
                                fact.namespace == record.namespace
                                    && fact.body_identity.as_ref().is_none_or(|id| id == &actual),
                                "Impulse abort retirement changed its original allocation"
                            );
                            fact.body_identity = Some(actual);
                            fact.phase = if record.removed() {
                                crate::repository::ImpulseImportPhase::Removed
                            } else {
                                crate::repository::ImpulseImportPhase::Aborting
                            };
                        }
                        if let Some(existing) = preferences
                            .retirements
                            .iter_mut()
                            .find(|existing| existing.token == record.token)
                        {
                            *existing = record;
                        } else {
                            preferences.retirements.push(record);
                        }
                        candidate.effects = Some(preferences);
                        self.commit_data(candidate)
                    })();
                    let _ = reply.try_send(result.map_err(|error| format!("{error:#}")));
                }
                CleanupMessage::Finished {
                    token,
                    owner,
                    result,
                } => {
                    if let Some(owner) = owner {
                        self.effects.retained.push((token.clone(), *owner));
                    }
                    if let Some(flight) = &mut self.effects.cleanup {
                        flight.completed = true;
                        flight.view.resource_id = Some(ImpulseResourceId::Imported(token));
                        flight.view.error = result.as_ref().err().map(|error| format!("{error:#}"));
                        flight.result = Some(result);
                    }
                }
            }
        }
        if self
            .effects
            .cleanup
            .as_ref()
            .is_some_and(|flight| flight.completed && flight.worker.wait(Duration::ZERO))
        {
            let mut flight = self.effects.cleanup.take().unwrap();
            let mut result = flight.result.take().unwrap_or_else(|| {
                Err(anyhow::anyhow!("Impulse cleanup ended without an outcome"))
            });
            let mut candidate = self.data.clone();
            let mut preferences = self.effects_preferences();
            let removed: Vec<_> = preferences
                .retirements
                .iter()
                .filter(|record| record.removed())
                .map(|record| record.token.clone())
                .collect();
            for token in &removed {
                preferences.resources.retain(|resource| {
                    resource.resource_id != ImpulseResourceId::Imported(token.clone())
                });
                preferences.imports.retain(|fact| &fact.token != token);
            }
            preferences.retirements.retain(|record| !record.removed());
            candidate.effects = Some(preferences);
            if let Err(error) = self.commit_data(candidate) {
                self.error(format!("Impulse retirement compaction failed: {error:#}"));
                result = Err(error);
            } else {
                let release = (|| -> Result<()> {
                    let mut host = self
                        .effects
                        .vault
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Impulse ownership was poisoned"))?;
                    let vault = host
                        .vault
                        .as_mut()
                        .context("Original impulse reservation host is missing")?;
                    for token in removed {
                        vault.release_import_reservation(&token)?;
                    }
                    Ok(())
                })();
                if let Err(error) = release {
                    self.effects.resource_error = Some(format!("{error:#}"));
                    result = Err(error);
                }
            }
            if flight.view.state != EffectsOperationState::Cancelled {
                flight.view.state = if result.is_ok() {
                    EffectsOperationState::Prepared
                } else {
                    EffectsOperationState::Failed
                };
            }
            flight.view.error = result.as_ref().err().map(|error| format!("{error:#}"));
            self.publish_finished(flight.view.operation_id.clone(), &result);
            self.effects.history.push_back(flight.view);
            if self.effects.history.len() > 64 {
                self.effects.history.pop_front();
            }
        }
    }
    pub(super) fn begin_effect_shutdown(&mut self) {
        self.effects.closing = true;
        self.effects.control.begin_shutdown();
        if let Some(flight) = &self.effects.cleanup {
            flight.context.cancel();
        }
    }
    pub(super) fn poll_effect_quiesce(&mut self) -> Result<bool> {
        self.poll_effects();
        let joined =
            self.effects.control.shutdown(Duration::ZERO) && self.effects.cleanup.is_none();
        if self
            .effects
            .cleanup
            .as_ref()
            .is_some_and(|flight| flight.worker.panicked())
        {
            anyhow::bail!("Impulse cleanup worker panicked with native ownership unconfirmed");
        }
        if !joined {
            return Ok(false);
        }
        if let Some((token, owner)) = self.effects.retained.pop() {
            let record = self
                .effects_preferences()
                .retirements
                .into_iter()
                .find(|record| record.token == token);
            let resource = self
                .effects_preferences()
                .resources
                .into_iter()
                .find(|resource| {
                    resource.resource_id == ImpulseResourceId::Imported(token.clone())
                });
            let partial = self
                .effects_preferences()
                .imports
                .into_iter()
                .find(|fact| fact.token == token);
            self.begin_impulse_cleanup(
                self.effects.vault.clone(),
                CleanupInput {
                    resource,
                    record,
                    partial,
                    selection: None,
                },
                Some(owner),
            )?;
            return Ok(false);
        }
        self.effects.retained.clear();
        self.effects.impulse = None;
        self.effects.cached = None;
        self.effects
            .vault
            .lock()
            .map_err(|_| anyhow::anyhow!("Impulse namespace was poisoned"))?
            .vault = None;
        Ok(true)
    }
}
fn effect_timeout(value: Option<u64>) -> Result<Duration> {
    let value = value.unwrap_or(30_000);
    ensure!(
        (1..=120_000).contains(&value),
        "Impulse timeout must be 1..120000 ms"
    );
    Ok(Duration::from_millis(value))
}

#[cfg(all(test, windows))]
pub(super) mod abort_actor_tests;
#[cfg(all(test, windows))]
pub(super) mod bundled_actor_tests;
#[cfg(all(test, windows))]
pub(super) mod namespace_actor_tests;
#[cfg(test)]
mod tests;
