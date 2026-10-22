use super::library_relink_cold::{ColdAction, ColdReplacementMode};
use super::*;
use crate::domain::{library::*, library_watch::*};
use crate::library::{
    actor::{LibraryAck, LibraryAutoRelinkOutcome},
    authority::LiveRelocationHint,
    scan::ScanContext,
};
use std::path::Path;

#[derive(Default)]
pub(super) struct RelocationRuntime {
    queue: VecDeque<AutoJob>,
    pending: HashMap<String, LibraryWatchToken>,
    live_checks: HashMap<String, (u64, u64, String)>,
    checked: HashMap<String, (u64, u64)>,
    pub(super) deferred_finishes: VecDeque<crate::library::actor::LibraryCompletion>,
    pub(super) cold: super::library_relink_cold::ColdScanFence,
    cold_sealed: HashSet<String>,
    cold_reopens: VecDeque<ColdReopen>,
    cold_pending: HashMap<String, ColdReopen>,
}

struct ColdReopen {
    operation_id: String,
    token: LibraryScanToken,
    watch: LibraryWatchToken,
    original: LibraryAssetBinding,
    path: PathBuf,
    identity: FileIdentity,
    mode: ColdReplacementMode,
}

#[derive(serde::Serialize)]
struct AutoJob {
    operation_id: String,
    path: PathBuf,
    original: TrackRecord,
    old_binding: LibraryAssetBinding,
    target_authority: LocalAssetAuthority,
    root: LibraryRoot,
    watch_token: LibraryWatchToken,
}

impl Runtime {
    pub(super) fn seal_cold_scan(
        &mut self,
        token: &LibraryScanToken,
        outcome: &LibraryOperationView,
        complete: bool,
    ) -> Result<(bool, bool)> {
        if !self.library.relocation.cold.owns(token) {
            return Ok((complete, complete));
        }
        let preference = self
            .repository
            .library_watch(&token.root_id)?
            .context("Cold scan watch was removed")?;
        if self
            .library
            .relocation
            .cold_sealed
            .contains(&token.operation_id)
        {
            return Ok((
                self.library.relocation.cold.can_finalize(token),
                self.library.relocation.cold.can_cover_watch(token),
            ));
        }
        let watch = LibraryWatchToken {
            root_id: token.root_id.clone(),
            grant_epoch: token.grant_epoch,
            watch_generation: preference.watch_generation,
            enabled: preference.enabled,
            options: preference.options,
        };
        let view = self
            .library_watch_view(&token.root_id)?
            .context("Cold scan watch is unavailable")?;
        let decision = self.library.relocation.cold.seal(
            token,
            &watch,
            complete,
            outcome.skipped_files,
            0,
            view.received_sequence,
            view.dropped_events,
            view.error.is_none()
                && !matches!(
                    view.state,
                    LibraryWatchState::Disabled
                        | LibraryWatchState::Degraded
                        | LibraryWatchState::Stopping
                ),
        )?;
        self.library
            .relocation
            .cold_sealed
            .insert(token.operation_id.clone());
        for action in decision.actions {
            self.queue_cold_action(token, &watch, action)?;
        }
        Ok((
            decision.allow_missing_finalizer,
            self.library.relocation.cold.can_cover_watch(token),
        ))
    }

    fn queue_cold_action(
        &mut self,
        token: &LibraryScanToken,
        watch: &LibraryWatchToken,
        action: ColdAction,
    ) -> Result<()> {
        match action {
            ColdAction::RelocateOriginal { original, new_path } => {
                self.queue_auto_relink(watch, &original.asset_id, new_path)
            }
            ColdAction::ReopenReplacement {
                original,
                path,
                observed_identity,
                mode,
            } => {
                let operation_id = crate::domain::music::stable_id(
                    "cold-reopen",
                    &serde_json::to_vec(&(
                        token.operation_id.as_str(),
                        original.asset_id.as_str(),
                        &path,
                        &observed_identity,
                    ))?,
                );
                if self
                    .library
                    .relocation
                    .cold_pending
                    .contains_key(&operation_id)
                    || self
                        .library
                        .relocation
                        .cold_reopens
                        .iter()
                        .any(|job| job.operation_id == operation_id)
                {
                    return Ok(());
                }
                let job = ColdReopen {
                    operation_id,
                    token: token.clone(),
                    watch: watch.clone(),
                    original,
                    path,
                    identity: observed_identity,
                    mode,
                };
                let bytes = self
                    .library
                    .relocation
                    .cold_reopens
                    .iter()
                    .chain(self.library.relocation.cold_pending.values())
                    .map(cold_reopen_bytes)
                    .sum::<usize>();
                ensure!(
                    self.library.relocation.cold_reopens.len()
                        + self.library.relocation.cold_pending.len()
                        < MAX_LIBRARY_BATCH_ITEMS
                        && bytes + cold_reopen_bytes(&job) <= MAX_LIBRARY_BATCH_BYTES,
                    LibraryError::budget("Cold replacement reopen metadata exceeds its budget")
                );
                self.library.relocation.cold_reopens.push_back(job);
                Ok(())
            }
            ColdAction::Unresolved { original, reason } => {
                self.error(format!("{}: {}", original.asset_id, reason.message));
                Ok(())
            }
        }
    }

    fn pump_cold_reopens(&mut self) {
        for _ in 0..2 {
            let Some(job) = self.library.relocation.cold_reopens.front() else {
                break;
            };
            let root = match self.current_auto_watch(&job.watch) {
                Ok(root) => root,
                Err(error) => {
                    self.error(format!("Cold replacement expired: {error:#}"));
                    self.library.relocation.cold_reopens.pop_front();
                    continue;
                }
            };
            let job = self.library.relocation.cold_reopens.pop_front().unwrap();
            let context = ScanContext::new(Duration::from_secs(30));
            let operation = job.operation_id.clone();
            let result = self.library.actor(&self.catalog).and_then(|actor| {
                actor.submit_refresh_root_path(
                    operation.clone(),
                    job.path.clone(),
                    root,
                    job.watch.clone(),
                    job.identity.clone(),
                    context.clone(),
                )
            });
            match result {
                Ok(()) => {
                    self.library.contexts.insert(operation.clone(), context);
                    self.library.relocation.cold_pending.insert(operation, job);
                }
                Err(error) => {
                    self.library.relocation.cold_reopens.push_front(job);
                    self.error(format!("Cold replacement submission failed: {error:#}"));
                    break;
                }
            }
        }
    }

    pub(super) fn complete_cold_reopen(
        &mut self,
        operation: String,
        watch: LibraryWatchToken,
        result: Result<Box<crate::library::actor::LibraryRootPathPreparation>>,
        ack: LibraryAck,
    ) {
        let context = self.library.contexts.remove(&operation);
        let job = self.library.relocation.cold_pending.remove(&operation);
        let outcome = (|| -> Result<()> {
            let context = context.as_ref().context("Cold replacement is stale")?;
            let job = job
                .as_ref()
                .context("Cold replacement intent is unavailable")?;
            ensure!(
                job.watch == watch,
                LibraryError::conflict("Cold replacement watch changed")
            );
            self.current_auto_watch(&watch)?;
            context.check()?;
            let prepared = result?;
            prepared.check()?;
            ensure!(
                prepared.prepared.file_identity.as_ref() == Some(&job.identity),
                LibraryError::conflict("Cold replacement physical identity changed")
            );
            let current = self
                .repository
                .library_asset_binding(&job.original.asset_id)?
                .context("Cold original binding is unavailable")?;
            match job.mode {
                ColdReplacementMode::PreserveLogicalIdentity => ensure!(
                    current == job.original,
                    LibraryError::conflict("Cold original binding changed")
                ),
                ColdReplacementMode::FreshIdentity => ensure!(
                    current.path != job.path
                        && current.binding_revision > job.original.binding_revision,
                    LibraryError::conflict(
                        "Cold original has not been published at its relocated path"
                    )
                ),
            }
            let item = self
                .repository
                .prepare_library_identity(&prepared.prepared, &|| {
                    context.check()?;
                    prepared.check()
                })?;
            let seen = LibrarySeenFile {
                path: item.asset.path.clone(),
                asset_id: Some(item.asset.id.clone()),
            };
            let check = || -> Result<()> {
                context.check()?;
                prepared.check()?;
                if let Some(evidence) = &prepared.content_evidence {
                    evidence.check()?;
                }
                Ok(())
            };
            let committed = self.repository.apply_library_scan_batch(
                &job.token,
                &[seen],
                std::slice::from_ref(&item),
                &check,
            )?;
            self.revision = committed.change.revision;
            if let Some(evidence) = &prepared.content_evidence {
                let binding = self
                    .repository
                    .library_asset_binding(&item.asset.id)?
                    .context("Cold replacement content binding is unavailable")?;
                self.repository.put_library_content_evidence(
                    &item.asset.id,
                    binding.binding_revision,
                    &evidence.file_identity,
                    evidence.size_bytes,
                    evidence.modified_ms,
                    &evidence.content_sha256,
                    &check,
                )?;
            }
            let mut local = prepared.local;
            local.bind_library_identity(&item)?;
            local.set_authorization(super::library::file_authorization(
                &self.repository,
                &self.library.authority,
                &item.asset.id,
            )?);
            self.catalog.commit_local(local);
            if job.mode == ColdReplacementMode::PreserveLogicalIdentity {
                self.invalidate_relocated_track(&item.track.id);
            }
            self.library
                .relocation
                .cold
                .resolve_replacement(&job.token, &job.original.asset_id)?;
            self.reconcile_queue()?;
            Ok(())
        })();
        ack.complete(
            outcome
                .as_ref()
                .map(|_| ())
                .map_err(|error| format!("{error:#}")),
        );
        if let Err(error) = outcome {
            if let Some(job) = job {
                self.library.relocation.cold.abort(&job.token);
            }
            self.error(format!("Cold replacement failed: {error:#}"));
        }
    }

    pub(super) fn retire_cold_scan(&mut self, token: &LibraryScanToken) {
        self.library.relocation.cold.remove(token);
        self.library
            .relocation
            .cold_sealed
            .remove(&token.operation_id);
    }
    pub(super) fn take_deferred_library_finish(
        &mut self,
    ) -> Option<crate::library::actor::LibraryCompletion> {
        let index =
            self.library.relocation.deferred_finishes.iter().position(
                |completion| match completion {
                    crate::library::actor::LibraryCompletion::Finished { token, .. } => {
                        !self.relocations_pending(&token.root_id)
                    }
                    _ => true,
                },
            )?;
        self.library.relocation.deferred_finishes.remove(index)
    }

    pub(super) fn stage_scan_relocations(
        &mut self,
        token: &LibraryScanToken,
        prepared: &mut Vec<(PreparedLocalImport, LibraryPreparedAsset)>,
        seen: &mut [LibrarySeenFile],
    ) -> Result<()> {
        let Some(preference) = self.repository.library_watch(&token.root_id)? else {
            return Ok(());
        };
        if !preference.enabled || !preference.options.auto_relink {
            return Ok(());
        }
        let watch = LibraryWatchToken {
            root_id: token.root_id.clone(),
            grant_epoch: token.grant_epoch,
            watch_generation: preference.watch_generation,
            enabled: true,
            options: preference.options,
        };
        let mut retain = Vec::with_capacity(prepared.len());
        for (local, item) in std::mem::take(prepared) {
            let matches = item
                .file_identity
                .as_ref()
                .map(|identity| self.repository.library_assets_for_identity(identity, 3))
                .transpose()?
                .unwrap_or_default();
            let candidate = matches
                .iter()
                .filter(|binding| {
                    crate::domain::music::normalize_local_path(&binding.path)
                        != crate::domain::music::normalize_local_path(&item.asset.path)
                })
                .collect::<Vec<_>>();
            let scoped = matches
                .iter()
                .filter_map(|binding| {
                    self.repository
                        .local_asset_authority(&binding.asset_id)
                        .ok()
                        .flatten()
                        .filter(|authority| {
                            authority.roots.iter().any(|root| {
                                root.root_id == token.root_id
                                    && root.grant_epoch == token.grant_epoch
                            })
                        })
                        .map(|_| binding.clone())
                })
                .collect::<Vec<_>>();
            let occupied = self
                .repository
                .library_asset_at_path(&item.asset.path)?
                .map(|asset| self.repository.library_asset_binding(&asset.id))
                .transpose()?
                .flatten();
            let observation = self.library.relocation.cold.observe(
                token,
                &item.asset.path,
                item.file_identity.as_ref(),
                occupied.as_ref(),
                &scoped,
            )?;
            if observation.suppress_replacement {
                for file in seen.iter_mut().filter(|file| file.path == item.asset.path) {
                    file.asset_id = None;
                }
                continue;
            }
            if candidate.len() == 1
                && self
                    .repository
                    .local_asset_authority(&candidate[0].asset_id)?
                    .is_some_and(|authority| {
                        authority.roots.iter().any(|root| {
                            root.root_id == token.root_id && root.grant_epoch == token.grant_epoch
                        })
                    })
            {
                self.queue_auto_relink(&watch, &candidate[0].asset_id, item.asset.path.clone())?;
                for file in seen.iter_mut().filter(|file| file.path == item.asset.path) {
                    file.asset_id = None;
                }
                continue;
            }
            retain.push((local, item));
        }
        *prepared = retain;
        Ok(())
    }

    pub(super) fn retire_root_relocations(&mut self, root_id: &str) {
        self.library.relocation.cold.retire_root(root_id);
        self.library
            .relocation
            .cold_reopens
            .retain(|job| job.token.root_id != root_id);
        self.library
            .relocation
            .queue
            .retain(|job| job.watch_token.root_id != root_id);
        self.library.relocation.checked.remove(root_id);
        let mut operations = self
            .library
            .relocation
            .pending
            .iter()
            .filter(|(_, token)| token.root_id == root_id)
            .map(|(operation, _)| operation.clone())
            .collect::<Vec<_>>();
        if let Some((_, _, operation)) = self.library.relocation.live_checks.remove(root_id) {
            operations.push(operation);
        }
        for operation in operations {
            self.library.cancel(&operation);
            self.library.relocation.pending.remove(&operation);
        }
    }
    fn current_auto_watch(&self, token: &LibraryWatchToken) -> Result<LibraryRoot> {
        let root = self
            .repository
            .library_root(&token.root_id)?
            .context("Automatic relocation root is unavailable")?;
        let preference = self
            .repository
            .library_watch(&token.root_id)?
            .context("Automatic relocation watch is unavailable")?;
        ensure!(
            root.grant_epoch == token.grant_epoch
                && preference.enabled
                && preference.options.auto_relink
                && preference.watch_generation == token.watch_generation,
            LibraryError::conflict("Automatic relocation watch changed")
        );
        Ok(root)
    }

    pub(super) fn queue_auto_relink(
        &mut self,
        token: &LibraryWatchToken,
        asset_id: &str,
        path: PathBuf,
    ) -> Result<()> {
        let path = crate::domain::music::normalize_local_path(&path);
        let root = self.current_auto_watch(token)?;
        let old_binding = self
            .repository
            .library_asset_binding(asset_id)?
            .context("Relocation binding is unavailable")?;
        if old_binding.path == path {
            return Ok(());
        }
        let old = self
            .repository
            .local_asset_authority(asset_id)?
            .context("Relocation original authority is unavailable")?;
        ensure!(
            old.roots
                .iter()
                .any(|root| root.root_id == token.root_id && root.grant_epoch == token.grant_epoch),
            LibraryError::conflict("Relocation has no original root membership")
        );
        let original = self
            .repository
            .track(&old.track_id)?
            .context("Relocation original track is unavailable")?;
        let roots = old
            .roots
            .into_iter()
            .filter(|root| root_contains_path(root, &path))
            .collect::<Vec<_>>();
        if !roots
            .iter()
            .any(|root| root.root_id == token.root_id && root.grant_epoch == token.grant_epoch)
        {
            return Ok(());
        }
        let target_authority = LocalAssetAuthority {
            asset_id: asset_id.into(),
            track_id: old.track_id,
            path: path.clone(),
            explicit_file_epoch: None,
            roots,
        };
        let operation_id = crate::domain::music::stable_id(
            "auto-relink",
            &serde_json::to_vec(&(
                token.root_id.as_str(),
                token.watch_generation,
                asset_id,
                old_binding.binding_revision,
                &path,
            ))?,
        );
        if self.library.relocation.pending.contains_key(&operation_id)
            || self
                .library
                .relocation
                .queue
                .iter()
                .any(|job| job.operation_id == operation_id)
        {
            return Ok(());
        }
        let job = AutoJob {
            operation_id,
            path,
            original,
            old_binding,
            target_authority,
            root,
            watch_token: token.clone(),
        };
        ensure!(
            self.library.relocation.queue.len() < MAX_RELINK_CANDIDATES,
            LibraryError::budget("Automatic relocation queue is full")
        );
        let bytes = self
            .library
            .relocation
            .queue
            .iter()
            .map(|job| {
                serde_json::to_vec(job).map_or(MAX_RELINK_DOCUMENT_BYTES, |bytes| bytes.len())
            })
            .sum::<usize>();
        ensure!(
            bytes + serde_json::to_vec(&job)?.len() <= MAX_RELINK_DOCUMENT_BYTES,
            LibraryError::budget("Automatic relocation queue exceeds its byte limit")
        );
        self.library.relocation.queue.push_back(job);
        Ok(())
    }

    pub(super) fn prepare_live_relocations(
        &mut self,
        token: &LibraryWatchToken,
        sequence: u64,
    ) -> Result<bool> {
        if !token.options.auto_relink {
            return Ok(false);
        }
        if self.library.relocation.checked.get(&token.root_id)
            == Some(&(token.watch_generation, sequence))
        {
            return Ok(false);
        }
        if self
            .library
            .relocation
            .live_checks
            .contains_key(&token.root_id)
        {
            return Ok(true);
        }
        let root = self.current_auto_watch(token)?;
        let operation = crate::domain::music::stable_id(
            "live-relink",
            &serde_json::to_vec(&(token.root_id.as_str(), token.watch_generation, sequence))?,
        );
        let context = ScanContext::new(Duration::from_secs(30));
        self.library
            .actor(&self.catalog)?
            .submit_live_relocation_hints(
                operation.clone(),
                root,
                token.clone(),
                context.clone(),
            )?;
        self.library.contexts.insert(operation.clone(), context);
        self.library.relocation.live_checks.insert(
            token.root_id.clone(),
            (token.watch_generation, sequence, operation),
        );
        Ok(true)
    }

    pub(super) fn complete_live_relocation_hints(
        &mut self,
        operation_id: String,
        root_id: String,
        generation: u64,
        result: Result<Vec<LiveRelocationHint>>,
    ) {
        let context = self.library.contexts.remove(&operation_id);
        let state = if self
            .library
            .relocation
            .live_checks
            .get(&root_id)
            .is_some_and(|(_, _, operation)| operation == &operation_id)
        {
            self.library.relocation.live_checks.remove(&root_id)
        } else {
            None
        };
        let outcome = (|| -> Result<()> {
            context
                .as_ref()
                .context("Relocation hint operation is stale")?
                .check()?;
            let (expected, sequence, expected_operation) =
                state.context("Relocation hint state is unavailable")?;
            ensure!(
                expected == generation && expected_operation == operation_id,
                LibraryError::conflict("Relocation hint generation changed")
            );
            let preference = self
                .repository
                .library_watch(&root_id)?
                .context("Relocation watch is unavailable")?;
            let root = self
                .repository
                .library_root(&root_id)?
                .context("Relocation root is unavailable")?;
            let token = LibraryWatchToken {
                root_id: root_id.clone(),
                grant_epoch: root.grant_epoch,
                watch_generation: preference.watch_generation,
                enabled: preference.enabled,
                options: preference.options,
            };
            ensure!(
                token.watch_generation == generation,
                LibraryError::conflict("Relocation hint generation changed")
            );
            for hint in result? {
                self.queue_auto_relink(&token, &hint.asset_id, hint.current_path)?;
            }
            self.library
                .relocation
                .checked
                .insert(root_id, (generation, sequence));
            Ok(())
        })();
        if let Err(error) = outcome {
            self.error(format!("Live relocation preparation failed: {error:#}"));
        }
    }

    pub(super) fn relocations_pending(&self, root_id: &str) -> bool {
        self.library.relocation.live_checks.contains_key(root_id)
            || self
                .library
                .relocation
                .pending
                .values()
                .any(|token| token.root_id == root_id)
            || self
                .library
                .relocation
                .queue
                .iter()
                .any(|job| job.root.root_id == root_id)
            || self
                .library
                .relocation
                .cold_reopens
                .iter()
                .any(|job| job.token.root_id == root_id)
            || self
                .library
                .relocation
                .cold_pending
                .values()
                .any(|job| job.token.root_id == root_id)
    }

    pub(super) fn pump_auto_relocations(&mut self) {
        self.pump_cold_reopens();
        for _ in 0..2 {
            let Some(job) = self.library.relocation.queue.front() else {
                break;
            };
            if let Err(error) = self.current_auto_watch(&job.watch_token) {
                self.library.relocation.queue.pop_front();
                self.error(format!("Automatic relocation expired: {error:#}"));
                continue;
            }
            let root_id = job.root.root_id.clone();
            if self
                .library
                .actor(&self.catalog)
                .and_then(|actor| actor.can_submit_scan(&root_id))
                .is_err()
            {
                break;
            }
            let mut job = self.library.relocation.queue.pop_front().unwrap();
            if let Ok(root) = self.current_auto_watch(&job.watch_token) {
                job.root = root;
                for grant in &mut job.target_authority.roots {
                    if let Ok(Some(current)) = self.repository.library_root(&grant.root_id)
                        && current.grant_epoch == grant.grant_epoch
                    {
                        *grant = current;
                    }
                }
            }
            let context = ScanContext::new(Duration::from_secs(30));
            let operation = job.operation_id.clone();
            let token = job.watch_token.clone();
            let submitted = self.library.actor(&self.catalog).and_then(|actor| {
                actor.submit_auto_relink(
                    job.operation_id,
                    job.path,
                    job.original,
                    job.old_binding,
                    job.target_authority,
                    job.root,
                    job.watch_token,
                    context.clone(),
                )
            });
            match submitted {
                Ok(()) => {
                    self.library.contexts.insert(operation.clone(), context);
                    self.library
                        .relocation
                        .pending
                        .insert(operation.clone(), token);
                    let mut view = super::library::validating(operation, self.revision);
                    view.root_id = Some(root_id);
                    view.phase = LibraryPhase::Preparing;
                    self.library.remember(view);
                }
                Err(error) => {
                    self.error(format!("Automatic relocation submission failed: {error:#}"))
                }
            }
        }
    }

    pub(super) fn complete_auto_relink(
        &mut self,
        operation_id: String,
        token: LibraryWatchToken,
        result: Result<LibraryAutoRelinkOutcome>,
        ack: LibraryAck,
    ) {
        let context = self.library.contexts.remove(&operation_id);
        let expected = self.library.relocation.pending.remove(&operation_id);
        let outcome = (|| -> Result<()> {
            let context = context.as_ref().context("Automatic relocation is stale")?;
            context.check()?;
            ensure!(
                expected.as_ref() == Some(&token),
                LibraryError::conflict("Automatic relocation token changed")
            );
            self.current_auto_watch(&token)?;
            match result? {
                LibraryAutoRelinkOutcome::NeedsSelection(selection) => {
                    let candidate = LibraryRelinkCandidate {
                        candidate_id: crate::domain::music::stable_id(
                            "relink-candidate",
                            operation_id.as_bytes(),
                        ),
                        asset_id: selection.asset_id,
                        root_id: token.root_id.clone(),
                        grant_epoch: token.grant_epoch,
                        watch_generation: token.watch_generation,
                        old_path: selection.old_path,
                        new_path: selection.new_path,
                        new_identity: selection.new_identity,
                        content_sha256: None,
                        binding_revision: selection.expected_binding_revision,
                        evidence: RelinkEvidence::ManualSelection,
                        requires_selection: true,
                        expires_at_ms: now_ms()?.saturating_add(300_000),
                    };
                    self.repository
                        .put_library_relink_candidate(&candidate, &|| context.check())?;
                }
                LibraryAutoRelinkOutcome::Prepared(mut prepared) => {
                    let request = LibraryRelinkCommit {
                        operation_id: operation_id.clone(),
                        expected_binding_revision: prepared.expected_binding_revision,
                        prepared: prepared.prepared.clone(),
                        authorization: LibraryRelinkAuthorization::Root {
                            root_id: token.root_id.clone(),
                            grant_epoch: token.grant_epoch,
                            watch_generation: token.watch_generation,
                        },
                        evidence: prepared.evidence,
                        content_sha256: prepared.content_sha256.clone(),
                        allow_content_change: false,
                    };
                    let committed = self.repository.commit_library_relink(&request, &|| {
                        context.check()?;
                        prepared.check()
                    })?;
                    self.revision = committed.change.revision;
                    let authority = self
                        .repository
                        .local_asset_authority(&committed.result.asset_id)?
                        .context("Committed relocation authority is unavailable")?;
                    let authorization = prepared.publish(&self.library.authority, &authority)?;
                    let binding = self
                        .repository
                        .library_asset_binding(&committed.result.asset_id)?
                        .context("Published relocation binding is unavailable")?;
                    let scans = self
                        .library
                        .tokens
                        .values()
                        .filter(|scan| scan.root_id == token.root_id)
                        .cloned()
                        .collect::<Vec<_>>();
                    for scan in scans {
                        if self
                            .library
                            .relocation
                            .cold
                            .contains_original(&scan, &binding.asset_id)
                            && let Some(action) = self.library.relocation.cold.published(
                                &scan,
                                &binding.asset_id,
                                &binding,
                            )?
                        {
                            self.queue_cold_action(&scan, &token, action)?;
                        }
                    }
                    let mut local = prepared.local;
                    local.set_authorization(authorization);
                    self.catalog
                        .commit_rebound(local, committed.result.media_changed)?;
                    if committed.result.media_changed {
                        self.invalidate_relocated_track(&committed.result.track_id);
                    }
                    self.reconcile_queue()?;
                    self.events.lock().unwrap().publish(
                        self.revision,
                        "library",
                        Some(operation_id.clone()),
                        BackendEvent::LibraryRelinked {
                            result: Box::new(committed.result),
                        },
                    );
                }
            }
            Ok(())
        })();
        ack.complete(
            outcome
                .as_ref()
                .map(|_| ())
                .map_err(|error| format!("{error:#}")),
        );
        let mut view = super::library::validating(operation_id.clone(), self.revision);
        view.root_id = Some(token.root_id);
        view.state = if outcome.is_ok() {
            LibraryOperationState::Completed
        } else {
            LibraryOperationState::Failed
        };
        view.error = outcome.as_ref().err().map(super::library::library_error);
        self.library.remember(view);
        self.publish_finished(operation_id, &outcome);
    }
}

fn root_contains_path(root: &LibraryRoot, path: &Path) -> bool {
    let normalize = |path: &Path| {
        let path = crate::domain::music::normalize_local_path(path);
        match root.path_policy {
            LibraryPathPolicy::Sensitive => path,
            LibraryPathPolicy::WindowsInsensitive => path
                .to_str()
                .map_or(path.clone(), |text| PathBuf::from(text.to_lowercase())),
        }
    };
    let path = normalize(path);
    let prefix = normalize(&root.path);
    path != prefix
        && path.starts_with(&prefix)
        && (root.recursive || path.parent() == Some(prefix.as_path()))
}

fn now_ms() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis()
        .min(i64::MAX as u128) as u64)
}

fn cold_reopen_bytes(job: &ColdReopen) -> usize {
    serde_json::to_vec(&(
        &job.operation_id,
        &job.token,
        &job.watch,
        &job.original,
        &job.path,
        &job.identity,
    ))
    .map_or(MAX_LIBRARY_BATCH_BYTES, |bytes| bytes.len())
}
