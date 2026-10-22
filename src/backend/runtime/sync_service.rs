use super::*;
use crate::backend::sync_service::{
    ManagedSyncRole, SyncManagementRequest, SyncManagementResponse, SyncOwnerGate, SyncPeerView,
    SyncRequest, SyncResponse,
};
use crate::sync::Dataset;
use crate::sync::network::{SessionTicket, SyncData, SyncOperation};
use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{
        Weak,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Default)]
pub(super) struct SyncServiceRuntime {
    epoch: u64,
    session: Option<Weak<SyncOwnerGate>>,
    tickets: HashMap<String, SessionTicket>,
    pending_reconcile: bool,
}
impl SyncServiceRuntime {
    pub(super) fn begin_shutdown(&mut self) {
        if let Some(gate) = self.session.as_ref().and_then(Weak::upgrade) {
            gate.begin_close();
        }
        for ticket in self.tickets.values() {
            ticket.retire();
        }
    }
}
impl Runtime {
    pub(super) fn open_sync_session(&mut self) -> Result<Arc<SyncOwnerGate>, BackendError> {
        if let Some(old) = self.sync_service.session.as_ref().and_then(Weak::upgrade) {
            if !old.closing.load(Ordering::Acquire) {
                return Err(BackendError::new(
                    ErrorKind::Busy,
                    "Retire active sync owner before replacing it",
                ));
            }
            if !old.retirement_complete() {
                return Err(BackendError::new(
                    ErrorKind::Busy,
                    "Sync owner still has pending effects",
                ));
            }
            let state = self
                .repository
                .sync_profile_state()
                .map_err(backend_error)?;
            if state.profile_epoch == old.profile_epoch {
                self.repository
                    .sync_rotate_profile(old.profile_epoch)
                    .map_err(backend_error)?;
            }
        }
        for ticket in self.sync_service.tickets.drain().map(|(_, ticket)| ticket) {
            self.repository
                .sync_retire_session(&ticket)
                .map_err(backend_error)?;
        }
        self.sync_service.epoch =
            self.sync_service.epoch.checked_add(1).ok_or_else(|| {
                BackendError::new(ErrorKind::Closed, "Sync owner epoch exhausted")
            })?;
        let profile = self
            .repository
            .sync_profile_state()
            .map_err(backend_error)?;
        let gate = SyncOwnerGate::new(self.sync_service.epoch, profile.profile_epoch);
        self.sync_service.session = Some(Arc::downgrade(&gate));
        Ok(gate)
    }
    fn check_sync_owner(
        &self,
        gate: &Arc<SyncOwnerGate>,
        cancel: &AtomicBool,
    ) -> Result<(), BackendError> {
        gate.check(cancel)?;
        if gate.epoch != self.sync_service.epoch
            || !self
                .sync_service
                .session
                .as_ref()
                .and_then(Weak::upgrade)
                .is_some_and(|actual| Arc::ptr_eq(&actual, gate))
        {
            return Err(BackendError::new(
                ErrorKind::Cancelled,
                "Sync owner registry identity changed",
            ));
        }
        if self
            .repository
            .sync_profile_state()
            .map_err(backend_error)?
            .profile_epoch
            != gate.profile_epoch
        {
            return Err(BackendError::new(
                ErrorKind::Cancelled,
                "Sync profile epoch changed",
            ));
        }
        Ok(())
    }
    pub(super) fn sync_request(
        &mut self,
        gate: Arc<SyncOwnerGate>,
        cancel: Arc<AtomicBool>,
        request: SyncRequest,
    ) -> Result<SyncResponse, BackendError> {
        if matches!(&request, SyncRequest::CloseOwner) {
            if self
                .sync_service
                .session
                .as_ref()
                .and_then(Weak::upgrade)
                .is_some_and(|current| Arc::ptr_eq(&current, &gate))
            {
                gate.begin_close();
                for ticket in self.sync_service.tickets.drain().map(|(_, ticket)| ticket) {
                    self.repository
                        .sync_retire_session(&ticket)
                        .map_err(backend_error)?;
                }
                if self
                    .repository
                    .sync_profile_state()
                    .map_err(backend_error)?
                    .profile_epoch
                    == gate.profile_epoch
                {
                    self.repository
                        .sync_rotate_profile(gate.profile_epoch)
                        .map_err(backend_error)?;
                }
                self.sync_service.session = None;
            }
            gate.mark_retired();
            return Ok(SyncResponse::Unit);
        }
        // Cleanup only removes the same lifetime. A closed old owner cannot
        // remove a new connection by reusing its public peer/epoch fields.
        if let SyncRequest::Retire { ticket } = &request {
            self.repository
                .sync_retire_session(ticket)
                .map_err(backend_error)?;
            if self
                .sync_service
                .tickets
                .get(&ticket.peer_id)
                .is_some_and(|old| old.same_lifetime(ticket))
            {
                self.sync_service.tickets.remove(&ticket.peer_id);
            }
            return Ok(SyncResponse::Unit);
        }
        self.check_sync_owner(&gate, &cancel)?;
        let _operation = gate.operation(&cancel)?;
        let response = match request {
            SyncRequest::Peers => {
                SyncResponse::Peers(self.repository.sync_peers().map_err(backend_error)?)
            }
            SyncRequest::ConfigurePairing {
                epoch,
                enabled,
                role,
            } => {
                gate.configure_role(role)?;
                self.repository
                    .sync_configure_pairing_with_commit_guard(
                        gate.profile_epoch,
                        epoch,
                        enabled,
                        &|| gate.check(&cancel).map_err(anyhow::Error::from),
                        || gate.effect_guard(&cancel).map_err(anyhow::Error::from),
                    )
                    .map_err(backend_error)?;
                SyncResponse::Unit
            }
            SyncRequest::StorePeer { epoch, peer } => {
                let check = || gate.check(&cancel).map_err(anyhow::Error::from);
                match gate.role()? {
                    ManagedSyncRole::Server => self
                        .repository
                        .sync_store_server_peer_with_commit_guard(epoch, &peer, &check, || {
                            gate.effect_guard(&cancel).map_err(anyhow::Error::from)
                        }),
                    ManagedSyncRole::Client => self
                        .repository
                        .sync_store_client_peer_with_commit_guard(epoch, &peer, &check, || {
                            gate.effect_guard(&cancel).map_err(anyhow::Error::from)
                        }),
                }
                .map_err(backend_error)?;
                SyncResponse::Unit
            }
            SyncRequest::Begin { ticket } => {
                if !self.sync_service.tickets.contains_key(&ticket.peer_id)
                    && self.sync_service.tickets.len() >= 8
                {
                    return Err(BackendError::new(
                        ErrorKind::Busy,
                        "Sync ticket mirror capacity reached",
                    ));
                }
                self.repository
                    .sync_begin_session_with_commit_guard(
                        &ticket,
                        &|| gate.check(&cancel).map_err(anyhow::Error::from),
                        || gate.effect_guard(&cancel).map_err(anyhow::Error::from),
                    )
                    .map_err(backend_error)?;
                self.sync_service
                    .tickets
                    .insert(ticket.peer_id.clone(), ticket);
                SyncResponse::Unit
            }
            SyncRequest::Snapshot { ticket } => {
                let result = self
                    .repository
                    .sync_snapshot(&ticket)
                    .map_err(backend_error)?;
                self.check_sync_owner(&gate, &cancel)?;
                self.repository
                    .sync_check_session(&ticket, None)
                    .map_err(backend_error)?;
                SyncResponse::Snapshot(result)
            }
            SyncRequest::Baseline { ticket, dataset } => SyncResponse::Baseline(
                self.repository
                    .sync_baseline(&ticket, dataset)
                    .map_err(backend_error)?,
            ),
            SyncRequest::Changes {
                ticket,
                sequence,
                limit,
            } => {
                let result = self
                    .repository
                    .sync_changes_after(&ticket, sequence, limit)
                    .map_err(backend_error)?;
                self.check_sync_owner(&gate, &cancel)?;
                self.repository
                    .sync_check_session(&ticket, None)
                    .map_err(backend_error)?;
                SyncResponse::Changes(result)
            }
            SyncRequest::Mode { ticket, dataset } => SyncResponse::Mode(
                self.repository
                    .sync_mode(&ticket, dataset)
                    .map_err(backend_error)?,
            ),
            SyncRequest::Confirm {
                ticket,
                data,
                revision,
                sequence,
                digest,
            } => {
                self.repository
                    .sync_confirm_baseline_with_commit_guard(
                        &ticket,
                        &data,
                        revision,
                        sequence,
                        &digest,
                        &|| gate.check(&cancel).map_err(anyhow::Error::from),
                        || gate.effect_guard(&cancel).map_err(anyhow::Error::from),
                    )
                    .map_err(backend_error)?;
                SyncResponse::Unit
            }
            SyncRequest::Apply {
                ticket,
                expected_sequence,
                operation,
            } => {
                let expected_sequence = if expected_sequence.is_none()
                    && matches!(
                        &operation,
                        SyncOperation::List(crate::sync::ListAction::DataOverwrite(_))
                            | SyncOperation::Dislike(crate::sync::DislikeAction::DataOverwrite(_))
                    ) {
                    Some(
                        self.repository
                            .sync_profile_state()
                            .map_err(backend_error)?
                            .sequence,
                    )
                } else {
                    expected_sequence
                };
                // The finalizer acquires this guard only after bulk SQL writes.
                // Keep it through native runtime effects, then release before
                // the actor dispatcher sends the actual committed receipt.
                let final_guard = RefCell::new(None);
                let (change, receipt) = self
                    .repository
                    .sync_apply_remote_with_commit_guard(
                        &ticket,
                        expected_sequence,
                        &operation,
                        &|| gate.check(&cancel).map_err(anyhow::Error::from),
                        || {
                            *final_guard.borrow_mut() =
                                Some(gate.effect_guard(&cancel).map_err(anyhow::Error::from)?);
                            Ok(())
                        },
                    )
                    .map_err(backend_error)?;
                self.revision = change.revision;
                self.origin = "sync".into();
                self.request_id = format!("sync-{}-{}", gate.epoch, change.revision);
                let effects = if change.kind == crate::repository::ChangeKind::ListsChanged {
                    self.sync_post_lists(&operation)
                } else {
                    self.sync_post_dislike()
                };
                if let Err(error) = effects {
                    self.sync_service.pending_reconcile = true;
                    self.error(format!(
                        "Committed sync data requires runtime reconciliation: {error:#}"
                    ));
                }
                drop(final_guard);
                SyncResponse::Receipt(receipt)
            }
            SyncRequest::Revoke { peer_id } => {
                if let Some(ticket) = self.sync_service.tickets.get(&peer_id) {
                    ticket.retire();
                    ticket.complete_retirement();
                }
                self.repository
                    .sync_revoke_peer_with_commit_guard(
                        &peer_id,
                        &|| gate.check(&cancel).map_err(anyhow::Error::from),
                        || gate.effect_guard(&cancel).map_err(anyhow::Error::from),
                    )
                    .map_err(backend_error)?;
                self.sync_service.tickets.remove(&peer_id);
                SyncResponse::Unit
            }
            SyncRequest::Retire { .. } => unreachable!(),
            SyncRequest::CloseOwner => unreachable!(),
        };
        if !matches!(&response, SyncResponse::Receipt(_)) {
            gate.check(&cancel)?;
        }
        Ok(response)
    }
    pub(super) fn sync_management(
        &mut self,
        request: SyncManagementRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<SyncManagementResponse, BackendError> {
        if cancel.load(Ordering::Acquire) {
            return Err(BackendError::new(
                ErrorKind::Cancelled,
                "Sync management request retired",
            ));
        }
        match request {
            SyncManagementRequest::ClientCredentials { server_id } => {
                let credentials = self
                    .repository
                    .sync_client_credentials(&server_id)
                    .map_err(backend_error)?;
                if cancel.load(Ordering::Acquire) {
                    return Err(BackendError::new(
                        ErrorKind::Cancelled,
                        "Saved credential query retired",
                    ));
                }
                Ok(SyncManagementResponse::ClientCredentials(credentials))
            }
            SyncManagementRequest::Peers => {
                let owner = self
                    .sync_service
                    .session
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .filter(|gate| !gate.closing.load(Ordering::Acquire));
                let peers = self
                    .repository
                    .sync_peers()
                    .map_err(backend_error)?
                    .into_iter()
                    .map(|peer| {
                        let ticket = owner
                            .as_ref()
                            .and_then(|_| self.sync_service.tickets.get(&peer.peer_id))
                            .filter(|ticket| ticket.is_active());
                        let mode = |dataset| {
                            ticket
                                .filter(|_| match dataset {
                                    Dataset::List => peer.capabilities.list.is_some(),
                                    Dataset::Dislike => peer.capabilities.dislike.is_some(),
                                })
                                .map(|ticket| self.repository.sync_mode(ticket, dataset))
                                .transpose()
                                .map(Option::flatten)
                        };
                        Ok(SyncPeerView {
                            list_mode: mode(Dataset::List)?,
                            dislike_mode: mode(Dataset::Dislike)?,
                            owner_epoch: owner.as_ref().map(|gate| gate.epoch),
                            session_epoch: ticket.map(|ticket| ticket.session_epoch),
                            peer_id: peer.peer_id,
                            client_id: peer.client_id,
                            device_name: peer.device_name,
                            is_mobile: peer.is_mobile,
                            auth_epoch: peer.auth_epoch,
                            capabilities: peer.capabilities,
                        })
                    })
                    .collect::<Result<Vec<_>>>()
                    .map_err(backend_error)?;
                Ok(SyncManagementResponse::Peers(peers))
            }
            SyncManagementRequest::ChooseMode {
                owner_epoch,
                peer_id,
                auth_epoch,
                session_epoch,
                dataset,
                mode,
            } => {
                let gate = self
                    .sync_service
                    .session
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .ok_or_else(|| BackendError::new(ErrorKind::Closed, "No active sync owner"))?;
                self.check_sync_owner(&gate, &cancel)?;
                if gate.epoch != owner_epoch {
                    return Err(BackendError::new(
                        ErrorKind::Cancelled,
                        "Mode owner epoch changed",
                    ));
                }
                let ticket = self.sync_service.tickets.get(&peer_id).ok_or_else(|| {
                    BackendError::new(ErrorKind::Unavailable, "Sync peer has no active session")
                })?;
                if ticket.auth_epoch != auth_epoch || ticket.session_epoch != session_epoch {
                    return Err(BackendError::new(
                        ErrorKind::Cancelled,
                        "Mode peer session changed",
                    ));
                }
                self.repository
                    .sync_choose_mode_with_commit_guard(
                        ticket,
                        dataset,
                        mode,
                        &|| gate.check(&cancel).map_err(anyhow::Error::from),
                        || gate.effect_guard(&cancel).map_err(anyhow::Error::from),
                    )
                    .map_err(backend_error)?;
                Ok(SyncManagementResponse::Unit)
            }
        }
    }
    fn sync_post_lists(&mut self, operation: &SyncOperation) -> Result<()> {
        let previous = self.queue.current.clone();
        let action = match operation {
            SyncOperation::Replace(SyncData::List(_)) => "list_data_overwrite",
            SyncOperation::List(action) => sync_list_action_name(action),
            _ => "list_data_overwrite",
        };
        self.events.lock().unwrap().publish(
            self.revision,
            &self.origin,
            Some(self.request_id.clone()),
            BackendEvent::ListsChanged {
                revision: self.revision,
                action: action.into(),
            },
        );
        if let Err(error) = self.reconcile_queue() {
            self.queue.set_list(DEFAULT_LIST.into(), Vec::new());
            self.reconcile_queue().context(error.to_string())?;
        }
        self.queue.invalidate_plan();
        self.failed_entries.clear();
        self.skip_pending = None;
        self.save_checkpoint()?;
        if let Some(current) = previous {
            let exists = current.list_id == "catalog"
                || self
                    .repository
                    .list_infos()?
                    .iter()
                    .any(|list| list.id == current.list_id);
            let removed = current.list_id != "catalog"
                && (!exists
                    || self
                        .repository
                        .list_track(&current.list_id, &current.track_id)?
                        .is_none());
            if removed && !current.temporary {
                let active = self.play_when_ready
                    && !self.restore_pending
                    && (self.busy
                        || matches!(
                            self.audio.snapshot().status,
                            PlaybackStatus::Playing | PlaybackStatus::Buffering
                        ));
                if active {
                    self.advance_queue(AdvanceReason::Ended, false)?;
                } else {
                    self.pending_promotion = None;
                    self.cancel_playback_resolution();
                    self.cancel_lyrics(true);
                    self.generation = self.generation.wrapping_add(1);
                    self.pending = None;
                    {
                        let mut snapshot = self.shared.lock().unwrap();
                        snapshot.pending_playback_url = None;
                        snapshot.approved_audio_identity = None;
                    }
                    self.audio.stop();
                    self.queue.clear_current();
                    self.current_record = None;
                    self.restore_pending = false;
                    self.restored_position_ms = 0;
                    self.seek_after_load = None;
                    self.save_checkpoint()?;
                }
            }
        }
        Ok(())
    }
    fn sync_post_dislike(&mut self) -> Result<()> {
        self.dislike = self.repository.dislike_rules()?;
        self.queue.invalidate_plan();
        self.failed_entries.clear();
        self.skip_pending = None;
        self.events.lock().unwrap().publish(
            self.revision,
            &self.origin,
            Some(self.request_id.clone()),
            BackendEvent::DislikeChanged {
                revision: self.revision,
            },
        );
        self.save_checkpoint()?;
        Ok(())
    }
    pub(super) fn poll_sync_service(&mut self) {
        if let Some(gate) = self.sync_service.session.as_ref().and_then(Weak::upgrade)
            && gate.closing.load(Ordering::Acquire)
            && let Err(error) = self.sync_request(
                gate,
                Arc::new(AtomicBool::new(false)),
                SyncRequest::CloseOwner,
            )
        {
            self.error(format!("Sync retirement failed: {error}"));
        }
        if !self.sync_service.pending_reconcile {
            return;
        }
        if self
            .reconcile_queue()
            .and_then(|_| self.sync_post_dislike())
            .is_ok()
        {
            self.sync_service.pending_reconcile = false;
        }
    }
    pub(super) fn retire_sync_service(&mut self) -> Result<(), BackendError> {
        self.sync_service.begin_shutdown();
        if let Some(gate) = self.sync_service.session.as_ref().and_then(Weak::upgrade) {
            self.sync_request(
                gate,
                Arc::new(AtomicBool::new(false)),
                SyncRequest::CloseOwner,
            )?;
        }
        Ok(())
    }
}
fn sync_list_action_name(action: &crate::sync::ListAction) -> &'static str {
    use crate::sync::ListAction::*;
    match action {
        DataOverwrite(_) => "list_data_overwrite",
        Create { .. } => "list_create",
        Remove(_) => "list_remove",
        Update(_) => "list_update",
        UpdatePosition { .. } => "list_update_position",
        MusicAdd { .. } => "list_music_add",
        MusicMove { .. } => "list_music_move",
        MusicRemove { .. } => "list_music_remove",
        MusicUpdate(_) => "list_music_update",
        MusicUpdatePosition { .. } => "list_music_update_position",
        MusicOverwrite { .. } => "list_music_overwrite",
        MusicClear(_) => "list_music_clear",
    }
}
