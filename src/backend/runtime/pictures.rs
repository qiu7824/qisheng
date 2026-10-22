use super::*;
use crate::domain::artwork::*;
use crate::pictures::service::{
    PictureAction, PictureContext, PictureError, PictureService, PictureSpec, PreparedPicture,
    now_ms,
};

pub(super) struct PictureSourceRequest {
    pub(super) operation: String,
    pub(super) request: crate::source::SourceRequest,
    pub(super) cancel: Arc<AtomicBool>,
    pub(super) deadline: Instant,
}

struct Operation {
    list_id: String,
    spec: PictureSpec,
    cancel: Arc<AtomicBool>,
    cache_epoch: u64,
    source_epoch: Option<u64>,
    source_info: Option<Value>,
    receipt: String,
    receipt_pending: bool,
    deadline: Instant,
    weight: usize,
    view: PictureOperationView,
}
pub(super) struct PictureRuntime {
    service: PictureService,
    operations: HashMap<String, Operation>,
    history: VecDeque<PictureOperationView>,
    bytes: usize,
    epoch: u64,
    publication_revision: u64,
}
pub(super) struct OpenApiArtwork {
    pub key_sha256: String,
    pub sha256: String,
    pub mime_type: &'static str,
    pub bytes: Arc<[u8]>,
}
impl PictureRuntime {
    pub(super) fn new(
        repository: Arc<Repository>,
        registry: Arc<ProviderRegistry>,
        authority: crate::library::authority::AuthorityRegistry,
    ) -> Result<Self> {
        Ok(Self {
            service: PictureService::with_authority(repository, registry, authority)?,
            operations: HashMap::new(),
            history: VecDeque::new(),
            bytes: 0,
            epoch: 0,
            publication_revision: 0,
        })
    }
    pub(super) fn begin_shutdown(&self) {
        for operation in self.operations.values() {
            operation.cancel.store(true, Ordering::Release);
        }
        self.service.begin_shutdown();
    }
    pub(super) fn shutdown(&self, timeout: Duration) -> bool {
        self.begin_shutdown();
        self.service.shutdown(timeout)
    }
    fn page(&self, offset: usize, limit: usize) -> Result<Vec<PictureOperationView>> {
        ensure!(
            limit <= 100 && offset <= 200,
            "Picture operation page exceeds its limit"
        );
        let mut views: Vec<_> = self
            .operations
            .values()
            .map(|operation| operation.view.clone())
            .chain(self.history.iter().cloned())
            .collect();
        views.sort_by(|a, b| a.operation_id.cmp(&b.operation_id));
        Ok(views.into_iter().skip(offset).take(limit).collect())
    }
}
impl Runtime {
    fn picture_spec(&self, list_id: &str, music_id: &str) -> Result<PictureSpec> {
        let original = if list_id == "catalog" {
            self.repository.track(music_id)?
        } else {
            self.repository.list_track(list_id, music_id)?
        }
        .context("Picture track is unavailable")?;
        let matched = self.current_matched_record(&original);
        let local = if original.platform.source == "local" {
            let track = self
                .catalog
                .track(&original.id)
                .context("Local pictures require a catalog-authorized asset")?;
            let asset = if let Some(id) = &original.local_asset_id {
                self.repository.local_asset(id)?
            } else {
                self.repository.local_asset_for_track(&original.id)?
            };
            if !track.is_demo {
                ensure!(
                    asset
                        .as_ref()
                        .is_some_and(|asset| asset.available && asset.track_id == original.id),
                    "Local picture asset permission is unavailable"
                );
            }
            asset
        } else {
            None
        };
        let permission = local
            .as_ref()
            .map(|asset| {
                crate::pictures::service::local_picture_permission(&self.repository, asset)
            })
            .transpose()?;
        let authority = if original.platform.source == "local" {
            let source = self.data.selected.as_ref().and_then(|id| {
                self.data
                    .sources
                    .iter()
                    .find(|source| &source.sha256 == id && source.approved)
            });
            if let Some(source) = source {
                source.require_approval()?;
                ArtworkAuthority::Source {
                    source_sha256: source.sha256.clone(),
                    policy_version: u64::from(source.approval_policy_version),
                    local_permission_sha256: permission,
                }
            } else {
                let asset = local
                    .as_ref()
                    .context("Catalog track has no embedded artwork asset")?;
                ArtworkAuthority::LocalAsset {
                    asset_id: asset.id.clone(),
                    permission_sha256: permission.context("Local picture permission is missing")?,
                }
            }
        } else {
            ArtworkAuthority::Native
        };
        let key = ArtworkKey::new(&original, matched.as_ref(), authority)?;
        Ok(PictureSpec {
            key,
            original,
            matched,
            local,
        })
    }
    fn picture_current(&self, operation: &Operation) -> bool {
        if operation.cancel.load(Ordering::Acquire) || operation.cache_epoch != self.pictures.epoch
        {
            return false;
        }
        let Ok(current) = self.picture_spec(&operation.list_id, &operation.spec.original.id) else {
            return false;
        };
        if current.key != operation.spec.key {
            return false;
        }
        if matches!(
            operation.spec.key.authority,
            ArtworkAuthority::Source { .. }
        ) && operation
            .source_epoch
            .is_some_and(|epoch| epoch != self.source_epoch)
        {
            return false;
        }
        true
    }
    pub(super) fn begin_picture(&mut self, list_id: String, music_id: String) -> Result<()> {
        let spec = self.picture_spec(&list_id, &music_id)?;
        let weight = serde_json::to_vec(&spec)?.len();
        ensure!(
            self.pictures.operations.len() < 100
                && weight <= (8 * 1024 * 1024usize).saturating_sub(self.pictures.bytes),
            BackendError::new(ErrorKind::Busy, "Too many pending picture operations")
        );
        let operation = self.request_id.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let source_info = matches!(spec.key.authority, ArtworkAuthority::Source { .. })
            .then(|| self.source_music_info(&spec.original));
        let tx = self.completed_tx.clone();
        let target = operation.clone();
        let deadline = Instant::now() + Duration::from_secs(30);
        self.pictures.service.submit(
            operation.clone(),
            spec.clone(),
            PictureAction::Resolve,
            PictureContext {
                cancel: cancel.clone(),
                deadline,
            },
            move |result| {
                let _ = tx.send(Completion::Picture {
                    operation: target,
                    result: result.map(Box::new),
                });
            },
        )?;
        let view = PictureOperationView {
            operation_id: operation.clone(),
            original_track_id: spec.original.id.clone(),
            state: PictureState::Resolving,
            info: None,
            cached: false,
            error: None,
        };
        self.pictures.bytes += weight;
        self.pictures.operations.insert(
            operation.clone(),
            Operation {
                list_id,
                spec,
                cancel,
                cache_epoch: self.pictures.epoch,
                source_epoch: None,
                source_info,
                receipt: operation,
                receipt_pending: true,
                deadline,
                weight,
                view,
            },
        );
        Ok(())
    }
    pub(super) fn approve_picture(&mut self, operation: &str, url: &str) -> Result<()> {
        let current = self
            .pictures
            .operations
            .get(operation)
            .context("Picture operation is unavailable")?;
        ensure!(
            current.view.state == PictureState::AwaitingApproval,
            "Picture operation is not awaiting URL approval"
        );
        ensure!(
            self.picture_current(current),
            BackendError::new(
                ErrorKind::Conflict,
                "Picture approval refers to changed metadata or permissions"
            )
        );
        let info = current
            .view
            .info
            .clone()
            .context("Picture information is missing")?;
        ensure!(
            info.expire_at_ms > now_ms(),
            BackendError::new(
                ErrorKind::Conflict,
                "Picture information expired; resolve it again"
            )
        );
        ensure!(
            info.url
                .as_ref()
                .is_some_and(|expected| expected.as_str() == url),
            BackendError::new(
                ErrorKind::Conflict,
                "Picture approval URL does not exactly match this operation"
            )
        );
        let spec = current.spec.clone();
        let cancel = current.cancel.clone();
        let target = operation.to_owned();
        let tx = self.completed_tx.clone();
        let deadline = Instant::now() + Duration::from_secs(30);
        self.pictures.service.submit(
            operation.to_owned(),
            spec,
            PictureAction::Fetch(Box::new(info)),
            PictureContext { cancel, deadline },
            move |result| {
                let _ = tx.send(Completion::Picture {
                    operation: target,
                    result: result.map(Box::new),
                });
            },
        )?;
        let current = self.pictures.operations.get_mut(operation).unwrap();
        current.receipt = self.request_id.clone();
        current.receipt_pending = true;
        current.deadline = deadline;
        current.view.state = PictureState::Fetching;
        current.view.cached = false;
        Ok(())
    }
    fn picture_finished(&mut self, mut operation: Operation, result: Result<()>, terminal: bool) {
        let result = result.map_err(|error| match error.downcast_ref::<PictureError>() {
            Some(PictureError::Cancelled) => {
                BackendError::new(ErrorKind::Cancelled, "Picture operation cancelled").into()
            }
            Some(PictureError::DeadlineExceeded) => {
                BackendError::new(ErrorKind::Timeout, "Picture operation deadline exceeded").into()
            }
            _ => error,
        });
        let success = result.is_ok();
        if let Err(error) = &result {
            operation.view.state = if operation.cancel.load(Ordering::Acquire)
                || error.downcast_ref::<PictureError>() == Some(&PictureError::Cancelled)
                || error.is::<crate::source::SourceRequestCancelled>()
                || error
                    .downcast_ref::<BackendError>()
                    .is_some_and(|error| error.kind == ErrorKind::Cancelled)
            {
                PictureState::Cancelled
            } else {
                PictureState::Failed
            };
            operation.view.error = Some(error.to_string().chars().take(512).collect());
        }
        if operation.receipt_pending {
            self.publish_finished(operation.receipt.clone(), &result);
            operation.receipt_pending = false;
        }
        self.events.lock().unwrap().publish(
            self.revision,
            "pictures",
            Some(operation.view.operation_id.clone()),
            BackendEvent::PictureChanged {
                operation_id: operation.view.operation_id.clone(),
                state: operation.view.state,
                sha256: operation
                    .view
                    .info
                    .as_ref()
                    .and_then(|info| info.blob.as_ref())
                    .map(|blob| blob.sha256.clone()),
            },
        );
        if terminal || !success {
            self.pictures.bytes = self.pictures.bytes.saturating_sub(operation.weight);
            self.pictures.history.push_back(operation.view);
            while self.pictures.history.len() > 100 {
                self.pictures.history.pop_front();
            }
        } else {
            self.pictures
                .operations
                .insert(operation.view.operation_id.clone(), operation);
        }
    }
    pub(super) fn complete_picture(&mut self, id: String, result: Result<Box<PreparedPicture>>) {
        let Some(mut operation) = self.pictures.operations.remove(&id) else {
            return;
        };
        if !self.picture_current(&operation) {
            self.picture_finished(
                operation,
                Err(BackendError::new(ErrorKind::Cancelled, "Picture context changed").into()),
                true,
            );
            return;
        }
        match result {
            Ok(prepared) => match *prepared {
                PreparedPicture::NeedSource => {
                    let request = crate::source::SourceRequest {
                        source: "local".into(),
                        action: "pic".into(),
                        info: serde_json::json!({"type":null,"musicInfo":operation.source_info.take().unwrap_or_else(||self.source_music_info(&operation.spec.original))}),
                    };
                    let job = PictureSourceRequest {
                        operation: id.clone(),
                        request,
                        cancel: operation.cancel.clone(),
                        deadline: operation.deadline,
                    };
                    self.pictures.operations.insert(id.clone(), operation);
                    if let Err(error) = self.queue_picture_source(job)
                        && let Some(operation) = self.pictures.operations.remove(&id)
                    {
                        self.picture_finished(operation, Err(error), true);
                    }
                }
                PreparedPicture::Info { info, blob, cached } => {
                    if matches!(
                        operation.spec.key.authority,
                        ArtworkAuthority::Source { .. }
                    ) && operation.source_epoch.is_none()
                    {
                        operation.source_epoch = Some(self.source_epoch);
                    }
                    let outcome = (|| -> Result<()> {
                        ensure!(
                            info.key == operation.spec.key && info.expire_at_ms > now_ms(),
                            "Picture metadata changed or expired"
                        );
                        if !cached {
                            self.repository.put_artwork(&info, blob.as_ref())?;
                        }
                        if let Some(blob) = &blob {
                            self.pictures.service.remember(blob);
                        }
                        Ok(())
                    })();
                    if outcome.is_ok() {
                        self.pictures.publication_revision =
                            self.pictures.publication_revision.wrapping_add(1);
                        operation.view.state = if blob.is_some() {
                            PictureState::Ready
                        } else {
                            PictureState::AwaitingApproval
                        };
                        operation.view.info = Some(*info);
                        operation.view.cached = cached;
                        let _ = self.pictures.service.refresh_physical();
                    }
                    let terminal = blob.is_some();
                    self.picture_finished(operation, outcome, terminal);
                }
            },
            Err(error) => self.picture_finished(operation, Err(error), true),
        }
    }
    pub(super) fn complete_picture_source(
        &mut self,
        id: String,
        source_epoch: u64,
        result: Result<String>,
    ) {
        let Some(mut operation) = self.pictures.operations.remove(&id) else {
            return;
        };
        if operation.cancel.load(Ordering::Acquire) || operation.cache_epoch != self.pictures.epoch
        {
            self.picture_finished(
                operation,
                Err(BackendError::new(ErrorKind::Cancelled, "Picture operation cancelled").into()),
                true,
            );
            return;
        }
        operation.source_epoch = Some(source_epoch);
        if Instant::now() >= operation.deadline && !operation.cancel.load(Ordering::Acquire) {
            self.picture_finished(operation, Err(PictureError::DeadlineExceeded.into()), true);
            return;
        }
        if !self.picture_current(&operation) {
            self.picture_finished(
                operation,
                Err(
                    BackendError::new(ErrorKind::Cancelled, "Picture source permission changed")
                        .into(),
                ),
                true,
            );
            return;
        }
        let action = match result {
            Ok(url) => match ArtworkUrl::new(url) {
                Ok(url) => PictureAction::SourceUrl(url),
                Err(_) if operation.spec.local.is_some() => PictureAction::LocalFallback,
                Err(error) => {
                    self.picture_finished(operation, Err(error), true);
                    return;
                }
            },
            Err(error) if error.is::<crate::source::SourceRequestCancelled>() => {
                self.picture_finished(operation, Err(error), true);
                return;
            }
            Err(_) if operation.spec.local.is_some() => PictureAction::LocalFallback,
            Err(error) => {
                self.picture_finished(operation, Err(error), true);
                return;
            }
        };
        let tx = self.completed_tx.clone();
        let target = id.clone();
        let submitted = self.pictures.service.submit(
            id.clone(),
            operation.spec.clone(),
            action,
            PictureContext {
                cancel: operation.cancel.clone(),
                deadline: operation.deadline,
            },
            move |result| {
                let _ = tx.send(Completion::Picture {
                    operation: target,
                    result: result.map(Box::new),
                });
            },
        );
        if let Err(error) = submitted {
            self.picture_finished(operation, Err(error), true);
        } else {
            self.pictures.operations.insert(id, operation);
        }
    }
    pub(super) fn cancel_picture(&mut self, id: &str) -> Result<()> {
        let operation = self
            .pictures
            .operations
            .remove(id)
            .context("Picture operation is unavailable")?;
        operation.cancel.store(true, Ordering::Release);
        self.picture_finished(
            operation,
            Err(BackendError::new(ErrorKind::Cancelled, "Picture operation cancelled").into()),
            true,
        );
        Ok(())
    }
    pub(super) fn cancel_local_pictures(&mut self, tracks: &HashSet<String>) {
        let ids = self
            .pictures
            .operations
            .iter()
            .filter(|(_, operation)| tracks.contains(&operation.spec.original.id))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            let _ = self.cancel_picture(&id);
        }
    }
    pub(super) fn cancel_picture_source_requests(&mut self) {
        for job in std::mem::take(&mut self.pending_pictures) {
            job.cancel.store(true, Ordering::Release);
        }
        self.pending_picture_bytes = 0;
        let ids: Vec<_> = self
            .pictures
            .operations
            .iter()
            .filter(|(_, operation)| {
                matches!(
                    operation.spec.key.authority,
                    ArtworkAuthority::Source { .. }
                )
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(operation) = self.pictures.operations.remove(&id) {
                operation.cancel.store(true, Ordering::Release);
                if self
                    .source_inflight
                    .as_ref()
                    .is_some_and(|flight| Arc::ptr_eq(&flight.cancel, &operation.cancel))
                {
                    self.source_inflight
                        .as_ref()
                        .unwrap()
                        .cancel
                        .store(true, Ordering::Release);
                }
                self.picture_finished(
                    operation,
                    Err(
                        BackendError::new(ErrorKind::Cancelled, "Picture source session changed")
                            .into(),
                    ),
                    true,
                );
            }
        }
    }
    pub(super) fn clear_pictures(&mut self) -> Result<()> {
        self.pictures.epoch = self.pictures.epoch.wrapping_add(1);
        let ids: Vec<_> = self.pictures.operations.keys().cloned().collect();
        for id in ids {
            self.cancel_picture(&id)?;
        }
        for job in std::mem::take(&mut self.pending_pictures) {
            job.cancel.store(true, Ordering::Release);
        }
        self.pending_picture_bytes = 0;
        self.pictures.service.clear_verified();
        self.repository.clear_image_cache()?;
        let _ = self.pictures.service.refresh_physical();
        Ok(())
    }
    pub(super) fn cancel_all_picture_requests(&mut self) {
        let ids: Vec<_> = self.pictures.operations.keys().cloned().collect();
        for id in ids {
            let _ = self.cancel_picture(&id);
        }
        for job in std::mem::take(&mut self.pending_pictures) {
            job.cancel.store(true, Ordering::Release);
        }
        self.pending_picture_bytes = 0;
    }
    pub(super) fn picture_info(&self, list_id: &str, music_id: &str) -> Result<ArtworkLookup> {
        let spec = self.picture_spec(list_id, music_id)?;
        self.repository.artwork_info(&spec.key, now_ms())
    }
    /// This projection neither resolves metadata nor downloads or reads artwork.
    /// The current key includes the active source policy and local file grant.
    pub(super) fn open_api_picture_revision(&self) -> (u64, u64) {
        (self.pictures.epoch, self.pictures.publication_revision)
    }
    pub(super) fn current_open_api_artwork(&self) -> Result<Option<OpenApiArtwork>> {
        use sha2::{Digest, Sha256};
        let Some(record) = self.current_record.as_ref() else {
            return Ok(None);
        };
        let Some(entry) = self
            .queue
            .current
            .as_ref()
            .filter(|entry| entry.track_id == record.id)
        else {
            return Ok(None);
        };
        let ArtworkLookup::Hit(info) = self.picture_info(&entry.list_id, &record.id)? else {
            return Ok(None);
        };
        let Some(blob) = &info.blob else {
            return Ok(None);
        };
        blob.validate()?;
        let Ok(bytes) = self.pictures.service.verified_arc(&blob.sha256) else {
            return Ok(None);
        };
        ensure!(
            bytes.len() as u64 == blob.encoded_bytes,
            "Verified artwork metadata changed"
        );
        let mime = match blob.mime_type.as_str() {
            "image/jpeg" => "image/jpeg",
            "image/png" => "image/png",
            "image/gif" => "image/gif",
            "image/webp" => "image/webp",
            _ => return Ok(None),
        };
        let source_epoch = matches!(info.key.authority, ArtworkAuthority::Source { .. })
            .then_some(self.source_epoch);
        let scope = serde_json::to_vec(&(
            info.key.fingerprint()?,
            self.pictures.epoch,
            source_epoch,
            info.resolved_at_ms,
            info.expire_at_ms,
        ))?;
        Ok(Some(OpenApiArtwork {
            key_sha256: format!("{:x}", Sha256::digest(scope)),
            sha256: blob.sha256.clone(),
            mime_type: mime,
            bytes,
        }))
    }
    pub(super) fn picture_operations(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<PictureOperationView>> {
        self.pictures.page(offset, limit)
    }
    pub(super) fn picture_bytes(
        &self,
        sha: &str,
        offset: u64,
        limit: usize,
    ) -> Result<PictureBytePage> {
        self.pictures.service.byte_page(sha, offset, limit)
    }
    pub(super) fn picture_stats(&self) -> Result<ImageCacheStats> {
        let mut stats = self.repository.image_cache_stats()?;
        let physical = self.pictures.service.physical_snapshot();
        stats.database_bytes = physical.database_bytes;
        stats.wal_bytes = physical.wal_bytes;
        stats.shared_memory_bytes = physical.shared_memory_bytes;
        stats.physical_observed_at_ms = physical.physical_observed_at_ms;
        Ok(stats)
    }
}
