use super::library_watch::WatchRuntime;
use super::*;
use crate::domain::library::*;
use crate::domain::library_watch::*;
use crate::library::actor::{LibraryActor, LibraryCompletion};
use crate::library::authority::{AuthorityRegistry, FileAuthorization};
use crate::library::scan::ScanContext;
use crate::repository::LibraryContentEvidenceCommit;

pub(super) struct LibraryRuntime {
    pub(super) authority: AuthorityRegistry,
    actor: Option<LibraryActor>,
    pub(super) contexts: HashMap<String, ScanContext>,
    pub(super) tokens: HashMap<String, LibraryScanToken>,
    operations: VecDeque<LibraryOperationView>,
    last_progress: HashMap<String, Instant>,
    verification_queue: VecDeque<LibraryRoot>,
    verifying: HashMap<String, u64>,
    pub(super) watch: WatchRuntime,
    pub(super) relocation: super::library_relink::RelocationRuntime,
    root_rebinds: HashMap<String, RootRebindIntent>,
    relinks: HashMap<String, RelinkIntent>,
}
struct RelinkIntent {
    expected_binding_revision: u64,
    allow_content_change: bool,
    candidate: Option<LibraryRelinkCandidate>,
}
struct RootRebindIntent {
    root_id: String,
    grant_epoch: u64,
    location_revision: u64,
    auto_follow: bool,
    watch_generation: Option<u64>,
}
impl LibraryRuntime {
    pub(super) fn new(authority: AuthorityRegistry) -> Self {
        Self {
            authority,
            actor: None,
            contexts: HashMap::new(),
            tokens: HashMap::new(),
            operations: VecDeque::new(),
            last_progress: HashMap::new(),
            verification_queue: VecDeque::new(),
            verifying: HashMap::new(),
            watch: WatchRuntime::default(),
            relocation: super::library_relink::RelocationRuntime::default(),
            root_rebinds: HashMap::new(),
            relinks: HashMap::new(),
        }
    }
    pub(super) fn actor(&mut self, catalog: &Catalog) -> Result<&mut LibraryActor> {
        if self.actor.is_none() {
            self.actor = Some(LibraryActor::new_with_authority(
                catalog.clone(),
                self.authority.clone(),
            )?);
        }
        Ok(self.actor.as_mut().unwrap())
    }
    pub(super) fn remember(&mut self, view: LibraryOperationView) {
        self.operations
            .retain(|old| old.operation_id != view.operation_id);
        self.operations.push_back(view);
        while self.operations.len() > MAX_LIBRARY_OPERATIONS
            || self
                .operations
                .iter()
                .map(|view| {
                    serde_json::to_vec(view).map_or(MAX_LIBRARY_PAGE_BYTES, |bytes| bytes.len())
                })
                .sum::<usize>()
                > MAX_LIBRARY_PAGE_BYTES
        {
            if let Some(index) = self
                .operations
                .iter()
                .position(|view| view.state != LibraryOperationState::Running)
            {
                self.operations.remove(index);
            } else {
                break;
            }
        }
    }
    pub(super) fn cancel(&self, operation: &str) -> bool {
        if let Some(context) = self.contexts.get(operation) {
            context.cancel.store(true, Ordering::Release);
        }
        self.actor
            .as_ref()
            .is_some_and(|actor| actor.cancel(operation))
    }
    pub(super) fn shutdown(&mut self, timeout: Duration) -> Result<bool> {
        for context in self.contexts.values() {
            context.cancel.store(true, Ordering::Release);
        }
        let deadline = Instant::now() + timeout;
        let watch = self.watch.shutdown(timeout);
        let actor = match &mut self.actor {
            Some(actor) => actor.shutdown(deadline.saturating_duration_since(Instant::now())),
            None => Ok(true),
        };
        // A permanent error is reported only after the other owner has also
        // joined; it must not turn an unfinished sibling into successful exit.
        if matches!(watch, Ok(false)) || matches!(actor, Ok(false)) {
            return Ok(false);
        }
        Ok(watch? && actor?)
    }
    pub(super) fn active(&self) -> usize {
        self.contexts.len()
    }
    pub(super) fn root_relocation_pending(&self, root_id: &str) -> bool {
        self.root_rebinds
            .values()
            .any(|intent| intent.root_id == root_id)
    }
}

pub(super) fn validating(operation: String, revision: u64) -> LibraryOperationView {
    LibraryOperationView {
        operation_id: operation,
        root_id: None,
        generation: None,
        state: LibraryOperationState::Running,
        phase: LibraryPhase::ValidatingRoot,
        entries_examined: 0,
        discovered_files: 0,
        prepared_files: 0,
        committed_files: 0,
        skipped_files: 0,
        changed_files: 0,
        missing_files: 0,
        warnings: Vec::new(),
        omitted_warnings: 0,
        error: None,
        repository_revision: revision,
    }
}

pub(super) fn file_authorization(
    repository: &Repository,
    registry: &AuthorityRegistry,
    asset_id: &str,
) -> Result<FileAuthorization> {
    let authority = repository
        .local_asset_authority(asset_id)?
        .context(LibraryError::new(
            LibraryErrorKind::Unavailable,
            "Local file has no current grant",
        ))?;
    ensure!(
        authority.explicit_file_epoch.is_some() || !authority.roots.is_empty(),
        LibraryError::new(
            LibraryErrorKind::Unavailable,
            "Local file has no current grant"
        )
    );
    registry.install(&authority, repository.local_asset_file_identity(asset_id)?)
}

impl Runtime {
    pub(super) fn follow_library_root(
        &mut self,
        token: &LibraryWatchToken,
        path: PathBuf,
        anchor: &FileIdentity,
    ) -> Result<()> {
        let root = self
            .repository
            .library_root(&token.root_id)?
            .context("Library root is unavailable")?;
        let preference = self
            .repository
            .library_watch(&token.root_id)?
            .context("Library watch is unavailable")?;
        ensure!(
            preference.enabled
                && preference.watch_generation == token.watch_generation
                && preference.options.root_move_policy == RootMovePolicy::FollowSameObject
                && root.grant_epoch == token.grant_epoch
                && &root.anchor == anchor,
            LibraryError::conflict("Root movement notification is stale")
        );
        ensure!(
            !self
                .library
                .root_rebinds
                .values()
                .any(|intent| intent.root_id == token.root_id),
            LibraryError::new(LibraryErrorKind::Busy, "Root relocation is pending")
        );
        let binding = self
            .repository
            .library_root_binding(&token.root_id)?
            .context("Root binding is unavailable")?;
        let operation = format!(
            "root-follow-{}-{}-{}",
            token.root_id, token.watch_generation, binding.location_revision
        );
        let previous = std::mem::replace(&mut self.request_id, operation.clone());
        let result = self.rebind_library_root(
            token.root_id.clone(),
            path,
            root.grant_epoch,
            binding.location_revision,
        );
        self.request_id = previous;
        result?;
        if let Some(intent) = self.library.root_rebinds.get_mut(&operation) {
            intent.auto_follow = true;
            intent.watch_generation = Some(token.watch_generation);
        }
        Ok(())
    }
    pub(super) fn begin_library_relink(
        &mut self,
        asset_id: String,
        path: PathBuf,
        expected_binding_revision: u64,
        allow_content_change: bool,
        candidate: Option<LibraryRelinkCandidate>,
    ) -> Result<()> {
        ensure!(
            self.library.relinks.is_empty(),
            LibraryError::new(
                LibraryErrorKind::Busy,
                "A library content verification is already running"
            )
        );
        validate_library_path(&path)?;
        let binding = self
            .repository
            .library_asset_binding(&asset_id)?
            .context("Library asset binding is unavailable")?;
        ensure!(
            binding.binding_revision == expected_binding_revision,
            LibraryError::conflict("Library asset binding changed")
        );
        let asset = self
            .repository
            .local_asset(&asset_id)?
            .context("Library asset is unavailable")?;
        let original = self
            .repository
            .track(&asset.track_id)?
            .context("Library track is unavailable")?;
        let operation = self.request_id.clone();
        let context = ScanContext::new(Duration::from_secs(30));
        self.library.actor(&self.catalog)?.submit_relink(
            operation.clone(),
            path,
            original,
            asset_id,
            context.clone(),
        )?;
        self.library.contexts.insert(operation.clone(), context);
        self.library.relinks.insert(
            operation.clone(),
            RelinkIntent {
                expected_binding_revision,
                allow_content_change,
                candidate,
            },
        );
        let mut view = validating(operation, self.revision);
        view.phase = LibraryPhase::Preparing;
        self.library.remember(view);
        Ok(())
    }

    pub(super) fn confirm_library_relink(
        &mut self,
        candidate_id: &str,
        expected_binding_revision: u64,
        allow_content_change: bool,
    ) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis()
            .min(u64::MAX as u128) as u64;
        let candidate = self
            .repository
            .library_relink_candidate(candidate_id, now)?
            .context("Library relocation candidate has expired")?;
        ensure!(
            candidate.binding_revision == expected_binding_revision,
            LibraryError::conflict("Library relocation candidate binding changed")
        );
        self.begin_library_relink(
            candidate.asset_id.clone(),
            candidate.new_path.clone(),
            expected_binding_revision,
            allow_content_change,
            Some(candidate),
        )
    }

    pub(super) fn begin_library_verification(&mut self) -> Result<()> {
        let roots = self
            .repository
            .library_roots_page(0, MAX_LIBRARY_ROOTS)?
            .items;
        for root in roots {
            self.library.authority.install_root(&root)?;
            if let Some(preference) = self.repository.library_watch(&root.root_id)? {
                self.library.authority.apply_root_move_policy(
                    &root.root_id,
                    root.grant_epoch,
                    preference.options.root_move_policy,
                )?;
            }
            self.library.verification_queue.push_back(root);
        }
        self.pump_library_verification()
    }
    fn pump_library_verification(&mut self) -> Result<()> {
        while self.library.verifying.len() < 2 {
            let Some(root) = self.library.verification_queue.front().cloned() else {
                break;
            };
            let current = self.repository.library_root(&root.root_id)?;
            if current
                .as_ref()
                .is_none_or(|current| current.grant_epoch != root.grant_epoch)
            {
                self.library.verification_queue.pop_front();
                continue;
            }
            let submitted = self
                .library
                .actor(&self.catalog)?
                .submit_verify(root.clone(), ScanContext::new(Duration::from_secs(30)));
            if let Err(error) = submitted {
                if error
                    .downcast_ref::<LibraryError>()
                    .is_some_and(|error| error.kind == LibraryErrorKind::Busy)
                {
                    break;
                }
                return Err(error);
            }
            self.library.verification_queue.pop_front();
            self.library
                .verifying
                .insert(root.root_id, root.grant_epoch);
        }
        Ok(())
    }
    pub(super) fn effective_library_assets(
        &self,
        mut page: LibraryAssetPage,
    ) -> Result<LibraryAssetPage> {
        for view in &mut page.items {
            view.authorized = self
                .repository
                .local_asset_authority(&view.asset.id)?
                .is_some_and(|authority| self.library.authority.effective_authority(&authority));
        }
        Ok(page)
    }
    pub(super) fn effective_library_summary(&self) -> Result<LibrarySummary> {
        let roots = self
            .repository
            .library_roots_page(0, MAX_LIBRARY_ROOTS)?
            .items
            .into_iter()
            .filter(|root| {
                self.library
                    .authority
                    .root_health(&root.root_id, root.grant_epoch)
                    == crate::library::authority::RootHealth::Verified
            })
            .map(|root| (root.root_id, root.grant_epoch))
            .collect::<Vec<_>>();
        self.repository.library_summary_with_verified_roots(&roots)
    }
    pub(super) fn add_library_root(&mut self, path: PathBuf, recursive: bool) -> Result<()> {
        validate_library_path(&path)?;
        let operation = self.request_id.clone();
        let context = ScanContext::new(Duration::from_secs(30));
        self.library.actor(&self.catalog)?.submit_validate(
            operation.clone(),
            path,
            recursive,
            context.clone(),
        )?;
        self.library.contexts.insert(operation.clone(), context);
        self.library.remember(validating(operation, self.revision));
        Ok(())
    }

    pub(super) fn rebind_library_root(
        &mut self,
        root_id: String,
        path: PathBuf,
        expected_grant_epoch: u64,
        expected_location_revision: u64,
    ) -> Result<()> {
        validate_library_path(&path)?;
        let root = self
            .repository
            .library_root(&root_id)?
            .context("Library root is unavailable")?;
        let binding = self
            .repository
            .library_root_binding(&root_id)?
            .context("Library root binding is unavailable")?;
        ensure!(
            root.grant_epoch == expected_grant_epoch
                && binding.location_revision == expected_location_revision,
            LibraryError::conflict("Library root binding has changed")
        );
        let operation = self.request_id.clone();
        let context = ScanContext::new(Duration::from_secs(30));
        self.library.actor(&self.catalog)?.submit_validate(
            operation.clone(),
            path,
            root.recursive,
            context.clone(),
        )?;
        self.library.root_rebinds.insert(
            operation.clone(),
            RootRebindIntent {
                root_id: root_id.clone(),
                grant_epoch: expected_grant_epoch,
                location_revision: expected_location_revision,
                auto_follow: false,
                watch_generation: None,
            },
        );
        self.library.contexts.insert(operation.clone(), context);
        let mut view = validating(operation, self.revision);
        view.root_id = Some(root_id);
        self.library.remember(view);
        Ok(())
    }

    pub(super) fn scan_library_root(
        &mut self,
        root_id: String,
        options: LibraryScanOptions,
    ) -> Result<()> {
        options.validate()?;
        ensure!(
            self.library.tokens.len() < 10,
            LibraryError::budget("Library scan admission capacity reached")
        );
        ensure!(
            !self
                .library
                .tokens
                .values()
                .any(|token| token.root_id == root_id),
            LibraryError::new(
                LibraryErrorKind::Busy,
                "Library root scan is awaiting finalization"
            )
        );
        self.library
            .actor(&self.catalog)?
            .can_submit_scan(&root_id)?;
        let operation = self.request_id.clone();
        let (token, change) = self.repository.begin_library_scan(&root_id, &operation)?;
        self.revision = change.revision;
        let root = self
            .repository
            .library_root(&root_id)?
            .context("Library root disappeared")?;
        let context = ScanContext::new(Duration::from_millis(options.timeout_ms));
        let evidence = self
            .repository
            .library_watch(&root_id)?
            .is_some_and(|watch| watch.enabled && watch.options.auto_relink);
        if evidence {
            let preference = self
                .repository
                .library_watch(&root_id)?
                .context("Content-evidence watch is unavailable")?;
            let watch = LibraryWatchToken {
                root_id: root_id.clone(),
                grant_epoch: root.grant_epoch,
                watch_generation: preference.watch_generation,
                enabled: preference.enabled,
                options: preference.options,
            };
            let view = self
                .library_watch_view(&root_id)?
                .context("Content-evidence watch state is unavailable")?;
            self.library.relocation.cold.begin(
                token.clone(),
                watch,
                LibraryScanScope::Root,
                view.received_sequence,
                view.dropped_events,
            )?;
        }
        let submitted = if evidence {
            self.library
                .actor(&self.catalog)?
                .submit_scan_with_content_evidence(token.clone(), root, options, context.clone())
        } else {
            self.library.actor(&self.catalog)?.submit_scan(
                token.clone(),
                root,
                options,
                context.clone(),
            )
        };
        if let Err(error) = submitted {
            self.library.relocation.cold.remove(&token);
            let mut outcome = LibraryOperationView::running(&token);
            outcome.state = LibraryOperationState::Failed;
            outcome.error = Some(library_error(&error));
            let finished = self
                .repository
                .finish_library_scan(&token, &outcome, false, &|| Ok(()))?;
            self.revision = finished.change.revision;
            return Err(error);
        }
        self.library.contexts.insert(operation, context);
        self.library
            .tokens
            .insert(token.operation_id.clone(), token.clone());
        self.library.remember(LibraryOperationView::running(&token));
        Ok(())
    }

    pub(super) fn remove_library_root(&mut self, root_id: &str) -> Result<()> {
        let ids = self.library_root_asset_ids(root_id)?;
        let change = self.repository.remove_library_root(root_id)?;
        self.revision = change.revision;
        self.revoke_library_root_access(root_id, ids)?;
        self.events.lock().unwrap().publish(
            self.revision,
            &self.origin,
            Some(self.request_id.clone()),
            BackendEvent::LibraryRootChanged {
                root_id: root_id.into(),
                removed: true,
            },
        );
        Ok(())
    }
    fn library_root_asset_ids(&self, root_id: &str) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        let mut cursor = None;
        let mut revision = None;
        loop {
            let page = self.repository.library_assets_page(
                Some(root_id),
                cursor.as_deref(),
                1000,
                revision,
            )?;
            revision = Some(page.revision);
            ids.extend(page.items.into_iter().map(|view| view.asset.id));
            cursor = page.next_asset_id;
            if cursor.is_none() {
                break;
            }
        }
        Ok(ids)
    }
    fn revoke_library_root_access(&mut self, root_id: &str, ids: Vec<String>) -> Result<()> {
        self.retire_library_watch(root_id);
        self.library.authority.revoke_root(root_id);
        if let Some(actor) = &self.library.actor {
            actor.cancel_root(root_id);
        }
        for view in &self.library.operations {
            if view.root_id.as_deref() == Some(root_id)
                && let Some(context) = self.library.contexts.get(&view.operation_id)
            {
                context.cancel.store(true, Ordering::Release);
            }
        }
        let mut unavailable = HashSet::new();
        for id in ids {
            let asset = self
                .repository
                .local_asset(&id)?
                .context("Library asset disappeared")?;
            match file_authorization(&self.repository, &self.library.authority, &id) {
                Ok(authorization) if asset.available => {
                    if let Some(record) = self.repository.track(&asset.track_id)? {
                        self.catalog
                            .restore_local_record(&record, &asset, authorization)?;
                    }
                }
                _ => {
                    unavailable.insert(asset.track_id);
                }
            }
        }
        let removed = unavailable.iter().cloned().collect::<Vec<_>>();
        self.catalog.remove_local_records(&removed);
        self.invalidate_library_tracks(&unavailable);
        self.reconcile_queue()?;
        Ok(())
    }

    fn invalidate_library_tracks(&mut self, tracks: &HashSet<String>) {
        if tracks.is_empty() {
            return;
        }
        if self
            .preload
            .snapshot()
            .candidate
            .as_ref()
            .is_some_and(|entry| tracks.contains(&entry.track_id))
        {
            self.invalidate_preload(Invalidation::ApprovalRevoked);
        }
        self.cancel_local_pictures(tracks);
        if self
            .current_record
            .as_ref()
            .is_some_and(|record| tracks.contains(&record.id))
        {
            self.cancel_playback_resolution();
            self.cancel_lyrics(false);
            self.audio.stop();
            self.play_when_ready = false;
            self.restore_pending = false;
            self.seek_after_load = None;
            self.skip_pending = None;
            self.media_retry_pending = None;
            self.pending = None;
            self.shared.lock().unwrap().pending_playback_url = None;
        }
    }
    pub(super) fn invalidate_relocated_track(&mut self, track_id: &str) {
        self.invalidate_library_tracks(&HashSet::from([track_id.into()]));
    }

    pub(super) fn poll_library(&mut self) {
        for _ in 0..4 {
            let completion = self
                .library
                .actor
                .as_mut()
                .and_then(LibraryActor::try_recv)
                .or_else(|| self.take_deferred_library_finish());
            let Some(completion) = completion else {
                break;
            };
            match completion {
                LibraryCompletion::RootPathPrepared {
                    operation_id,
                    watch_token,
                    result,
                    ack,
                } => {
                    self.complete_cold_reopen(operation_id, watch_token, result, ack);
                }
                LibraryCompletion::LiveRelocationHints {
                    operation_id,
                    root_id,
                    watch_generation,
                    result,
                } => {
                    self.complete_live_relocation_hints(
                        operation_id,
                        root_id,
                        watch_generation,
                        result,
                    );
                }
                LibraryCompletion::AutoRelinkPrepared {
                    operation_id,
                    watch_token,
                    result,
                    ack,
                } => {
                    self.complete_auto_relink(operation_id, watch_token, result, ack);
                }
                LibraryCompletion::RelinkPrepared {
                    operation_id,
                    result,
                    ack,
                } => {
                    let context = self.library.contexts.remove(&operation_id);
                    let intent = self.library.relinks.remove(&operation_id);
                    let finish = (|| -> Result<()> {
                        let context = context.as_ref().context("Library relocation is stale")?;
                        let intent = intent.context("Library relocation intent is unavailable")?;
                        context.check()?;
                        let prepared = result?;
                        if let Some(candidate) = &intent.candidate {
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)?
                                .as_millis()
                                .min(u64::MAX as u128) as u64;
                            let current = self
                                .repository
                                .library_relink_candidate(&candidate.candidate_id, now)?
                                .context("Library relocation candidate has expired")?;
                            ensure!(
                                prepared.prepared.file_identity.as_ref()
                                    == Some(&current.new_identity)
                                    && current.binding_revision == intent.expected_binding_revision
                                    && current
                                        .content_sha256
                                        .as_ref()
                                        .is_none_or(|hash| hash
                                            .eq_ignore_ascii_case(&prepared.content_sha256)),
                                LibraryError::conflict("Library relocation candidate file changed")
                            );
                        }
                        let preview = self.repository.preview_manual_relink_authority(
                            &prepared.prepared,
                            intent.expected_binding_revision,
                        )?;
                        let old_binding = self
                            .repository
                            .library_asset_binding(&prepared.prepared.asset.id)?
                            .context("Relocation original content binding is unavailable")?;
                        let old_asset = self
                            .repository
                            .local_asset(&prepared.prepared.asset.id)?
                            .context("Relocation original asset is unavailable")?;
                        let same_content = old_binding
                            .content_sha256
                            .as_ref()
                            .filter(|_| {
                                old_binding.hash_size_bytes == Some(old_asset.size_bytes)
                                    && old_binding.hash_modified_ms == old_asset.modified_ms
                            })
                            .is_some_and(|hash| {
                                hash.eq_ignore_ascii_case(&prepared.content_sha256)
                            });
                        let ticket = if same_content {
                            self.library.authority.prepare_replacement_same_content(
                                &preview,
                                prepared.prepared.file_identity.clone(),
                                old_binding.content_sha256.as_deref().unwrap(),
                                &prepared.content_sha256,
                            )?
                        } else {
                            self.library.authority.prepare_replacement(
                                &preview,
                                prepared.prepared.file_identity.clone(),
                            )?
                        };
                        let request = LibraryRelinkCommit {
                            operation_id: operation_id.clone(),
                            expected_binding_revision: intent.expected_binding_revision,
                            prepared: prepared.prepared.clone(),
                            authorization: LibraryRelinkAuthorization::Manual,
                            evidence: RelinkEvidence::ManualSelection,
                            content_sha256: Some(prepared.content_sha256.clone()),
                            allow_content_change: intent.allow_content_change,
                        };
                        let committed = self
                            .repository
                            .commit_library_relink(&request, &|| context.check())?;
                        self.revision = committed.change.revision;
                        let authority = self
                            .repository
                            .local_asset_authority(&committed.result.asset_id)?
                            .context("Committed relocation has no file grant")?;
                        let authorization = if same_content {
                            self.library
                                .authority
                                .publish_replacement_same_content(ticket, &authority)?
                        } else {
                            self.library
                                .authority
                                .publish_replacement(ticket, &authority)?
                        };
                        let mut local = prepared.local;
                        local.set_authorization(authorization);
                        self.catalog
                            .commit_rebound(local, committed.result.media_changed)?;
                        if committed.result.media_changed {
                            self.invalidate_library_tracks(&HashSet::from([committed
                                .result
                                .track_id
                                .clone()]));
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
                        Ok(())
                    })();
                    ack.complete(
                        finish
                            .as_ref()
                            .map(|_| ())
                            .map_err(|error| format!("{error:#}")),
                    );
                    let mut view = validating(operation_id.clone(), self.revision);
                    view.state = if finish.is_ok() {
                        LibraryOperationState::Completed
                    } else if context
                        .as_ref()
                        .is_some_and(|context| context.cancel.load(Ordering::Acquire))
                    {
                        LibraryOperationState::Cancelled
                    } else {
                        LibraryOperationState::Failed
                    };
                    view.error = finish.as_ref().err().map(library_error);
                    self.library.remember(view.clone());
                    self.publish_library_terminal(&view);
                    self.publish_finished(operation_id, &finish);
                }
                LibraryCompletion::RootValidated {
                    operation_id,
                    result,
                    proof,
                } => {
                    let context = self.library.contexts.remove(&operation_id);
                    let rebind = self.library.root_rebinds.remove(&operation_id);
                    let outcome = (|| -> Result<_> {
                        context
                            .as_ref()
                            .context("Library validation is stale")?
                            .check()?;
                        let draft = result?;
                        if let Some(intent) = &rebind {
                            let members = self.library_root_asset_ids(&intent.root_id)?;
                            let old = self
                                .repository
                                .library_root(&intent.root_id)?
                                .context("Library root disappeared")?;
                            if let Some(generation) = intent.watch_generation {
                                let preference = self
                                    .repository
                                    .library_watch(&intent.root_id)?
                                    .context("Root watch disappeared")?;
                                ensure!(
                                    preference.enabled && preference.watch_generation == generation,
                                    LibraryError::conflict(
                                        "Root movement watch generation changed"
                                    )
                                );
                            }
                            let mut proof =
                                Some(proof.context("Root relocation has no physical proof")?);
                            let ticket = if old.anchor == draft.anchor && old.path != draft.path {
                                Some(if intent.auto_follow {
                                    self.library
                                        .authority
                                        .prepare_root_relocation(&old, &draft.path)?
                                } else {
                                    self.library.authority.prepare_manual_root_relocation(
                                        &old,
                                        proof.take().unwrap(),
                                    )?
                                })
                            } else {
                                None
                            };
                            let rebound = self.repository.rebind_library_root(
                                &intent.root_id,
                                &draft,
                                intent.grant_epoch,
                                intent.location_revision,
                                !intent.auto_follow,
                                &|| {
                                    context
                                        .as_ref()
                                        .context("Root relocation is stale")?
                                        .check()
                                        .and_then(|_| {
                                            ticket.as_ref().map_or(Ok(()), |ticket| ticket.check())
                                        })
                                },
                            )?;
                            self.revision = rebound.1.revision;
                            if old.grant_epoch != rebound.0.grant_epoch {
                                self.revoke_library_root_access(&intent.root_id, members)?;
                            }
                            if let Some(ticket) = ticket {
                                self.library
                                    .authority
                                    .publish_root_relocation(ticket, &rebound.0)?;
                            } else {
                                self.library.authority.accept_root_proof(
                                    &rebound.0,
                                    proof.context("Root relocation proof was consumed")?,
                                )?;
                            }
                            self.retire_library_watch(&intent.root_id);
                            for asset_id in self.library_root_asset_ids(&intent.root_id)? {
                                if let Some(asset) = self.repository.local_asset(&asset_id)?
                                    && asset.available
                                    && let Some(record) = self.repository.track(&asset.track_id)?
                                {
                                    let authorization = file_authorization(
                                        &self.repository,
                                        &self.library.authority,
                                        &asset_id,
                                    )?;
                                    self.catalog.restore_local_record(
                                        &record,
                                        &asset,
                                        authorization,
                                    )?;
                                }
                            }
                            self.reconcile_queue()?;
                            self.restore_library_watch(&intent.root_id)?;
                            let binding = self
                                .repository
                                .library_root_binding(&intent.root_id)?
                                .context("Root binding disappeared")?;
                            self.events.lock().unwrap().publish(
                                self.revision,
                                "library",
                                Some(operation_id.clone()),
                                BackendEvent::LibraryRootLocationChanged {
                                    root_id: intent.root_id.clone(),
                                    grant_epoch: rebound.0.grant_epoch,
                                    location_revision: binding.location_revision,
                                },
                            );
                            return Ok(rebound);
                        }
                        let key = library_path_key(&draft.path, draft.path_policy)?;
                        let previous = self
                            .repository
                            .library_roots_page(0, MAX_LIBRARY_ROOTS)?
                            .items
                            .into_iter()
                            .find(|root| {
                                library_path_key(&root.path, root.path_policy)
                                    .ok()
                                    .as_deref()
                                    == Some(key.as_str())
                            });
                        let members = previous
                            .as_ref()
                            .map(|root| self.library_root_asset_ids(&root.root_id))
                            .transpose()?
                            .unwrap_or_default();
                        let added = self.repository.add_library_root(&draft)?;
                        self.revision = added.1.revision;
                        if let Some(previous) = previous
                            && previous.grant_epoch != added.0.grant_epoch
                        {
                            self.revoke_library_root_access(&previous.root_id, members)?;
                        }
                        self.library.authority.accept_root_proof(
                            &added.0,
                            proof.context("Library validation has no physical proof")?,
                        )?;
                        Ok(added)
                    })();
                    let mut view = validating(operation_id.clone(), self.revision);
                    let finish = match outcome {
                        Ok((root, change)) => {
                            self.revision = change.revision;
                            view.root_id = Some(root.root_id.clone());
                            view.state = LibraryOperationState::Completed;
                            self.events.lock().unwrap().publish(
                                self.revision,
                                "library",
                                Some(operation_id.clone()),
                                BackendEvent::LibraryRootChanged {
                                    root_id: root.root_id,
                                    removed: false,
                                },
                            );
                            Ok(())
                        }
                        Err(error) => {
                            view.state = if context
                                .as_ref()
                                .is_some_and(|context| context.cancel.load(Ordering::Acquire))
                            {
                                LibraryOperationState::Cancelled
                            } else {
                                LibraryOperationState::Failed
                            };
                            view.error = Some(library_error(&error));
                            Err(error)
                        }
                    };
                    view.repository_revision = self.revision;
                    self.library.remember(view.clone());
                    self.publish_library_terminal(&view);
                    self.publish_finished(operation_id, &finish);
                }
                LibraryCompletion::RootVerified {
                    root_id,
                    grant_epoch,
                    health,
                    error,
                } => {
                    if self.library.verifying.get(&root_id) == Some(&grant_epoch) {
                        self.library.verifying.remove(&root_id);
                    }
                    if health == crate::library::authority::RootHealth::Verified
                        && self
                            .repository
                            .library_root(&root_id)
                            .ok()
                            .flatten()
                            .is_some_and(|root| root.grant_epoch == grant_epoch)
                        && let Err(error) = self.restore_library_watch(&root_id)
                    {
                        self.error(format!("Library watch restore failed: {error:#}"));
                    }
                    if self
                        .repository
                        .library_root(&root_id)
                        .ok()
                        .flatten()
                        .is_some_and(|root| root.grant_epoch == grant_epoch)
                        && health != crate::library::authority::RootHealth::Verified
                        && let Some(error) = error
                    {
                        self.error(format!("Library directory is unavailable: {error:#}"));
                    }
                }
                LibraryCompletion::Batch {
                    token,
                    mut seen,
                    mut prepared,
                    mut progress,
                    ack,
                    _storage,
                    content_evidence,
                } => {
                    let result = (|| -> Result<()> {
                        let context = self
                            .library
                            .contexts
                            .get(&token.operation_id)
                            .context("Library scan is stale")?
                            .clone();
                        self.stage_scan_relocations(&token, &mut prepared, &mut seen)?;
                        let pending_identities = prepared
                            .iter()
                            .map(|(_, item)| item.clone())
                            .collect::<Vec<_>>();
                        let identities = self
                            .repository
                            .prepare_library_identities(&pending_identities, &|| context.check())?;
                        for ((local, item), identity) in prepared.iter_mut().zip(identities) {
                            *item = identity;
                            local.bind_library_identity(item)?;
                            for discovered in &mut seen {
                                if discovered.path == item.asset.path {
                                    discovered.asset_id = Some(item.asset.id.clone());
                                }
                            }
                            if !local.tags_verified()
                                && self.repository.local_asset(&item.asset.id)?.is_some()
                                && let Some(previous) = self.repository.track(&item.track.id)?
                            {
                                local.preserve_metadata_from(&previous)?;
                                item.track = local.library_asset().track;
                            }
                        }
                        let items = prepared
                            .iter()
                            .map(|(_, item)| item.clone())
                            .collect::<Vec<_>>();
                        let mut changed_tracks = HashSet::new();
                        for item in &items {
                            if let Some(previous) = self.repository.local_asset(&item.asset.id)? {
                                let identity =
                                    self.repository.local_asset_file_identity(&item.asset.id)?;
                                let content_changed = if let Some(evidence) =
                                    content_evidence.iter().find(|evidence| {
                                        crate::domain::music::normalize_local_path(&evidence.path)
                                            == crate::domain::music::normalize_local_path(
                                                &item.asset.path,
                                            )
                                    }) {
                                    self.repository
                                        .library_asset_binding(&item.asset.id)?
                                        .is_some_and(|binding| {
                                            binding.content_sha256.as_ref().is_some_and(|old| {
                                                !old.eq_ignore_ascii_case(&evidence.content_sha256)
                                            })
                                        })
                                } else {
                                    false
                                };
                                if previous.path != item.asset.path
                                    || previous.size_bytes != item.asset.size_bytes
                                    || previous.modified_ms != item.asset.modified_ms
                                    || identity.as_ref().is_some_and(|identity| {
                                        item.file_identity.as_ref() != Some(identity)
                                    })
                                    || content_changed
                                {
                                    changed_tracks.insert(item.track.id.clone());
                                }
                            }
                        }
                        let committed = self.repository.apply_library_scan_batch(
                            &token,
                            &seen,
                            &items,
                            &|| context.check(),
                        )?;
                        self.revision = committed.change.revision;
                        let mut evidence_commits = Vec::new();
                        let mut evidence_leases = Vec::new();
                        for evidence in &content_evidence {
                            if let Some(item) = items.iter().find(|item| {
                                item.asset.path == evidence.path
                                    && item.file_identity.as_ref() == Some(&evidence.file_identity)
                            }) {
                                let binding = self
                                    .repository
                                    .library_asset_binding(&item.asset.id)?
                                    .context("Committed content binding is unavailable")?;
                                evidence_commits.push(LibraryContentEvidenceCommit {
                                    asset_id: item.asset.id.clone(),
                                    expected_binding_revision: binding.binding_revision,
                                    file_identity: evidence.file_identity.clone(),
                                    size_bytes: evidence.size_bytes,
                                    modified_ms: evidence.modified_ms,
                                    content_sha256: evidence.content_sha256.clone(),
                                });
                                evidence_leases.push(evidence);
                            }
                        }
                        if !evidence_commits.is_empty() {
                            self.repository.put_library_content_evidence_batch(
                                &token,
                                &evidence_commits,
                                &|| context.check(),
                                &|index| evidence_leases[index].check(),
                            )?;
                        }
                        for (mut local, item) in prepared {
                            let authorization = file_authorization(
                                &self.repository,
                                &self.library.authority,
                                &item.asset.id,
                            )?;
                            local.set_authorization(authorization);
                            self.catalog.commit_local(local);
                        }
                        self.invalidate_library_tracks(&changed_tracks);
                        self.reconcile_queue()?;
                        if let Some(saved) =
                            self.repository.library_operation(&token.operation_id)?
                        {
                            progress.committed_files = saved.committed_files;
                            progress.changed_files = saved.changed_files;
                        }
                        progress.repository_revision = self.revision;
                        self.library.remember(progress.clone());
                        let now = Instant::now();
                        let due = self
                            .library
                            .last_progress
                            .get(&token.operation_id)
                            .is_none_or(|last| {
                                now.duration_since(*last) >= Duration::from_millis(100)
                            });
                        if due {
                            self.library
                                .last_progress
                                .insert(token.operation_id.clone(), now);
                            self.events.lock().unwrap().publish(
                                self.revision,
                                "library",
                                Some(token.operation_id.clone()),
                                BackendEvent::LibraryScanProgress {
                                    state: Box::new(progress.clone()),
                                },
                            );
                        }
                        Ok(())
                    })();
                    ack.complete(
                        result
                            .as_ref()
                            .map(|_| ())
                            .map_err(|error| format!("{error:#}")),
                    );
                    if let Err(error) = result {
                        self.error(format!("Library batch failed: {error:#}"));
                    }
                    drop(seen);
                    drop(progress);
                    drop(token);
                    drop(_storage);
                }
                LibraryCompletion::Finished {
                    token,
                    mut outcome,
                    mut discovery_complete,
                } => {
                    let mut watch_coverage_complete;
                    match self.seal_cold_scan(&token, &outcome, discovery_complete) {
                        Ok((allow_missing, allow_coverage)) => {
                            watch_coverage_complete = allow_coverage;
                            if !allow_missing && !self.relocations_pending(&token.root_id) {
                                discovery_complete = false;
                            }
                        }
                        Err(error) => {
                            discovery_complete = false;
                            watch_coverage_complete = false;
                            outcome.state = LibraryOperationState::Partial;
                            outcome.error = Some(library_error(&error));
                        }
                    }
                    if self.relocations_pending(&token.root_id) {
                        let bytes = self
                            .library
                            .relocation
                            .deferred_finishes
                            .iter()
                            .map(|completion| match completion {
                                LibraryCompletion::Finished { token, outcome, .. } => {
                                    serde_json::to_vec(&(token, outcome))
                                        .map_or(MAX_LIBRARY_PAGE_BYTES, |bytes| bytes.len())
                                }
                                _ => MAX_LIBRARY_PAGE_BYTES,
                            })
                            .sum::<usize>();
                        if self.library.relocation.deferred_finishes.len() < 10
                            && bytes
                                + serde_json::to_vec(&(&token, &outcome))
                                    .map_or(MAX_LIBRARY_PAGE_BYTES, |bytes| bytes.len())
                                <= MAX_LIBRARY_PAGE_BYTES
                        {
                            self.library.relocation.deferred_finishes.push_back(
                                LibraryCompletion::Finished {
                                    token,
                                    outcome,
                                    discovery_complete,
                                },
                            );
                            continue;
                        }
                        self.retire_root_relocations(&token.root_id);
                        discovery_complete = false;
                        watch_coverage_complete = false;
                        outcome.state = LibraryOperationState::Failed;
                        outcome.error = Some(LibraryError::budget(
                            "Deferred library finalization exceeds its budget",
                        ));
                    }
                    self.library.tokens.remove(&token.operation_id);
                    self.retire_cold_scan(&token);
                    let context = self.library.contexts.remove(&token.operation_id);
                    self.library.last_progress.remove(&token.operation_id);
                    if let Ok(Some(saved)) = self.repository.library_operation(&token.operation_id)
                        && saved.state != LibraryOperationState::Running
                    {
                        let finish = if matches!(
                            saved.state,
                            LibraryOperationState::Completed | LibraryOperationState::Partial
                        ) {
                            Ok(())
                        } else {
                            Err(saved
                                .error
                                .clone()
                                .unwrap_or_else(|| {
                                    LibraryError::new(
                                        LibraryErrorKind::Cancelled,
                                        "Library scan grant revoked",
                                    )
                                })
                                .into())
                        };
                        self.library.remember(saved.clone());
                        self.finish_watch_scan(&saved, false);
                        self.publish_library_terminal(&saved);
                        self.publish_finished(token.operation_id, &finish);
                        continue;
                    }
                    let result = self.repository.finish_library_scan(
                        &token,
                        &outcome,
                        discovery_complete,
                        &|| {
                            context
                                .as_ref()
                                .context("Library scan context is unavailable")?
                                .check()
                        },
                    );
                    let mut terminal = outcome;
                    let finish = match result {
                        Ok(finished) => {
                            self.revision = finished.change.revision;
                            let mut missing = HashSet::new();
                            for id in finished.missing_asset_ids {
                                if let Ok(Some(asset)) = self.repository.local_asset(&id) {
                                    missing.insert(asset.track_id);
                                }
                            }
                            self.catalog
                                .remove_local_records(&missing.iter().cloned().collect::<Vec<_>>());
                            self.invalidate_library_tracks(&missing);
                            if let Ok(Some(saved)) =
                                self.repository.library_operation(&token.operation_id)
                            {
                                terminal = saved;
                            }
                            let _ = self.reconcile_queue();
                            if terminal.state == LibraryOperationState::Completed
                                || terminal.state == LibraryOperationState::Partial
                            {
                                Ok(())
                            } else {
                                Err(terminal
                                    .error
                                    .clone()
                                    .unwrap_or_else(|| {
                                        LibraryError::new(
                                            LibraryErrorKind::Cancelled,
                                            "Library scan ended without completion",
                                        )
                                    })
                                    .into())
                            }
                        }
                        Err(error) => {
                            terminal.state = if context
                                .as_ref()
                                .is_some_and(|context| context.cancel.load(Ordering::Acquire))
                            {
                                LibraryOperationState::Cancelled
                            } else {
                                LibraryOperationState::Failed
                            };
                            terminal.error = Some(library_error(&error));
                            if let Ok(finished) = self.repository.finish_library_scan(
                                &token,
                                &terminal,
                                false,
                                &|| Ok(()),
                            ) {
                                self.revision = finished.change.revision;
                            }
                            if let Ok(Some(saved)) =
                                self.repository.library_operation(&token.operation_id)
                            {
                                terminal = saved;
                            }
                            Err(error)
                        }
                    };
                    terminal.repository_revision = self.revision;
                    self.library.remember(terminal.clone());
                    self.finish_watch_scan(&terminal, watch_coverage_complete);
                    self.publish_library_terminal(&terminal);
                    self.publish_finished(token.operation_id, &finish);
                }
            }
        }
        if let Err(error) = self.pump_library_verification() {
            self.error(format!("Library verification failed: {error:#}"));
        }
    }
    fn publish_library_terminal(&self, view: &LibraryOperationView) {
        self.events.lock().unwrap().publish(
            self.revision,
            "library",
            Some(view.operation_id.clone()),
            BackendEvent::LibraryScanFinished {
                state: Box::new(view.clone()),
            },
        );
    }
    pub(super) fn library_operation(&self, id: &str) -> Result<Option<LibraryOperationView>> {
        if let Some(saved) = self.repository.library_operation(id)?
            && saved.state != LibraryOperationState::Running
        {
            return Ok(Some(saved));
        }
        if let Some(view) = self
            .library
            .operations
            .iter()
            .find(|view| view.operation_id == id)
        {
            return Ok(Some(view.clone()));
        }
        self.repository.library_operation(id)
    }
    pub(super) fn library_operation_latest(&self) -> Option<String> {
        self.library
            .operations
            .back()
            .map(|operation| operation.operation_id.clone())
    }
    pub(super) fn shutdown_library_operations(&mut self) {
        let ids = self.library.contexts.keys().cloned().collect::<Vec<_>>();
        for id in ids {
            self.library.cancel(&id);
            let mut view = self
                .library_operation(&id)
                .ok()
                .flatten()
                .unwrap_or_else(|| validating(id.clone(), self.revision));
            if view.state == LibraryOperationState::Running {
                view.state = LibraryOperationState::Cancelled;
                view.error = Some(LibraryError::new(
                    LibraryErrorKind::Cancelled,
                    "Backend shutting down",
                ));
                if let Some(token) = self.library.tokens.get(&id) {
                    match self
                        .repository
                        .finish_library_scan(token, &view, false, &|| Ok(()))
                    {
                        Ok(finished) => {
                            self.revision = finished.change.revision;
                        }
                        Err(error) => {
                            view.state = LibraryOperationState::Failed;
                            view.error = Some(library_error(&error));
                        }
                    }
                }
                view.repository_revision = self.revision;
                self.library.remember(view.clone());
                self.publish_library_terminal(&view);
                self.publish_finished(id, &Err(view.error.unwrap().into()));
            }
        }
        self.library.contexts.clear();
        self.library.tokens.clear();
        self.library.last_progress.clear();
        self.library.verification_queue.clear();
        self.library.verifying.clear();
        self.library.root_rebinds.clear();
        self.library.relinks.clear();
        self.library.relocation = Default::default();
    }
    pub(super) fn library_operations(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<LibraryOperationPage> {
        ensure!(
            (1..=1000).contains(&limit),
            LibraryError::invalid("Invalid library operation page limit")
        );
        let saved = self
            .repository
            .library_operations_page(0, MAX_LIBRARY_OPERATIONS)?;
        let mut rows = saved.items;
        for view in &self.library.operations {
            if rows.iter().any(|old| {
                old.operation_id == view.operation_id && old.state != LibraryOperationState::Running
            }) {
                continue;
            }
            rows.retain(|old| old.operation_id != view.operation_id);
            rows.push(view.clone());
        }
        rows.sort_by(|left, right| left.operation_id.cmp(&right.operation_id));
        if rows.len() > MAX_LIBRARY_OPERATIONS {
            rows.drain(..rows.len() - MAX_LIBRARY_OPERATIONS);
        }
        let total = rows.len();
        let page = LibraryOperationPage {
            items: rows.into_iter().skip(offset).take(limit).collect(),
            total,
            offset,
            limit,
            revision: self.revision,
        };
        ensure!(
            serde_json::to_vec(&page)?.len() <= MAX_LIBRARY_PAGE_BYTES,
            LibraryError::budget("Library operation page exceeds its byte limit")
        );
        Ok(page)
    }
}

pub(super) fn library_error(error: &anyhow::Error) -> LibraryError {
    if let Some(error) = error.downcast_ref::<LibraryError>() {
        return error.clone();
    }
    let text = format!("{error:#}");
    let kind = if error.is::<crate::library::scan::ScanError>() {
        match error
            .downcast_ref::<crate::library::scan::ScanError>()
            .unwrap()
        {
            crate::library::scan::ScanError::Cancelled => LibraryErrorKind::Cancelled,
            crate::library::scan::ScanError::DeadlineExceeded => LibraryErrorKind::Timeout,
            crate::library::scan::ScanError::BudgetExceeded => LibraryErrorKind::BudgetExceeded,
            crate::library::scan::ScanError::UnsupportedPath => LibraryErrorKind::InvalidData,
        }
    } else if error.is::<rusqlite::Error>() {
        LibraryErrorKind::Storage
    } else {
        LibraryErrorKind::Unavailable
    };
    LibraryError::new(kind, text.chars().take(1024).collect::<String>())
}

pub(super) fn backend_library_error_ref(error: &anyhow::Error) -> Option<BackendError> {
    let error = error.downcast_ref::<LibraryError>()?;
    let kind = match error.kind {
        LibraryErrorKind::InvalidData => ErrorKind::InvalidData,
        LibraryErrorKind::Conflict => ErrorKind::Conflict,
        LibraryErrorKind::Busy | LibraryErrorKind::BudgetExceeded => ErrorKind::Busy,
        LibraryErrorKind::Cancelled => ErrorKind::Cancelled,
        LibraryErrorKind::Timeout => ErrorKind::Timeout,
        LibraryErrorKind::Storage => ErrorKind::Storage,
        LibraryErrorKind::Unavailable => ErrorKind::Unavailable,
    };
    Some(BackendError {
        kind,
        message: error.message.clone(),
        retryable: error.retryable,
    })
}
