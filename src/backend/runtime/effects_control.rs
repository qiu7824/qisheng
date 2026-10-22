//! Actor-owned effect preferences and bounded resource preparation flights.
//!
//! Durable settings describe intent. Only AudioPlayer's presented sequence
//! reports graph application; a saved preference or prepared resource cannot
//! advance that sequence. Workers retain cancellation, descriptors and results
//! until their registered owner has actually joined.

use crate::{
    audio::{
        effects_factory::OwnedImpulse,
        impulse_decode::ImpulseDecodeContext,
        worker_owner::{TrackedWorker, WorkerKind, WorkerPermit},
    },
    domain::audio_effects::{EffectsSettings, ImpulseResourceId, SpatialSettings},
    repository::{
        ImpulseImportFact, ImpulseRetirementRecord, ImpulseVault, ImpulseVaultReceipt,
        MAX_STORED_IMPULSES, StoredImpulseResource,
    },
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashSet, VecDeque},
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

const MAX_IMPORT_FACTS: usize = 64;
const MAX_RETIRED_FLIGHTS: usize = 4;
const MAX_OPERATION_HISTORY: usize = 64;

#[derive(Clone, Debug, Serialize)]
pub struct EffectsBackendState {
    pub persisted: EffectsSettings,
    pub audio: crate::domain::audio_effects::EffectsSnapshot,
    pub resource_preparing: bool,
    pub resource_ready: bool,
    pub resource_error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImpulseAbortSelection {
    pub path: std::path::PathBuf,
    pub file_read_approved: bool,
    pub file_write_approved: bool,
}
impl ImpulseAbortSelection {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.file_read_approved && self.file_write_approved,
            "Cold impulse abort requires explicit selected-file read and write approval"
        );
        crate::domain::library::validate_library_path(&self.path)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImpulseNamespaceSelection {
    pub directory_path: std::path::PathBuf,
    pub directory_read_approved: bool,
    pub directory_write_approved: bool,
    pub owner: Option<ImpulseAbortSelection>,
}
impl ImpulseNamespaceSelection {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.directory_read_approved && self.directory_write_approved,
            "Impulse namespace recovery requires explicit directory read and write approval"
        );
        crate::domain::library::validate_library_path(&self.directory_path)?;
        if let Some(owner) = &self.owner {
            owner.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EffectsPreferences {
    pub settings: EffectsSettings,
    pub vault: Option<ImpulseVaultReceipt>,
    pub resources: Vec<StoredImpulseResource>,
    pub imports: Vec<ImpulseImportFact>,
    pub retirements: Vec<ImpulseRetirementRecord>,
    pub creation: Option<crate::repository::ImpulseVaultCreationFact>,
}
impl EffectsPreferences {
    pub fn from_legacy_spatial(spatial: SpatialSettings) -> Self {
        Self {
            settings: EffectsSettings {
                spatial,
                ..Default::default()
            },
            ..Default::default()
        }
    }
    pub fn validate(&self) -> Result<()> {
        self.settings.validate()?;
        if let Some(creation) = &self.creation {
            creation.validate()?;
        }
        ensure!(
            self.resources.len() <= MAX_STORED_IMPULSES
                && self.imports.len() <= MAX_IMPORT_FACTS
                && self.retirements.len() <= MAX_STORED_IMPULSES,
            "Impulse resource inventory exceeds its quota"
        );
        if let Some(vault) = &self.vault {
            vault.validate()?;
        }
        ensure!(
            self.vault.is_some()
                || (self.resources.is_empty()
                    && self.imports.is_empty()
                    && self.retirements.is_empty()),
            "Impulse inventory lacks its namespace ownership record"
        );
        let mut identities = HashSet::new();
        for resource in &self.resources {
            resource.validate()?;
            ensure!(
                self.vault
                    .as_ref()
                    .is_some_and(|vault| vault.namespace == resource.namespace),
                "Impulse representation belongs to a different persisted namespace"
            );
            let key = serde_json::to_string(&resource.resource_id)?;
            ensure!(
                identities.insert(key),
                "Duplicate imported impulse identity"
            );
        }
        let mut tokens = HashSet::new();
        for fact in &self.imports {
            fact.validate()?;
            ensure!(
                self.vault
                    .as_ref()
                    .is_some_and(|vault| vault.namespace == fact.namespace),
                "Impulse allocation belongs to a different persisted namespace"
            );
            ensure!(
                tokens.insert(&fact.token),
                "Duplicate impulse allocation inventory"
            );
        }
        let mut retirements = HashSet::new();
        for record in &self.retirements {
            record.validate()?;
            ensure!(
                self.vault
                    .as_ref()
                    .is_some_and(|vault| vault.namespace == record.namespace)
                    && retirements.insert(&record.token),
                "Invalid or duplicate impulse retirement inventory"
            );
        }
        if let Some(id) = &self.settings.convolution.impulse {
            match id {
                ImpulseResourceId::Bundled(id) => {
                    crate::audio::bundled_impulses::preset(id)?;
                }
                ImpulseResourceId::Imported(_) => {
                    self.resource(id)?;
                }
            }
        }
        Ok(())
    }
    pub fn resource(&self, id: &ImpulseResourceId) -> Result<&StoredImpulseResource> {
        id.validate()?;
        self.resources
            .iter()
            .find(|resource| &resource.resource_id == id)
            .context("Selected impulse is not a registered durable resource")
    }
    /// The actor saves this candidate and acknowledges the inventory message
    /// before the worker creates the object or starts its first body write.
    pub fn record_import(&mut self, fact: ImpulseImportFact) -> Result<()> {
        fact.validate()?;
        ensure!(
            self.vault
                .as_ref()
                .is_some_and(|vault| vault.namespace == fact.namespace),
            "Impulse import is not owned by this persisted namespace"
        );
        if let Some(previous) = self
            .imports
            .iter_mut()
            .find(|entry| entry.token == fact.token)
        {
            ensure!(
                previous.namespace == fact.namespace
                    && previous.expected_sha256 == fact.expected_sha256
                    && previous.reserved_bytes == fact.reserved_bytes
                    && previous
                        .body_identity
                        .as_ref()
                        .is_none_or(|identity| Some(identity) == fact.body_identity.as_ref())
                    && phase_rank(fact.phase) >= phase_rank(previous.phase)
                    && phase_rank(fact.phase) <= phase_rank(previous.phase) + 1,
                "Impulse import inventory changed ownership or regressed"
            );
            *previous = fact;
        } else {
            ensure!(
                self.imports.len() < MAX_IMPORT_FACTS,
                "Impulse import inventory is full"
            );
            self.imports.push(fact);
        }
        self.validate()
    }
    /// Resource admission and its final inventory removal are one store write.
    pub fn register_resource(&mut self, resource: StoredImpulseResource) -> Result<()> {
        resource.validate()?;
        let ImpulseResourceId::Imported(token) = &resource.resource_id else {
            anyhow::bail!("Only actual imported representations can enter the impulse vault");
        };
        let token = token.clone();
        let fact = self
            .imports
            .iter()
            .find(|fact| fact.token == token)
            .context("Imported impulse lacks its durable producer inventory")?;
        ensure!(
            fact.phase == crate::repository::ImpulseImportPhase::Sealed
                && fact.namespace == resource.namespace
                && fact.expected_sha256 == resource.encoded_sha256
                && fact.reserved_bytes == resource.storage_bytes()
                && fact.body_identity.as_ref() == Some(&resource.body_identity),
            "Imported impulse does not match its sealed actual allocation"
        );
        if let Some(existing) = self
            .resources
            .iter()
            .find(|existing| existing.resource_id == resource.resource_id)
        {
            ensure!(
                existing == &resource,
                "Imported impulse identity was reused for another representation"
            );
        } else {
            ensure!(
                self.resources.len() < MAX_STORED_IMPULSES,
                "Stored impulse resource quota is full"
            );
            self.resources.push(resource);
        }
        self.imports.retain(|fact| fact.token != token);
        self.validate()
    }
}
fn phase_rank(phase: crate::repository::ImpulseImportPhase) -> u8 {
    match phase {
        crate::repository::ImpulseImportPhase::Allocating => 0,
        crate::repository::ImpulseImportPhase::Writing => 1,
        crate::repository::ImpulseImportPhase::Sealed => 2,
        crate::repository::ImpulseImportPhase::Aborting => 3,
        crate::repository::ImpulseImportPhase::Removed => 4,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectsOperationState {
    Preparing,
    Prepared,
    Cancelled,
    Failed,
}
#[derive(Clone, Debug, Serialize)]
pub struct EffectsOperationView {
    pub operation_id: String,
    pub revision: u64,
    pub state: EffectsOperationState,
    pub resource_id: Option<ImpulseResourceId>,
    pub error: Option<String>,
}

pub(super) struct PreparedEffectsChange {
    pub settings: EffectsSettings,
    pub impulse: Option<Arc<OwnedImpulse>>,
}
pub(super) enum EffectsPreparation {
    Imported {
        resource: StoredImpulseResource,
        impulse: Arc<OwnedImpulse>,
    },
    Settings(PreparedEffectsChange),
    NamespaceRecovered,
}
pub(super) enum EffectsMessage {
    Creation {
        operation: String,
        revision: u64,
        fact: crate::repository::ImpulseVaultCreationFact,
        reply: mpsc::SyncSender<std::result::Result<(), String>>,
    },
    Namespace {
        operation: String,
        revision: u64,
        receipt: ImpulseVaultReceipt,
        reply: mpsc::SyncSender<std::result::Result<(), String>>,
    },
    Inventory {
        operation: String,
        revision: u64,
        fact: ImpulseImportFact,
        reply: mpsc::SyncSender<std::result::Result<(), String>>,
    },
    Prepared {
        operation: String,
        revision: u64,
        result: Result<EffectsPreparation>,
    },
}
#[derive(Default)]
pub(super) struct EffectsVaultHost {
    pub(super) vault: Option<ImpulseVault>,
    creator: Option<crate::repository::ImpulseVaultCreator>,
}
pub(super) struct EffectsResourceInput {
    pub(super) host: Arc<Mutex<EffectsVaultHost>>,
    pub(super) profile: std::path::PathBuf,
    pub(super) preferences: EffectsPreferences,
}
impl EffectsVaultHost {
    pub(super) fn existing(
        &mut self,
        profile: &std::path::Path,
        preferences: &EffectsPreferences,
    ) -> Result<&mut ImpulseVault> {
        if self.vault.is_none()
            && let Some(creator) = &mut self.creator
        {
            let receipt = preferences
                .vault
                .as_ref()
                .context("Namespace receipt is not durable")?;
            ensure!(
                &creator.receipt()? == receipt,
                "Retained namespace differs from its durable receipt"
            );
            self.vault = Some(creator.take_vault()?);
            self.creator = None;
        }
        if self.vault.is_none() {
            let profile = crate::repository::ImpulseProfileDirectory::pin_profile(profile, 1)?;
            self.vault = Some(ImpulseVault::reopen(
                &profile,
                preferences
                    .vault
                    .as_ref()
                    .context("No durable impulse namespace is registered")?,
            )?);
        }
        let vault = self.vault.as_mut().unwrap();
        vault.arm(
            preferences
                .vault
                .as_ref()
                .context("Namespace receipt is not durable")?,
        )?;
        Ok(vault)
    }
    fn prepare(
        &mut self,
        profile: &std::path::Path,
        preferences: &EffectsPreferences,
        context: &ImpulseDecodeContext,
        sender: &mpsc::SyncSender<EffectsMessage>,
        operation: &str,
        revision: u64,
    ) -> Result<&mut ImpulseVault> {
        if self.vault.is_some() || preferences.vault.is_some() {
            return self.existing(profile, preferences);
        }
        let profile = crate::repository::ImpulseProfileDirectory::pin_profile(profile, 1)?;
        if self.creator.is_none() {
            self.creator = Some(if let Some(fact) = &preferences.creation {
                crate::repository::ImpulseVaultCreator::resume(&profile, fact)?
            } else {
                crate::repository::ImpulseVaultCreator::prepare(&profile)?
            });
        }
        let creator = self.creator.as_mut().unwrap();
        creator.advance(context, &mut |fact| {
            let (reply, receiver) = mpsc::sync_channel(1);
            sender
                .send(EffectsMessage::Creation {
                    operation: operation.into(),
                    revision,
                    fact: fact.clone(),
                    reply,
                })
                .map_err(|_| anyhow::anyhow!("Effect namespace actor stopped"))?;
            acknowledge(receiver, context)
        })?;
        let receipt = creator.receipt()?;
        let (reply, receiver) = mpsc::sync_channel(1);
        sender
            .send(EffectsMessage::Namespace {
                operation: operation.into(),
                revision,
                receipt: receipt.clone(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("Effect namespace actor stopped"))?;
        acknowledge(receiver, context)?;
        let mut vault = creator.take_vault()?;
        vault.arm(&receipt)?;
        self.vault = Some(vault);
        self.creator = None;
        Ok(self.vault.as_mut().unwrap())
    }
    fn recover_namespace(
        &mut self,
        input: &EffectsResourceInput,
        selection: &ImpulseNamespaceSelection,
        authority: &crate::library::authority::AuthorityRegistry,
        context: &ImpulseDecodeContext,
    ) -> Result<()> {
        selection.validate()?;
        context.check()?;
        ensure!(
            self.vault.is_none() && input.preferences.vault.is_none(),
            "Impulse namespace already has a durable receipt"
        );
        let fact = input
            .preferences
            .creation
            .as_ref()
            .context("No current impulse namespace creation obligation exists")?;
        let profile = crate::repository::ImpulseProfileDirectory::pin_profile(&input.profile, 1)?;
        if self.creator.is_none() {
            self.creator = Some(crate::repository::ImpulseVaultCreator::resume(
                &profile, fact,
            )?);
        }
        let selected_directory = crate::repository::ImpulseProfileDirectory::open_selected(
            &selection.directory_path,
            1,
        )?;
        let creator = self.creator.as_mut().unwrap();
        let selected_owner = if creator.has_retained_owner() {
            ensure!(
                selection.owner.is_none(),
                "Retained namespace owner recovery does not accept another read selection"
            );
            None
        } else {
            selection
                .owner
                .as_ref()
                .map(|owner| authority.open_selection(&owner.path))
                .transpose()?
        };
        creator.reapprove_namespace(selected_directory, selected_owner, context)
    }
}
fn acknowledge(
    receiver: mpsc::Receiver<std::result::Result<(), String>>,
    context: &ImpulseDecodeContext,
) -> Result<()> {
    loop {
        context.check()?;
        match receiver.recv_timeout(Duration::from_millis(10)) {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) => anyhow::bail!(error),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("Effect inventory actor stopped")
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}
struct EffectsFlight {
    revision: u64,
    context: ImpulseDecodeContext,
    receiver: mpsc::Receiver<EffectsMessage>,
    worker: TrackedWorker,
    view: EffectsOperationView,
    completed: bool,
    prepared: Option<EffectsMessage>,
}
pub(super) struct EffectsControl {
    next_revision: u64,
    active: Option<EffectsFlight>,
    retired: Vec<EffectsFlight>,
    history: VecDeque<EffectsOperationView>,
    closing: bool,
}
impl EffectsControl {
    pub(super) fn new() -> Self {
        Self {
            next_revision: 0,
            active: None,
            retired: Vec::new(),
            history: VecDeque::new(),
            closing: false,
        }
    }
    pub(super) fn views(&self) -> Vec<EffectsOperationView> {
        self.active
            .iter()
            .map(|flight| flight.view.clone())
            .chain(self.retired.iter().map(|flight| flight.view.clone()))
            .chain(self.history.iter().cloned())
            .collect()
    }
    pub(super) fn current(&self, operation: &str, revision: u64) -> bool {
        !self.closing
            && self.active.as_ref().is_some_and(|flight| {
                flight.revision == revision
                    && flight.view.operation_id == operation
                    && flight.view.state != EffectsOperationState::Cancelled
            })
    }
    fn begin(
        &mut self,
        operation: String,
        context: ImpulseDecodeContext,
        prepare: impl FnOnce(
            mpsc::SyncSender<EffectsMessage>,
            u64,
            ImpulseDecodeContext,
        ) -> Result<EffectsPreparation>
        + Send
        + 'static,
    ) -> Result<()> {
        ensure!(!self.closing, "Effect resource preparation is closing");
        ensure!(
            !operation.is_empty() && operation.len() <= 128 && !operation.contains('\0'),
            "Invalid effect operation identity"
        );
        self.reap();
        ensure!(
            self.retired.len() < MAX_RETIRED_FLIGHTS,
            "Cancelled effect producers are still retiring"
        );
        ensure!(
            self.active.is_none(),
            "An effect resource operation is already running"
        );
        context.check()?;
        let revision = self
            .next_revision
            .checked_add(1)
            .context("Effect operation revision exhausted")?;
        let permit = WorkerPermit::reserve(WorkerKind::Library)?;
        let (sender, receiver) = mpsc::sync_channel(2);
        let worker_context = context.clone();
        let worker_operation = operation.clone();
        let worker = std::thread::Builder::new()
            .name("lx-effect-resource".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    prepare(sender.clone(), revision, worker_context)
                }))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("Effect resource producer panicked")));
                let _ = sender.send(EffectsMessage::Prepared {
                    operation: worker_operation.clone(),
                    revision,
                    result,
                });
                #[cfg(all(test, windows))]
                super::effects_service::namespace_actor_tests::after_result(&worker_operation);
                #[cfg(all(test, windows))]
                super::effects_service::bundled_actor_tests::after_result(&worker_operation);
            })?;
        self.next_revision = revision;
        self.active = Some(EffectsFlight {
            revision,
            context,
            receiver,
            worker: permit.attach(worker),
            view: EffectsOperationView {
                operation_id: operation,
                revision,
                state: EffectsOperationState::Preparing,
                resource_id: None,
                error: None,
            },
            completed: false,
            prepared: None,
        });
        Ok(())
    }
    pub(super) fn begin_import(
        &mut self,
        operation: String,
        input: EffectsResourceInput,
        authority: crate::library::authority::AuthorityRegistry,
        path: std::path::PathBuf,
        extension: String,
        timeout: Duration,
    ) -> Result<()> {
        let EffectsResourceInput {
            host,
            profile,
            preferences,
        } = input;
        let journal_operation = operation.clone();
        self.begin(
            operation,
            ImpulseDecodeContext::new(timeout)?,
            move |sender, revision, context| {
                context.check()?;
                let selected = authority.open_selection(&path)?;
                let mut host = host
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Impulse namespace owner was poisoned"))?;
                let vault = host.prepare(
                    &profile,
                    &preferences,
                    &context,
                    &sender,
                    &journal_operation,
                    revision,
                )?;
                let imported = vault.import_selected(&selected, &extension, &context, &|fact| {
                    let (reply, receiver) = mpsc::sync_channel(1);
                    sender
                        .send(EffectsMessage::Inventory {
                            operation: journal_operation.clone(),
                            revision,
                            fact: fact.clone(),
                            reply,
                        })
                        .map_err(|_| anyhow::anyhow!("Effect inventory owner stopped"))?;
                    acknowledge(receiver, &context)?;
                    #[cfg(all(test, windows))]
                    super::effects_service::abort_actor_tests::after_writing_ack(
                        &profile, fact, &context,
                    )?;
                    Ok(())
                })?;
                Ok(EffectsPreparation::Imported {
                    resource: imported.resource,
                    impulse: imported.impulse,
                })
            },
        )
    }
    pub(super) fn begin_settings(
        &mut self,
        operation: String,
        input: EffectsResourceInput,
        settings: EffectsSettings,
        resource: StoredImpulseResource,
        timeout: Duration,
    ) -> Result<()> {
        let EffectsResourceInput {
            host,
            profile,
            preferences,
        } = input;
        settings.validate()?;
        resource.validate()?;
        ensure!(
            settings.convolution.impulse.as_ref() == Some(&resource.resource_id),
            "Effect request differs from its resource"
        );
        let journal_operation = operation.clone();
        self.begin(
            operation,
            ImpulseDecodeContext::new(timeout)?,
            move |sender, revision, context| {
                let mut host = host
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Impulse namespace owner was poisoned"))?;
                let vault = host.prepare(
                    &profile,
                    &preferences,
                    &context,
                    &sender,
                    &journal_operation,
                    revision,
                )?;
                let impulse = vault.prepare_owned(&resource, &context)?;
                Ok(EffectsPreparation::Settings(PreparedEffectsChange {
                    settings,
                    impulse: Some(impulse),
                }))
            },
        )
    }
    pub(super) fn begin_bundled_settings(
        &mut self,
        operation: String,
        settings: EffectsSettings,
        timeout: Duration,
    ) -> Result<()> {
        settings.validate()?;
        let Some(ImpulseResourceId::Bundled(id)) = &settings.convolution.impulse else {
            anyhow::bail!("Bundled effect preparation requires a bundled identity");
        };
        crate::audio::bundled_impulses::preset(id)?;
        let id = id.clone();
        self.begin(
            operation,
            ImpulseDecodeContext::new(timeout)?,
            move |_, _, context| {
                let impulse = crate::audio::bundled_impulses::prepare(&id, &context)?;
                Ok(EffectsPreparation::Settings(PreparedEffectsChange {
                    settings,
                    impulse: Some(impulse),
                }))
            },
        )
    }
    pub(super) fn begin_namespace_recovery(
        &mut self,
        operation: String,
        input: EffectsResourceInput,
        authority: crate::library::authority::AuthorityRegistry,
        selection: ImpulseNamespaceSelection,
        timeout: Duration,
    ) -> Result<()> {
        selection.validate()?;
        ensure!(
            input.preferences.creation.is_some(),
            "No current namespace creation obligation exists"
        );
        let journal_operation = operation.clone();
        self.begin(
            operation,
            ImpulseDecodeContext::new(timeout)?,
            move |sender, revision, context| {
                let mut host = input
                    .host
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Impulse namespace owner was poisoned"))?;
                host.recover_namespace(&input, &selection, &authority, &context)?;
                host.prepare(
                    &input.profile,
                    &input.preferences,
                    &context,
                    &sender,
                    &journal_operation,
                    revision,
                )?;
                Ok(EffectsPreparation::NamespaceRecovered)
            },
        )
    }
    pub(super) fn begin_recovery(
        &mut self,
        operation: String,
        host: Arc<Mutex<EffectsVaultHost>>,
        profile: std::path::PathBuf,
        preferences: EffectsPreferences,
        fact: ImpulseImportFact,
        timeout: Duration,
    ) -> Result<()> {
        fact.validate()?;
        ensure!(
            fact.body_identity.is_some(),
            "Impulse recovery requires an attested allocation"
        );
        let journal_operation = operation.clone();
        self.begin(
            operation,
            ImpulseDecodeContext::new(timeout)?,
            move |sender, revision, context| {
                let mut host = host
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Impulse namespace owner was poisoned"))?;
                let vault = host.prepare(
                    &profile,
                    &preferences,
                    &context,
                    &sender,
                    &journal_operation,
                    revision,
                )?;
                let recovered = vault.recover_import(&fact, &context)?;
                let mut fact = fact;
                fact.phase = crate::repository::ImpulseImportPhase::Sealed;
                let (reply, receiver) = mpsc::sync_channel(1);
                sender
                    .send(EffectsMessage::Inventory {
                        operation: journal_operation,
                        revision,
                        fact,
                        reply,
                    })
                    .map_err(|_| anyhow::anyhow!("Effect inventory owner stopped"))?;
                acknowledge(receiver, &context)?;
                Ok(EffectsPreparation::Imported {
                    resource: recovered.resource,
                    impulse: recovered.impulse,
                })
            },
        )
    }
    /// Actor handles inventory first, persists it, then replies. Completion
    /// ownership stays in the flight until the actor accepts or rejects it.
    pub(super) fn poll(&mut self) -> Vec<EffectsMessage> {
        let mut messages = Vec::with_capacity(2);
        if let Some(flight) = &mut self.active {
            while let Ok(message) = flight.receiver.try_recv() {
                if matches!(&message, EffectsMessage::Prepared { .. }) {
                    flight.completed = true;
                    flight.prepared = Some(message);
                } else {
                    messages.push(message);
                }
            }
            if !flight.completed && flight.worker.is_finished() && flight.worker.panicked() {
                flight.completed = true;
                flight.prepared = Some(EffectsMessage::Prepared {
                    operation: flight.view.operation_id.clone(),
                    revision: flight.revision,
                    result: Err(anyhow::anyhow!("Effect producer ended without a result")),
                });
            }
            if flight.completed
                && flight.worker.wait(Duration::ZERO)
                && let Some(prepared) = flight.prepared.take()
            {
                messages.push(prepared);
            }
        }
        self.reap();
        messages
    }
    pub(super) fn finish(
        &mut self,
        operation: &str,
        revision: u64,
        resource: Option<ImpulseResourceId>,
        error: Option<String>,
    ) -> Result<()> {
        ensure!(
            self.current(operation, revision),
            "Effect preparation is stale or cancelled"
        );
        ensure!(
            self.active.as_ref().is_some_and(|flight| flight.completed),
            "Effect producer has not transferred its result"
        );
        let mut flight = self.active.take().expect("current active flight");
        flight.view.state = if error.is_some() {
            EffectsOperationState::Failed
        } else {
            EffectsOperationState::Prepared
        };
        flight.view.resource_id = resource;
        flight.view.error = error.map(|message| message.chars().take(2048).collect());
        self.retired.push(flight);
        self.reap();
        Ok(())
    }
    pub(super) fn cancel(&mut self, operation: &str) -> bool {
        let Some(active) = &self.active else {
            return false;
        };
        if active.view.operation_id != operation {
            return false;
        }
        let mut flight = self.active.take().unwrap();
        flight.context.cancel();
        flight.view.state = EffectsOperationState::Cancelled;
        self.retired.push(flight);
        self.reap();
        true
    }
    fn reap(&mut self) {
        let mut index = 0;
        while index < self.retired.len() {
            // Drain cancelled publications on the registered producer's owner;
            // negative inventory acknowledgements let blocked workers retire.
            while let Ok(message) = self.retired[index].receiver.try_recv() {
                if let EffectsMessage::Inventory { reply, .. }
                | EffectsMessage::Creation { reply, .. }
                | EffectsMessage::Namespace { reply, .. } = message
                {
                    let _ = reply.try_send(Err("Effect operation was retired".into()));
                }
            }
            if self.retired[index].worker.wait(Duration::ZERO) {
                let flight = self.retired.remove(index);
                self.history.push_back(flight.view);
                if self.history.len() > MAX_OPERATION_HISTORY {
                    self.history.pop_front();
                }
            } else {
                index += 1;
            }
        }
    }
    pub(super) fn begin_shutdown(&mut self) {
        self.closing = true;
        if let Some(mut flight) = self.active.take() {
            flight.context.cancel();
            flight.view.state = EffectsOperationState::Cancelled;
            self.retired.push(flight);
        }
        for flight in &self.retired {
            flight.context.cancel();
        }
        self.reap();
    }
    pub(super) fn shutdown(&mut self, timeout: Duration) -> bool {
        self.begin_shutdown();
        let started = std::time::Instant::now();
        for flight in &self.retired {
            flight
                .worker
                .wait(timeout.saturating_sub(started.elapsed()));
        }
        self.reap();
        self.retired.is_empty()
    }
}
impl Drop for EffectsControl {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::{ImpulseFileIdentity, ImpulseImportPhase};

    fn identity() -> ImpulseFileIdentity {
        ImpulseFileIdentity {
            device: 1,
            object: "actual-file".into(),
        }
    }
    fn preferences() -> EffectsPreferences {
        EffectsPreferences {
            vault: Some(ImpulseVaultReceipt {
                namespace: "1".repeat(32),
                profile_identity: identity(),
                directory_identity: identity(),
                owner_identity: identity(),
            }),
            ..Default::default()
        }
    }
    fn fact() -> ImpulseImportFact {
        ImpulseImportFact {
            namespace: "1".repeat(32),
            token: "2".repeat(32),
            body_identity: None,
            reserved_bytes: 192,
            expected_sha256: "3".repeat(64),
            phase: ImpulseImportPhase::Allocating,
        }
    }
    fn resource() -> StoredImpulseResource {
        StoredImpulseResource {
            resource_id: ImpulseResourceId::Imported("2".repeat(32)),
            namespace: "1".repeat(32),
            body_identity: identity(),
            encoded_sha256: "3".repeat(64),
            encoded_bytes: 48,
            extension: "wav".into(),
            codec: "pcm".into(),
            decoded_frames: 2,
            sample_rate: 8000,
            channels: 1,
        }
    }
    #[test]
    fn persisted_resource_needs_exact_sealed_inventory_and_selection_membership() {
        let mut preferences = preferences();
        assert!(preferences.register_resource(resource()).is_err());
        preferences.record_import(fact()).unwrap();
        let mut writing = fact();
        writing.phase = ImpulseImportPhase::Writing;
        writing.body_identity = Some(identity());
        preferences.record_import(writing.clone()).unwrap();
        assert!(preferences.record_import(fact()).is_err());
        assert!(preferences.register_resource(resource()).is_err());
        writing.phase = ImpulseImportPhase::Sealed;
        preferences.record_import(writing).unwrap();
        preferences.register_resource(resource()).unwrap();
        assert!(preferences.imports.is_empty());
        preferences.settings.convolution.impulse = Some(resource().resource_id);
        preferences.validate().unwrap();
        preferences.settings.convolution.impulse =
            Some(ImpulseResourceId::Imported("4".repeat(32)));
        assert!(preferences.validate().is_err());
    }
    #[test]
    fn legacy_spatial_migrates_without_creating_ir_authority() {
        let spatial = SpatialSettings {
            enabled: true,
            ..Default::default()
        };
        let migrated = EffectsPreferences::from_legacy_spatial(spatial);
        assert_eq!(migrated.settings.spatial, spatial);
        assert!(migrated.vault.is_none());
        assert!(migrated.resources.is_empty());
        assert!(migrated.settings.convolution.impulse.is_none());
        migrated.validate().unwrap();
        assert!(serde_json::from_str::<EffectsPreferences>(r#"{"settings":{"convolution":{"impulse":{"kind":"imported","id":"../../foreign"}}}}"#).is_err());
    }
    #[test]
    fn cancelled_completion_cannot_become_a_current_application() {
        let mut control = EffectsControl::new();
        let (entered, observed) = mpsc::sync_channel(1);
        let (release, blocked) = mpsc::sync_channel(1);
        control
            .begin(
                "op".into(),
                ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap(),
                move |_, _, _| {
                    entered.send(()).unwrap();
                    blocked.recv().unwrap();
                    Ok(EffectsPreparation::Settings(PreparedEffectsChange {
                        settings: EffectsSettings::default(),
                        impulse: None,
                    }))
                },
            )
            .unwrap();
        observed.recv_timeout(Duration::from_secs(2)).unwrap();
        let revision = control.views()[0].revision;
        assert!(control.cancel("op"));
        assert!(!control.current("op", revision));
        assert!(control.finish("op", revision, None, None).is_err());
        release.send(()).unwrap();
        assert!(control.shutdown(Duration::from_secs(3)));
        assert!(
            control
                .views()
                .iter()
                .all(|view| view.state == EffectsOperationState::Cancelled)
        );
    }
    #[test]
    fn bundled_selection_needs_known_id_and_no_private_namespace_or_inventory() {
        let mut preferences = EffectsPreferences::default();
        preferences.settings.convolution = crate::audio::bundled_impulses::preset("telephone")
            .unwrap()
            .convolution_settings();
        preferences.validate().unwrap();
        assert!(preferences.vault.is_none());
        assert!(preferences.resources.is_empty() && preferences.imports.is_empty());
        let mut control = EffectsControl::new();
        let mut unknown = preferences.settings.clone();
        unknown.convolution.impulse = Some(ImpulseResourceId::Bundled("not_shipped".into()));
        assert!(
            control
                .begin_bundled_settings("unknown".into(), unknown.clone(), Duration::from_secs(30))
                .is_err()
        );
        assert!(control.views().is_empty());
        preferences.settings = unknown;
        assert!(preferences.validate().is_err());
        assert!(control.shutdown(Duration::ZERO));
    }
}
