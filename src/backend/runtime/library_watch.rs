use super::*;
use crate::domain::{library::*, library_watch::*};
use crate::library::watch::{NativeWatch, NativeWatchEvent};

#[derive(Default)]
pub(super) struct WatchRuntime {
    pub(super) native: Option<NativeWatch>,
    entries: HashMap<String, WatchEntry>,
    intents: HashMap<String, WatchIntent>,
    recovery: HashMap<String, Instant>,
    owner_failed: bool,
    lyric_dirty: Option<(String, u64)>,
    scan_serial: u64,
}

struct WatchEntry {
    view: LibraryWatchView,
    token: LibraryWatchToken,
    first_dirty: Option<Instant>,
    last_dirty: Option<Instant>,
    scan: Option<(String, u64)>,
    rename_old: Option<(PathBuf, Option<u32>, Instant)>,
}

struct WatchIntent {
    operation_id: String,
    token: LibraryWatchToken,
    context: crate::library::scan::ScanContext,
}

fn empty_watch_entry(token: LibraryWatchToken, preference: LibraryWatchPreference) -> WatchEntry {
    WatchEntry {
        view: LibraryWatchView {
            preference,
            grant_epoch: token.grant_epoch,
            state: LibraryWatchState::Starting,
            received_sequence: 0,
            covered_sequence: 0,
            pending_paths: 0,
            full_rescan_required: true,
            dropped_events: 0,
            last_scan_operation_id: None,
            error: None,
        },
        token,
        first_dirty: None,
        last_dirty: None,
        scan: None,
        rename_old: None,
    }
}

impl WatchRuntime {
    pub(super) fn shutdown(&mut self, timeout: Duration) -> Result<bool> {
        for intent in self.intents.values() {
            intent.context.cancel.store(true, Ordering::Release);
        }
        match &mut self.native {
            Some(native) => native.shutdown(timeout),
            None => Ok(true),
        }
    }
}

impl Runtime {
    fn fail_watch_subscription(&mut self, token: &LibraryWatchToken, error: LibraryError) {
        if let Some(entry) = self.library.watch.entries.get_mut(&token.root_id)
            && entry.token == *token
        {
            entry.view.state = LibraryWatchState::Degraded;
            entry.view.error = Some(error);
            entry.first_dirty = None;
            entry.last_dirty = None;
            let first = self
                .library
                .watch
                .recovery
                .insert(
                    token.root_id.clone(),
                    Instant::now() + Duration::from_secs(1),
                )
                .is_none();
            if first && let Some(native) = &self.library.watch.native {
                let _ = native.stop(&token.root_id, token.watch_generation);
            }
        }
    }

    pub(super) fn poll_library_watch(&mut self) {
        for _ in 0..4 {
            let event = self
                .library
                .watch
                .native
                .as_mut()
                .and_then(NativeWatch::try_recv);
            let Some(event) = event else {
                break;
            };
            match event {
                NativeWatchEvent::Prepared { token, result } => {
                    self.complete_watch_intent(
                        &token.root_id,
                        Some(token.watch_generation),
                        result,
                    );
                }
                NativeWatchEvent::Stopped {
                    root_id,
                    watch_generation,
                } => {
                    if self
                        .library
                        .watch
                        .intents
                        .get(&root_id)
                        .is_some_and(|intent| {
                            !intent.token.enabled
                                && intent.token.watch_generation.checked_sub(1)
                                    == Some(watch_generation)
                        })
                    {
                        self.complete_watch_intent(&root_id, None, Ok(()));
                    } else if self
                        .library
                        .watch
                        .entries
                        .get(&root_id)
                        .is_some_and(|entry| entry.token.watch_generation == watch_generation)
                    {
                        self.library.watch.entries.remove(&root_id);
                        if self
                            .repository
                            .library_watch(&root_id)
                            .ok()
                            .flatten()
                            .is_some_and(|preference| preference.enabled)
                        {
                            self.library
                                .watch
                                .recovery
                                .insert(root_id.clone(), Instant::now());
                        }
                    }
                }
                NativeWatchEvent::Watching { token } => {
                    if let Some(entry) = self.library.watch.entries.get_mut(&token.root_id)
                        && entry.token == token
                        && entry.first_dirty.is_some()
                    {
                        entry.view.state = LibraryWatchState::RescanPending;
                    }
                }
                NativeWatchEvent::Dirty {
                    token,
                    sequence,
                    hints,
                    full_rescan_required,
                } => {
                    self.refresh_watched_sidecar(&token, &hints);
                    if token.options.auto_relink {
                        self.queue_native_relocation_hints(&token, &hints);
                    }
                    let stale = self
                        .library
                        .watch
                        .entries
                        .get(&token.root_id)
                        .is_some_and(|entry| sequence < entry.view.received_sequence);
                    if !stale
                        && let Err(error) = self.accept_watch_dirty(
                            &token,
                            sequence,
                            full_rescan_required,
                            hints.len(),
                            0,
                        )
                    {
                        self.error(format!("Library watch event failed: {error:#}"));
                    }
                }
                NativeWatchEvent::RootMoved {
                    token,
                    current_path,
                    anchor,
                } => {
                    if token.options.root_move_policy == RootMovePolicy::FollowSameObject
                        && let Err(error) = self.follow_library_root(&token, current_path, &anchor)
                        && !error
                            .downcast_ref::<LibraryError>()
                            .is_some_and(|error| error.kind == LibraryErrorKind::Busy)
                    {
                        self.error(format!("Library root follow failed: {error:#}"));
                    }
                    if let Some(entry) = self.library.watch.entries.get_mut(&token.root_id)
                        && entry.token == token
                    {
                        entry.view.state = LibraryWatchState::Degraded;
                        entry.view.error = Some(LibraryError::new(
                            LibraryErrorKind::Unavailable,
                            "Library root moved and requires location verification",
                        ));
                        entry.view.full_rescan_required = true;
                    }
                    self.publish_watch_view(&token.root_id, None);
                }
                NativeWatchEvent::Error { token, error } => {
                    self.fail_watch_subscription(&token, error);
                    self.publish_watch_view(&token.root_id, None);
                }
                NativeWatchEvent::OwnerError { error } => {
                    self.library.watch.owner_failed = true;
                    for entry in self.library.watch.entries.values_mut() {
                        entry.view.state = LibraryWatchState::Degraded;
                        entry.view.error = Some(error.clone());
                    }
                    self.error(format!("Library watch owner failed: {}", error.message));
                }
            }
        }
        let tokens = self
            .library
            .watch
            .entries
            .values()
            .map(|entry| entry.token.clone())
            .collect::<Vec<_>>();
        for token in tokens {
            let dirty = self
                .library
                .watch
                .native
                .as_ref()
                .and_then(|native| native.dirty(&token.root_id, token.watch_generation));
            let Some(dirty) = dirty else {
                continue;
            };
            if let Some(path) = dirty.root_moved_path.as_ref()
                && token.options.root_move_policy == RootMovePolicy::FollowSameObject
                && let Ok(Some(root)) = self.repository.library_root(&token.root_id)
                && root.path != *path
            {
                if let Err(error) = self.follow_library_root(&token, path.clone(), &root.anchor)
                    && !error
                        .downcast_ref::<LibraryError>()
                        .is_some_and(|error| error.kind == LibraryErrorKind::Busy)
                {
                    self.error(format!("Library root follow recovery failed: {error:#}"));
                }
                continue;
            }
            if let Some(error) = dirty.error {
                self.fail_watch_subscription(&token, error);
                continue;
            }
            let stale = self
                .library
                .watch
                .entries
                .get(&token.root_id)
                .is_some_and(|entry| {
                    dirty.received_sequence < entry.view.received_sequence
                        || (dirty.received_sequence <= entry.view.covered_sequence
                            && !entry.view.full_rescan_required)
                });
            if stale {
                continue;
            }
            let changed = self
                .library
                .watch
                .entries
                .get(&token.root_id)
                .is_some_and(|entry| {
                    dirty.received_sequence != entry.view.received_sequence
                        || dirty.full_rescan_required != entry.view.full_rescan_required
                        || dirty.dropped_events != entry.view.dropped_events
                });
            if changed {
                if let Err(error) = self.accept_watch_dirty(
                    &token,
                    dirty.received_sequence,
                    dirty.full_rescan_required,
                    dirty.pending_paths,
                    dirty.dropped_events,
                ) {
                    self.error(format!("Library watch recovery failed: {error:#}"));
                } else if dirty.dropped_events > 0 {
                    self.events.lock().unwrap().publish(
                        self.revision,
                        "library",
                        None,
                        BackendEvent::LibraryWatchOverflow {
                            root_id: token.root_id.clone(),
                            watch_generation: token.watch_generation,
                            dropped_events: dirty.dropped_events,
                        },
                    );
                }
            }
        }
        let expired = self
            .library
            .watch
            .intents
            .iter()
            .filter(|(_, intent)| intent.context.check().is_err())
            .map(|(root_id, intent)| (root_id.clone(), intent.token.watch_generation))
            .collect::<Vec<_>>();
        for (root_id, generation) in expired {
            if let Some(native) = &self.library.watch.native {
                let _ = native.stop(&root_id, generation);
            }
            self.complete_watch_intent(
                &root_id,
                Some(generation),
                Err(LibraryError::new(
                    LibraryErrorKind::Cancelled,
                    "Library watch change cancelled or expired",
                )
                .into()),
            );
        }
        if self
            .library
            .watch
            .native
            .as_ref()
            .is_some_and(NativeWatch::is_finished)
        {
            self.library.watch.owner_failed = true;
        }
        if self.library.watch.owner_failed {
            let stopped = self
                .library
                .watch
                .native
                .as_mut()
                .map(|native| native.shutdown(Duration::ZERO));
            if matches!(stopped, None | Some(Ok(true)) | Some(Err(_))) {
                self.library.watch.native = None;
                self.library.watch.owner_failed = false;
                let intents = self
                    .library
                    .watch
                    .intents
                    .iter()
                    .map(|(root_id, intent)| (root_id.clone(), intent.token.watch_generation))
                    .collect::<Vec<_>>();
                for (root_id, generation) in intents {
                    self.complete_watch_intent(
                        &root_id,
                        Some(generation),
                        Err(LibraryError::new(
                            LibraryErrorKind::Unavailable,
                            "Native watch owner stopped",
                        )
                        .into()),
                    );
                }
                for (root_id, entry) in self.library.watch.entries.drain() {
                    if entry.token.enabled {
                        self.library.watch.recovery.insert(root_id, Instant::now());
                    }
                }
            }
        }
        self.pump_watch_scans();
        self.pump_auto_relocations();
        self.pump_watched_lyrics();
        let recover = self
            .library
            .watch
            .recovery
            .iter()
            .filter(|(root_id, due)| {
                **due <= Instant::now() && !self.library.watch.intents.contains_key(*root_id)
            })
            .map(|(root_id, _)| root_id.clone())
            .take(2)
            .collect::<Vec<_>>();
        for root_id in recover {
            let preference = self.repository.library_watch(&root_id).ok().flatten();
            if preference
                .as_ref()
                .is_none_or(|preference| !preference.enabled)
            {
                self.library.watch.recovery.remove(&root_id);
                continue;
            }
            let preference = preference.unwrap();
            let still_owned = self
                .library
                .watch
                .native
                .as_ref()
                .and_then(|native| native.dirty(&root_id, preference.watch_generation))
                .is_some();
            if still_owned {
                self.library
                    .watch
                    .recovery
                    .insert(root_id, Instant::now() + Duration::from_secs(1));
                continue;
            }
            self.library.watch.entries.remove(&root_id);
            if let Err(error) = self.restore_library_watch(&root_id) {
                self.error(format!("Library watch recovery failed: {error:#}"));
            }
            self.library
                .watch
                .recovery
                .insert(root_id, Instant::now() + Duration::from_secs(1));
        }
    }

    pub(super) fn retire_library_watch(&mut self, root_id: &str) {
        self.retire_root_relocations(root_id);
        let entry = self.library.watch.entries.remove(root_id);
        self.library.watch.recovery.remove(root_id);
        let intent = self.library.watch.intents.remove(root_id);
        if let Some(native) = &mut self.library.watch.native {
            if let Some(entry) = &entry {
                let _ = native.stop(root_id, entry.token.watch_generation);
            }
            if let Some(intent) = &intent {
                let _ = native.stop(root_id, intent.token.watch_generation);
            }
        }
        if let Some(intent) = intent {
            intent.context.cancel.store(true, Ordering::Release);
            self.library.contexts.remove(&intent.operation_id);
            self.publish_finished(
                intent.operation_id,
                &Err(
                    LibraryError::new(LibraryErrorKind::Cancelled, "Library root grant changed")
                        .into(),
                ),
            );
        }
    }

    pub(super) fn restore_library_watch(&mut self, root_id: &str) -> Result<()> {
        let Some(preference) = self.repository.library_watch(root_id)? else {
            return Ok(());
        };
        if !preference.enabled
            || self.library.watch.intents.contains_key(root_id)
            || self.library.watch.entries.contains_key(root_id)
        {
            return Ok(());
        }
        let operation = format!("watch-restore-{root_id}");
        let previous = std::mem::replace(&mut self.request_id, operation);
        let result = self.set_library_watch(root_id.into(), true, preference.options);
        self.request_id = previous;
        result
    }

    fn complete_watch_intent(
        &mut self,
        root_id: &str,
        prepared_generation: Option<u64>,
        native_result: Result<()>,
    ) {
        let Some(intent) = self.library.watch.intents.remove(root_id) else {
            return;
        };
        if prepared_generation.is_some_and(|generation| generation != intent.token.watch_generation)
        {
            self.library.watch.intents.insert(root_id.into(), intent);
            return;
        }
        let result = (|| -> Result<()> {
            native_result?;
            intent.context.check()?;
            let (preference, change) = self
                .repository
                .commit_library_watch(&intent.token, &|| intent.context.check())?;
            self.revision = change.revision;
            self.library.authority.apply_root_move_policy(
                root_id,
                intent.token.grant_epoch,
                preference.options.root_move_policy,
            )?;
            if intent.token.enabled {
                self.library
                    .watch
                    .native
                    .as_mut()
                    .context("Native watch owner is unavailable")?
                    .activate(intent.token.clone())?;
                let entry = empty_watch_entry(intent.token.clone(), preference);
                self.library.watch.entries.insert(root_id.into(), entry);
                let dirty = self
                    .repository
                    .library_dirty(root_id)?
                    .context("Library dirty state is unavailable")?;
                // Baseline coverage shares the native sequence space. No
                // synthetic increment may collide with the first OS event.
                self.accept_watch_dirty(&intent.token, dirty.received_sequence, true, 0, 0)?;
            } else {
                self.library.watch.entries.remove(root_id);
            }
            Ok(())
        })();
        if result.is_err()
            && intent.token.enabled
            && let Some(native) = &mut self.library.watch.native
        {
            let _ = native.stop(root_id, intent.token.watch_generation);
        }
        if result.is_ok() {
            self.library.watch.recovery.remove(root_id);
        } else if !intent.token.enabled
            || self
                .repository
                .library_watch(root_id)
                .ok()
                .flatten()
                .is_some_and(|preference| {
                    preference.watch_generation == intent.token.watch_generation
                        && preference.enabled
                })
        {
            self.library
                .watch
                .recovery
                .insert(root_id.into(), Instant::now() + Duration::from_secs(1));
        }
        self.library.contexts.remove(&intent.operation_id);
        let mut view = super::library::validating(intent.operation_id.clone(), self.revision);
        view.root_id = Some(root_id.into());
        view.state = if result.is_ok() {
            LibraryOperationState::Completed
        } else if intent.context.cancel.load(Ordering::Acquire) {
            LibraryOperationState::Cancelled
        } else {
            LibraryOperationState::Failed
        };
        view.error = result.as_ref().err().map(super::library::library_error);
        self.library.remember(view);
        self.publish_watch_view(root_id, Some(intent.operation_id.clone()));
        self.publish_finished(intent.operation_id, &result);
    }

    fn accept_watch_dirty(
        &mut self,
        token: &LibraryWatchToken,
        sequence: u64,
        full: bool,
        pending_paths: usize,
        dropped_events: u64,
    ) -> Result<()> {
        let preference = self
            .repository
            .library_watch(&token.root_id)?
            .context("Watch root is unavailable")?;
        let root = self
            .repository
            .library_root(&token.root_id)?
            .context("Watch root is unavailable")?;
        if root.grant_epoch != token.grant_epoch
            || preference.watch_generation != token.watch_generation
            || !preference.enabled
        {
            return Ok(());
        }
        let dirty = self.repository.mark_library_dirty(
            &token.root_id,
            token.grant_epoch,
            token.watch_generation,
            sequence,
            full,
            &|| Ok(()),
        )?;
        let entry = self
            .library
            .watch
            .entries
            .entry(token.root_id.clone())
            .or_insert_with(|| empty_watch_entry(token.clone(), preference));
        if entry.token != *token {
            return Ok(());
        }
        let now = Instant::now();
        if dirty.received_sequence > entry.view.received_sequence || full {
            entry.first_dirty.get_or_insert(now);
            entry.last_dirty = Some(now);
        }
        entry.view.received_sequence = dirty.received_sequence;
        entry.view.covered_sequence = dirty.covered_sequence;
        entry.view.full_rescan_required = dirty.full_rescan_required;
        entry.view.pending_paths = pending_paths.min(MAX_LIBRARY_BATCH_ITEMS);
        entry.view.dropped_events = entry.view.dropped_events.max(dropped_events);
        if dirty.received_sequence > dirty.covered_sequence || dirty.full_rescan_required {
            entry.view.state = LibraryWatchState::RescanPending;
        }
        Ok(())
    }

    pub(super) fn finish_watch_scan(
        &mut self,
        outcome: &LibraryOperationView,
        discovery_complete: bool,
    ) {
        let Some(root_id) = outcome.root_id.as_ref() else {
            return;
        };
        let Some(entry) = self.library.watch.entries.get_mut(root_id) else {
            return;
        };
        let Some((operation, sequence)) = entry.scan.as_ref() else {
            return;
        };
        if operation != &outcome.operation_id {
            return;
        }
        let sequence = *sequence;
        entry.scan = None;
        let complete = discovery_complete
            && matches!(
                outcome.state,
                LibraryOperationState::Completed | LibraryOperationState::Partial
            );
        let metadata_partial = complete
            && outcome.state == LibraryOperationState::Partial
            && (outcome.skipped_files > 0
                || !outcome.warnings.is_empty()
                || outcome.omitted_warnings > 0
                || outcome.error.is_some());
        let covered = self.repository.cover_library_dirty(
            root_id,
            entry.token.grant_epoch,
            entry.token.watch_generation,
            sequence,
            complete,
            &|| Ok(()),
        );
        match covered {
            Ok(dirty) => {
                if complete
                    && let Some(native) = &self.library.watch.native
                    && let Err(error) = native.cover(&entry.token, sequence, true)
                {
                    entry.view.error = Some(super::library::library_error(&error));
                }
                entry.view.received_sequence = dirty.received_sequence;
                entry.view.covered_sequence = dirty.covered_sequence;
                entry.view.full_rescan_required = dirty.full_rescan_required;
                entry.view.error = outcome.error.clone();
                if complete
                    && dirty.received_sequence == dirty.covered_sequence
                    && !dirty.full_rescan_required
                {
                    entry.first_dirty = None;
                    entry.last_dirty = None;
                    entry.view.pending_paths = 0;
                    entry.view.state = if metadata_partial {
                        LibraryWatchState::Degraded
                    } else {
                        LibraryWatchState::Watching
                    };
                    entry.view.error = metadata_partial.then(|| {
                        LibraryError::new(
                            LibraryErrorKind::Unavailable,
                            "Some indexed audio files could not be refreshed",
                        )
                    });
                } else {
                    entry.view.state = if outcome.error.is_some() {
                        LibraryWatchState::Degraded
                    } else {
                        LibraryWatchState::RescanPending
                    };
                    // A failed scan retains the gap and retries at a bounded cadence.
                    entry.first_dirty = Some(Instant::now());
                    entry.last_dirty = entry.first_dirty;
                }
            }
            Err(error) => {
                entry.view.state = LibraryWatchState::Degraded;
                entry.view.error = Some(super::library::library_error(&error));
            }
        }
        self.publish_watch_view(root_id, Some(outcome.operation_id.clone()));
    }

    fn pump_watch_scans(&mut self) {
        let now = Instant::now();
        let roots = self
            .library
            .watch
            .entries
            .iter()
            .filter_map(|(root_id, entry)| {
                if !entry.token.enabled
                    || entry.scan.is_some()
                    || entry.first_dirty.is_none()
                    || self.library.watch.recovery.contains_key(root_id)
                    || self.library.root_relocation_pending(root_id)
                {
                    return None;
                }
                let quiet = entry.last_dirty.is_some_and(|last| {
                    now.duration_since(last)
                        >= Duration::from_millis(entry.token.options.debounce_ms)
                });
                let maximum = entry.first_dirty.is_some_and(|first| {
                    now.duration_since(first)
                        >= Duration::from_millis(entry.token.options.max_wait_ms)
                });
                (quiet || maximum).then(|| (root_id.clone(), entry.view.received_sequence))
            })
            .take(2)
            .collect::<Vec<_>>();
        for (root_id, sequence) in roots {
            if self.relocations_pending(&root_id) {
                continue;
            }
            if let Some(token) = self
                .library
                .watch
                .entries
                .get(&root_id)
                .map(|entry| entry.token.clone())
            {
                match self.prepare_live_relocations(&token, sequence) {
                    Ok(true) => continue,
                    Err(error) => {
                        if !error
                            .downcast_ref::<LibraryError>()
                            .is_some_and(|error| error.kind == LibraryErrorKind::Busy)
                        {
                            self.error(format!("Library relocation preflight failed: {error:#}"));
                        }
                        continue;
                    }
                    Ok(false) => {}
                }
            }
            self.library.watch.scan_serial = self.library.watch.scan_serial.saturating_add(1);
            let operation = format!("watch-scan-{}-{}", root_id, self.library.watch.scan_serial);
            let prior_request = std::mem::replace(&mut self.request_id, operation.clone());
            let result = self.scan_library_root(root_id.clone(), LibraryScanOptions::default());
            self.request_id = prior_request;
            if let Some(entry) = self.library.watch.entries.get_mut(&root_id) {
                match result {
                    Ok(()) => {
                        entry.scan = Some((operation.clone(), sequence));
                        entry.view.last_scan_operation_id = Some(operation);
                        entry.view.state = LibraryWatchState::RescanPending;
                    }
                    Err(error) => {
                        if !error
                            .downcast_ref::<LibraryError>()
                            .is_some_and(|error| error.kind == LibraryErrorKind::Busy)
                        {
                            entry.view.state = LibraryWatchState::Degraded;
                            entry.view.error = Some(super::library::library_error(&error));
                            entry.first_dirty = Some(now);
                            entry.last_dirty = Some(now);
                        }
                    }
                }
            }
        }
    }

    fn queue_native_relocation_hints(
        &mut self,
        token: &LibraryWatchToken,
        hints: &[crate::library::watch::NativeWatchHint],
    ) {
        use crate::library::watch::NativeWatchKind;
        let Some(root) = self.repository.library_root(&token.root_id).ok().flatten() else {
            return;
        };
        for hint in hints {
            let path = root.path.join(&hint.path);
            match hint.kind {
                NativeWatchKind::RenamedOld => {
                    if let Some(entry) = self.library.watch.entries.get_mut(&token.root_id)
                        && entry.token == *token
                    {
                        entry.rename_old = Some((path, hint.rename_cookie, Instant::now()));
                    }
                }
                NativeWatchKind::RenamedNew => {
                    let old = self
                        .library
                        .watch
                        .entries
                        .get_mut(&token.root_id)
                        .and_then(|entry| entry.rename_old.take());
                    if let Some((old, cookie, at)) = old
                        && at.elapsed() <= Duration::from_secs(2)
                        && (cookie.is_none() || cookie == hint.rename_cookie)
                        && let Ok(Some(binding)) = self.repository.library_asset_at_path(&old)
                        && let Err(error) = self.queue_auto_relink(token, &binding.id, path)
                    {
                        self.error(format!("Library rename hint failed: {error:#}"));
                    }
                }
                _ => {}
            }
        }
    }

    fn refresh_watched_sidecar(
        &mut self,
        token: &LibraryWatchToken,
        hints: &[crate::library::watch::NativeWatchHint],
    ) {
        let Some(record) = self
            .current_record
            .as_ref()
            .filter(|record| record.platform.source == "local")
        else {
            return;
        };
        let Some(asset_id) = record.local_asset_id.as_ref() else {
            return;
        };
        let Ok(Some(asset)) = self.repository.local_asset(asset_id) else {
            return;
        };
        let Ok(Some(authority)) = self.repository.local_asset_authority(asset_id) else {
            return;
        };
        let Some(root) = authority
            .roots
            .iter()
            .find(|root| root.root_id == token.root_id && root.grant_epoch == token.grant_epoch)
        else {
            return;
        };
        let changed = hints.iter().any(|hint| {
            let path = root.path.join(&hint.path);
            ["lrc", "krc"].iter().any(|extension| {
                crate::domain::music::normalize_local_path(&path)
                    == crate::domain::music::normalize_local_path(
                        &asset.path.with_extension(extension),
                    )
            })
        });
        if changed {
            let id = record.id.clone();
            let generation = self.audio.snapshot().generation;
            self.library.watch.lyric_dirty = Some((id, generation));
        }
    }

    fn pump_watched_lyrics(&mut self) {
        let Some((id, generation)) = self.library.watch.lyric_dirty.as_ref() else {
            return;
        };
        let audio = self.audio.snapshot();
        if audio.generation != *generation || audio.track_id.as_deref() != Some(id.as_str()) {
            self.library.watch.lyric_dirty = None;
            return;
        }
        if self.library.watch.entries.values().any(|entry| {
            entry.scan.is_some() || entry.first_dirty.is_some() || entry.view.full_rescan_required
        }) || self.tasks.len() >= 4
        {
            return;
        }
        let (id, generation) = self.library.watch.lyric_dirty.take().unwrap();
        self.cancel_lyrics(false);
        if let Err(error) = self.begin_lyrics(id, generation, None, 0) {
            self.error(format!("Watched lyrics refresh failed: {error:#}"));
        }
    }

    pub(super) fn set_library_watch(
        &mut self,
        root_id: String,
        enabled: bool,
        options: LibraryWatchOptions,
    ) -> Result<()> {
        let root = self
            .repository
            .library_root(&root_id)?
            .context("Library root is unavailable")?;
        let preference = self
            .repository
            .library_watch(&root_id)?
            .context("Library watch preference is unavailable")?;
        let token = self.repository.prepare_library_watch(
            &root_id,
            enabled,
            &options,
            Some(preference.watch_generation),
        )?;
        ensure!(
            !self.library.watch.intents.contains_key(&root_id),
            LibraryError::new(LibraryErrorKind::Busy, "Library watch change is pending")
        );
        if self.library.watch.native.is_none() {
            self.library.watch.native = Some(NativeWatch::new(self.library.authority.clone())?);
        }
        let context = crate::library::scan::ScanContext::new(Duration::from_secs(30));
        let dirty = self.repository.library_dirty(&root_id)?;
        let sequence = dirty.as_ref().map_or(0, |dirty| dirty.received_sequence);
        if enabled {
            self.library
                .watch
                .native
                .as_mut()
                .unwrap()
                .prepare(root, token.clone(), sequence)?;
        } else {
            // The old generation remains owned until its native requests drain.
            self.library
                .watch
                .native
                .as_mut()
                .unwrap()
                .stop(&root_id, preference.watch_generation)?;
        }
        self.library
            .contexts
            .insert(self.request_id.clone(), context.clone());
        let mut view = super::library::validating(self.request_id.clone(), self.revision);
        view.root_id = Some(root_id.clone());
        self.library.remember(view);
        self.library.watch.intents.insert(
            root_id,
            WatchIntent {
                operation_id: self.request_id.clone(),
                token,
                context,
            },
        );
        Ok(())
    }

    fn publish_watch_view(&self, root_id: &str, operation_id: Option<String>) {
        if let Ok(Some(state)) = self.library_watch_view(root_id) {
            self.events.lock().unwrap().publish(
                self.revision,
                "library",
                operation_id,
                BackendEvent::LibraryWatchChanged {
                    state: Box::new(state),
                },
            );
        }
    }

    pub(super) fn library_watch_view(&self, root_id: &str) -> Result<Option<LibraryWatchView>> {
        let Some(preference) = self.repository.library_watch(root_id)? else {
            return Ok(None);
        };
        let root = self
            .repository
            .library_root(root_id)?
            .context("Watch root is unavailable")?;
        if let Some(entry) = self.library.watch.entries.get(root_id)
            && entry.token.grant_epoch == root.grant_epoch
            && entry.token.watch_generation == preference.watch_generation
        {
            let mut view = entry.view.clone();
            view.preference = preference;
            if let Some(intent) = self.library.watch.intents.get(root_id) {
                view.state = if intent.token.enabled {
                    LibraryWatchState::Starting
                } else {
                    LibraryWatchState::Stopping
                };
            }
            if self.library.watch.recovery.contains_key(root_id) {
                view.state = LibraryWatchState::Degraded;
                view.error = Some(LibraryError::new(
                    LibraryErrorKind::Unavailable,
                    "Native subscription is recovering",
                ));
            }
            return Ok(Some(view));
        }
        let dirty = self.repository.library_dirty(root_id)?;
        let state = if self.library.watch.recovery.contains_key(root_id) {
            LibraryWatchState::Degraded
        } else if let Some(intent) = self.library.watch.intents.get(root_id) {
            if intent.token.enabled {
                LibraryWatchState::Starting
            } else {
                LibraryWatchState::Stopping
            }
        } else if preference.enabled {
            LibraryWatchState::Starting
        } else {
            LibraryWatchState::Disabled
        };
        Ok(Some(LibraryWatchView {
            grant_epoch: root.grant_epoch,
            preference,
            state,
            received_sequence: dirty.as_ref().map_or(0, |dirty| dirty.received_sequence),
            covered_sequence: dirty.as_ref().map_or(0, |dirty| dirty.covered_sequence),
            full_rescan_required: dirty
                .as_ref()
                .is_some_and(|dirty| dirty.full_rescan_required),
            pending_paths: 0,
            dropped_events: 0,
            last_scan_operation_id: None,
            error: None,
        }))
    }

    pub(super) fn library_watch_page(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<LibraryWatchRuntimePage> {
        let page = self.repository.library_watches_page(offset, limit)?;
        let mut items = Vec::with_capacity(page.items.len());
        for preference in page.items {
            if let Some(view) = self.library_watch_view(&preference.root_id)? {
                items.push(view);
            }
        }
        let page = LibraryWatchRuntimePage {
            items,
            total: page.total,
            offset,
            limit,
            revision: page.revision,
        };
        ensure!(
            serde_json::to_vec(&page)?.len() <= MAX_LIBRARY_PAGE_BYTES,
            LibraryError::budget("Library watch page exceeds its byte limit")
        );
        Ok(page)
    }
}
