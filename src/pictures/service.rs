//! Four owned picture workers. Metadata resolution and image GET are separate jobs.
use crate::{
    audio::{
        http_client::LimitedHttpClient,
        worker_owner::{TrackedWorker, WorkerKind, WorkerPermit},
    },
    domain::{
        artwork::*,
        music::{LocalAsset, TrackRecord},
    },
    library::{
        artwork_validator::{VerifiedArtwork, validate_artwork},
        metadata::read_local_metadata_from_open_file,
    },
    platforms::{Platform, ProviderContext, ProviderRegistry, ProviderRequest, ProviderResponse},
    repository::Repository,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    collections::{HashMap, VecDeque},
    fmt,
    future::Future,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const PICTURE_WORKERS: usize = 4;
const QUEUE_CAPACITY: usize = 16;
const QUEUE_BYTES: usize = 4 * 1024 * 1024;
const VERIFIED_RAM_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PictureError {
    Cancelled,
    DeadlineExceeded,
    Busy,
    NoPicture,
    WorkerFailed,
    NotVerified,
}
impl fmt::Display for PictureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "Picture operation cancelled",
            Self::DeadlineExceeded => "Picture operation deadline exceeded",
            Self::Busy => "Picture worker queue is full",
            Self::NoPicture => "No permitted artwork is available",
            Self::WorkerFailed => "Picture worker failed",
            Self::NotVerified => {
                "Resolve the picture to verify its cached bytes before reading them"
            }
        })
    }
}
impl std::error::Error for PictureError {}

#[derive(Clone)]
pub struct PictureContext {
    pub cancel: Arc<AtomicBool>,
    pub deadline: Instant,
}
impl PictureContext {
    pub fn check(&self) -> Result<()> {
        if self.cancel.load(Ordering::Acquire) {
            return Err(PictureError::Cancelled.into());
        }
        if Instant::now() >= self.deadline {
            return Err(PictureError::DeadlineExceeded.into());
        }
        Ok(())
    }
}
#[derive(Clone, Serialize)]
pub struct PictureSpec {
    pub key: ArtworkKey,
    pub original: TrackRecord,
    pub matched: Option<TrackRecord>,
    pub local: Option<LocalAsset>,
}
pub enum PictureAction {
    Resolve,
    SourceUrl(ArtworkUrl),
    LocalFallback,
    Fetch(Box<ArtworkInfo>),
}
pub enum PreparedPicture {
    NeedSource,
    Info {
        info: Box<ArtworkInfo>,
        blob: Option<VerifiedArtwork>,
        cached: bool,
    },
}
struct Task {
    operation: String,
    weight: usize,
    spec: PictureSpec,
    action: PictureAction,
    context: PictureContext,
    done: Box<dyn FnOnce(Result<PreparedPicture>) + Send>,
}
#[derive(Default)]
struct Queue {
    tasks: VecDeque<Task>,
    bytes: usize,
    active: HashMap<String, Arc<AtomicBool>>,
}
#[derive(Default)]
struct VerifiedCache {
    entries: VecDeque<(String, Arc<[u8]>)>,
    bytes: usize,
}
struct Core {
    local_authority: crate::library::authority::AuthorityRegistry,
    repository: Arc<Repository>,
    registry: Arc<ProviderRegistry>,
    client: LimitedHttpClient,
    queue: Mutex<Queue>,
    changed: Condvar,
    closing: AtomicBool,
    measure: AtomicBool,
    active: AtomicUsize,
    verified: Mutex<VerifiedCache>,
    physical: Mutex<ImageCacheStats>,
}
pub struct PictureService {
    core: Arc<Core>,
    workers: Mutex<Vec<TrackedWorker>>,
}
impl PictureService {
    pub fn new(repository: Arc<Repository>, registry: Arc<ProviderRegistry>) -> Result<Self> {
        Self::with_authority(
            repository,
            registry,
            crate::library::authority::AuthorityRegistry::default(),
        )
    }
    pub fn with_authority(
        repository: Arc<Repository>,
        registry: Arc<ProviderRegistry>,
        local_authority: crate::library::authority::AuthorityRegistry,
    ) -> Result<Self> {
        let core = Arc::new(Core {
            local_authority,
            repository,
            registry,
            client: LimitedHttpClient::new(Duration::from_secs(8))?,
            queue: Mutex::new(Queue::default()),
            changed: Condvar::new(),
            closing: AtomicBool::new(false),
            measure: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            verified: Mutex::new(VerifiedCache::default()),
            physical: Mutex::new(ImageCacheStats::default()),
        });
        Ok(Self {
            core,
            workers: Mutex::new(Vec::new()),
        })
    }
    fn ensure_workers(&self) -> Result<()> {
        let core = &self.core;
        let mut workers = self
            .workers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ensure!(
            !core.closing.load(Ordering::Acquire),
            PictureError::Cancelled
        );
        if !workers.is_empty() {
            return Ok(());
        }
        for index in 0..PICTURE_WORKERS {
            let permit = match WorkerPermit::reserve(WorkerKind::Artwork) {
                Ok(permit) => permit,
                Err(error) => {
                    core.closing.store(true, Ordering::Release);
                    core.changed.notify_all();
                    return Err(error);
                }
            };
            let owned = core.clone();
            match std::thread::Builder::new()
                .name(format!("lx-artwork-{index}"))
                .spawn(move || run(owned))
            {
                Ok(handle) => workers.push(permit.attach(handle)),
                Err(error) => {
                    core.closing.store(true, Ordering::Release);
                    core.changed.notify_all();
                    return Err(error.into());
                }
            }
        }
        Ok(())
    }
    pub fn submit(
        &self,
        operation: String,
        spec: PictureSpec,
        action: PictureAction,
        context: PictureContext,
        done: impl FnOnce(Result<PreparedPicture>) + Send + 'static,
    ) -> Result<()> {
        context.check()?;
        spec.key.validate()?;
        let weight = serde_json::to_vec(&spec)?.len();
        ensure!(
            weight <= 2 * 1024 * 1024,
            "Picture request metadata exceeds 2 MiB"
        );
        self.ensure_workers()?;
        let mut queue = self
            .core
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ensure!(
            !self.core.closing.load(Ordering::Acquire),
            PictureError::Cancelled
        );
        ensure!(
            queue.tasks.len() < QUEUE_CAPACITY && weight <= QUEUE_BYTES.saturating_sub(queue.bytes),
            PictureError::Busy
        );
        queue.bytes += weight;
        queue.tasks.push_back(Task {
            operation,
            weight,
            spec,
            action,
            context,
            done: Box::new(done),
        });
        drop(queue);
        self.core.changed.notify_one();
        Ok(())
    }
    pub fn remember(&self, blob: &VerifiedArtwork) {
        let mut cache = self
            .core
            .verified
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(index) = cache
            .entries
            .iter()
            .position(|(sha, _)| sha == &blob.metadata().sha256)
        {
            let (_, bytes) = cache.entries.remove(index).unwrap();
            cache.bytes -= bytes.len();
        }
        while cache.bytes + blob.encoded().len() > VERIFIED_RAM_BYTES {
            let Some((_, bytes)) = cache.entries.pop_front() else {
                break;
            };
            cache.bytes -= bytes.len();
        }
        cache.bytes += blob.encoded().len();
        cache
            .entries
            .push_back((blob.metadata().sha256.clone(), blob.encoded_arc()));
    }
    /// Shares only a decoded-and-validated cache entry, never a stored blob or URL.
    pub(crate) fn verified_arc(&self, sha256: &str) -> Result<Arc<[u8]>> {
        crate::domain::artwork::digest(sha256)?;
        let mut cache = self
            .core
            .verified
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let index = cache
            .entries
            .iter()
            .position(|(sha, _)| sha == sha256)
            .ok_or(PictureError::NotVerified)?;
        let entry = cache.entries.remove(index).unwrap();
        let bytes = entry.1.clone();
        cache.entries.push_back(entry);
        Ok(bytes)
    }
    pub fn byte_page(&self, sha256: &str, offset: u64, limit: usize) -> Result<PictureBytePage> {
        crate::domain::artwork::digest(sha256)?;
        ensure!(
            (1..=MAX_ARTWORK_PAGE_BYTES).contains(&limit),
            "Picture byte page must contain 1..=65536 bytes"
        );
        let mut cache = self
            .core
            .verified
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let index = cache
            .entries
            .iter()
            .position(|(sha, _)| sha == sha256)
            .ok_or(PictureError::NotVerified)?;
        ensure!(
            offset <= cache.entries[index].1.len() as u64,
            "Picture byte offset exceeds its length"
        );
        let entry = cache.entries.remove(index).unwrap();
        let start = usize::try_from(offset)?;
        let end = start.saturating_add(limit).min(entry.1.len());
        let page = PictureBytePage {
            sha256: sha256.into(),
            offset,
            total: entry.1.len() as u64,
            bytes: entry.1[start..end].to_vec(),
        };
        cache.entries.push_back(entry);
        Ok(page)
    }
    pub fn clear_verified(&self) {
        let mut cache = self
            .core
            .verified
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        cache.entries.clear();
        cache.bytes = 0;
    }
    pub fn physical_snapshot(&self) -> ImageCacheStats {
        self.core
            .physical
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    pub fn refresh_physical(&self) -> Result<()> {
        self.ensure_workers()?;
        self.core.measure.store(true, Ordering::Release);
        self.core.changed.notify_one();
        Ok(())
    }
    pub fn active_jobs(&self) -> usize {
        self.core.active.load(Ordering::Acquire)
    }
    pub fn begin_shutdown(&self) {
        self.core.closing.store(true, Ordering::Release);
        let queue = self
            .core
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for task in &queue.tasks {
            task.context.cancel.store(true, Ordering::Release);
        }
        for cancel in queue.active.values() {
            cancel.store(true, Ordering::Release);
        }
        drop(queue);
        self.core.changed.notify_all();
    }
    pub fn shutdown(&self, timeout: Duration) -> bool {
        self.begin_shutdown();
        let started = Instant::now();
        self.workers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .all(|worker| worker.wait(timeout.saturating_sub(started.elapsed())))
    }
    pub fn outstanding_workers() -> usize {
        crate::audio::worker_owner::outstanding(WorkerKind::Artwork)
    }
}
impl Drop for PictureService {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}
fn run(core: Arc<Core>) {
    loop {
        let task = {
            let mut queue = core.queue.lock().unwrap_or_else(|error| error.into_inner());
            loop {
                if let Some(task) = queue.tasks.pop_front() {
                    queue.bytes -= task.weight;
                    queue
                        .active
                        .insert(task.operation.clone(), task.context.cancel.clone());
                    break Some(task);
                }
                if core.closing.load(Ordering::Acquire) {
                    return;
                }
                if core.measure.swap(false, Ordering::AcqRel) {
                    break None;
                }
                queue = core
                    .changed
                    .wait(queue)
                    .unwrap_or_else(|error| error.into_inner());
            }
        };
        let Some(task) = task else {
            measure_physical(&core);
            continue;
        };
        core.active.fetch_add(1, Ordering::AcqRel);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            prepare(&core, &task.spec, &task.action, &task.context)
        }))
        .unwrap_or_else(|_| Err(PictureError::WorkerFailed.into()));
        core.queue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .active
            .remove(&task.operation);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (task.done)(result)));
        core.active.fetch_sub(1, Ordering::AcqRel);
        measure_physical(&core);
    }
}
fn prepare(
    core: &Core,
    spec: &PictureSpec,
    action: &PictureAction,
    context: &PictureContext,
) -> Result<PreparedPicture> {
    context.check()?;
    if let Some(asset) = &spec.local {
        let file = open_local(core, asset, context)?;
        verify_local(asset, &file)?;
    }
    match action {
        PictureAction::Resolve => {
            if let ArtworkLookup::Hit(mut info) =
                core.repository.artwork_info(&spec.key, now_ms())?
            {
                if let Some(metadata) = &info.blob {
                    if let Some((stored, bytes)) = core
                        .repository
                        .stored_artwork_blob(&metadata.sha256, now_ms())?
                    {
                        let blob =
                            validate_artwork(bytes, Some(&stored.mime_type), || context.check())?;
                        ensure!(
                            blob.metadata() == &stored && metadata == &stored,
                            "Cached picture checksum or decoded information changed"
                        );
                        return Ok(PreparedPicture::Info {
                            info,
                            blob: Some(blob),
                            cached: true,
                        });
                    }
                    info.blob = None;
                }
                if info.url.is_some() {
                    info.validate()?;
                    return Ok(PreparedPicture::Info {
                        info,
                        blob: None,
                        cached: true,
                    });
                }
            }
            match &spec.key.authority {
                ArtworkAuthority::Source { .. } => Ok(PreparedPicture::NeedSource),
                ArtworkAuthority::LocalAsset { .. } => embedded(core, spec, context),
                ArtworkAuthority::Native => native(core, spec, context),
            }
        }
        PictureAction::SourceUrl(url) => {
            let ArtworkAuthority::Source {
                source_sha256,
                policy_version,
                ..
            } = &spec.key.authority
            else {
                anyhow::bail!("Picture source authority is missing")
            };
            let info = ArtworkInfo::new(
                spec.key.clone(),
                ArtworkProvenance::Source {
                    source_sha256: source_sha256.clone(),
                    policy_version: *policy_version,
                },
                Some(url.clone()),
                None,
                now_ms(),
            )?;
            Ok(PreparedPicture::Info {
                info: Box::new(info),
                blob: None,
                cached: false,
            })
        }
        PictureAction::LocalFallback => embedded(core, spec, context),
        PictureAction::Fetch(info) => {
            info.validate()?;
            ensure!(info.key == spec.key, "Picture approval metadata changed");
            let url = info
                .url
                .as_ref()
                .context("Picture approval URL is missing")?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let fetched = runtime.block_on(fetch(core, url, context));
            drop(runtime);
            let (bytes, mime) = fetched?;
            let blob = validate_artwork(bytes, mime.as_deref(), || context.check())?;
            let mut info = info.clone();
            info.blob = Some(blob.metadata().clone());
            info.validate()?;
            context.check()?;
            Ok(PreparedPicture::Info {
                info,
                blob: Some(blob),
                cached: false,
            })
        }
    }
}
fn open_local(
    core: &Core,
    asset: &LocalAsset,
    context: &PictureContext,
) -> Result<crate::library::authority::AuthorizedFile> {
    let intent = core
        .repository
        .local_asset_authority(&asset.id)?
        .context("Local artwork has no current file grant")?;
    ensure!(
        intent.explicit_file_epoch.is_some() || !intent.roots.is_empty(),
        "Local artwork has no current file grant"
    );
    core.local_authority
        .install(
            &intent,
            core.repository.local_asset_file_identity(&asset.id)?,
        )?
        .with_cancel(context.cancel.clone())?
        .open()
}
fn verify_local(
    asset: &LocalAsset,
    file: &crate::library::authority::AuthorizedFile,
) -> Result<()> {
    ensure!(
        asset.available && asset.path.is_absolute(),
        "Local artwork permission is unavailable"
    );
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() == asset.size_bytes,
        "Local artwork asset content changed"
    );
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|time| u64::try_from(time.as_millis()).ok());
    ensure!(
        modified == asset.modified_ms,
        "Local artwork asset modification stamp changed"
    );
    Ok(())
}
fn embedded(core: &Core, spec: &PictureSpec, context: &PictureContext) -> Result<PreparedPicture> {
    let asset = spec.local.as_ref().ok_or(PictureError::NoPicture)?;
    let file = open_local(core, asset, context)?;
    verify_local(asset, &file)?;
    context.check()?;
    let metadata = read_local_metadata_from_open_file(
        file.try_clone()?,
        file.path().to_path_buf(),
        file.metadata()?.len(),
    )?;
    context.check()?;
    verify_local(asset, &file)?;
    let mut pictures = metadata.pictures;
    pictures.sort_by_key(|picture| usize::from(!picture.picture_type.contains("CoverFront")));
    let picture = pictures.into_iter().next().ok_or(PictureError::NoPicture)?;
    let blob = validate_artwork(picture.data, Some(&picture.mime_type), || context.check())?;
    let info = ArtworkInfo::new(
        spec.key.clone(),
        ArtworkProvenance::Embedded {
            asset_id: asset.id.clone(),
            permission_sha256: local_picture_permission(&core.repository, asset)?,
        },
        None,
        Some(blob.metadata().clone()),
        now_ms(),
    )?;
    Ok(PreparedPicture::Info {
        info: Box::new(info),
        blob: Some(blob),
        cached: false,
    })
}
pub fn local_picture_permission(repository: &Repository, asset: &LocalAsset) -> Result<String> {
    use sha2::{Digest, Sha256};
    let intent = repository
        .local_asset_authority(&asset.id)?
        .context("Local artwork has no current file grant")?;
    ensure!(
        intent.track_id == asset.track_id
            && intent.path == asset.path
            && (intent.explicit_file_epoch.is_some() || !intent.roots.is_empty()),
        "Local artwork file grant differs from its asset"
    );
    let document = serde_json::to_vec(&(
        local_artwork_permission(asset)?,
        intent.permission_sha256()?,
    ))?;
    Ok(format!("{:x}", Sha256::digest(document)))
}

fn metadata_url(track: &TrackRecord) -> Option<ArtworkUrl> {
    track
        .metadata
        .get("picUrl")
        .and_then(serde_json::Value::as_str)
        .filter(|url| !url.is_empty())
        .and_then(|url| ArtworkUrl::new(url).ok())
}
fn native(core: &Core, spec: &PictureSpec, context: &PictureContext) -> Result<PreparedPicture> {
    for track in std::iter::once(&spec.original).chain(spec.matched.iter()) {
        context.check()?;
        let result = if let Some(url) = metadata_url(track) {
            Ok((
                url,
                ArtworkProvenance::Metadata {
                    platform: track.platform.source.clone(),
                },
            ))
        } else {
            let platform: Platform =
                serde_json::from_value(serde_json::json!(track.platform.source))?;
            let request = ProviderRequest::Picture {
                track: track.clone(),
            };
            let provider = ProviderContext {
                cancel: context.cancel.clone(),
                deadline: context.deadline,
            };
            core.registry
                .execute(platform, request, &provider)
                .map_err(anyhow::Error::from)
                .and_then(|result| {
                    let ProviderResponse::Picture(url) = result else {
                        anyhow::bail!("Picture provider returned incompatible metadata")
                    };
                    Ok((
                        ArtworkUrl::new(url)?,
                        ArtworkProvenance::Provider {
                            platform: track.platform.source.clone(),
                        },
                    ))
                })
        };
        if let Ok((url, provenance)) = result {
            let info = ArtworkInfo::new(spec.key.clone(), provenance, Some(url), None, now_ms())?;
            return Ok(PreparedPicture::Info {
                info: Box::new(info),
                blob: None,
                cached: false,
            });
        }
        context.check()?;
    }
    Err(PictureError::NoPicture.into())
}
async fn checked<T>(
    future: impl Future<Output = Result<T>>,
    context: &PictureContext,
) -> Result<T> {
    let mut future = Box::pin(future);
    loop {
        context.check()?;
        let tick = context
            .deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(10));
        match tokio::time::timeout(tick, future.as_mut()).await {
            Ok(result) => {
                context.check()?;
                return result;
            }
            Err(_) => continue,
        }
    }
}
async fn fetch(
    core: &Core,
    url: &ArtworkUrl,
    context: &PictureContext,
) -> Result<(Vec<u8>, Option<String>)> {
    let parsed = reqwest::Url::parse(url.as_str())?;
    let mut response = checked(core.client.get(&parsed, None, None), context).await?;
    ensure!(
        response.status() == reqwest::StatusCode::OK,
        "Picture URL did not return HTTP 200"
    );
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= MAX_ARTWORK_BYTES as u64),
        "Picture HTTP body exceeds 8 MiB"
    );
    ensure!(
        response
            .headers()
            .get("content-encoding")
            .is_none_or(|value| value.as_bytes() == b"identity"),
        "Picture HTTP content encoding is unsupported"
    );
    let mime = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut bytes = Vec::new();
    while let Some(chunk) = checked(response.chunk(), context).await? {
        ensure!(
            chunk.len() <= MAX_ARTWORK_BYTES.saturating_sub(bytes.len()),
            "Picture HTTP body exceeds 8 MiB"
        );
        bytes.extend_from_slice(&chunk);
    }
    context.check()?;
    Ok((bytes, mime))
}
fn measure_physical(core: &Core) {
    let path = core.repository.path();
    let size = |path: &std::path::Path| match std::fs::metadata(path) {
        Ok(metadata) => Some(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(0),
        Err(_) => None,
    };
    let mut snapshot = core
        .physical
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    snapshot.database_bytes = size(path);
    snapshot.wal_bytes = size(&std::path::PathBuf::from(format!("{}-wal", path.display())));
    snapshot.shared_memory_bytes =
        size(&std::path::PathBuf::from(format!("{}-shm", path.display())));
    snapshot.physical_observed_at_ms = Some(now_ms());
}
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as u64
}
