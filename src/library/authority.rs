//! File access through verified handles and revocable grants.
use super::scan::ScanContext;
use crate::audio::worker_owner::{TrackedWorker, WorkerKind, WorkerPermit};
use crate::domain::library::{
    FileIdentity, LibraryPathPolicy, LibraryRoot, LibraryRootDraft, LocalAssetAuthority,
};
use crate::domain::library_watch::{LibraryAssetBinding, RootMovePolicy};
use crate::domain::music::normalize_local_path;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom};
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Condvar, Mutex, OnceLock, RwLock, Weak,
    atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

const MAX_PATH_BYTES: usize = 16 * 1024;
const MAX_COMPONENTS: usize = 256;
const MAX_READ_BYTES: usize = 64 * 1024;
const MAX_AUTHORITY_BYTES: usize = 32 * 1024 * 1024;
const RETIREMENT_CAPACITY: usize = 2048;
const MAX_LIVE_OPENS_PER_ASSET: usize = 8;
const MAX_CONTENT_HASH_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_RELOCATION_HINTS: usize = 200;
const MAX_RELOCATION_HINT_BYTES: usize = 1024 * 1024;
mod download_output;
use download_output::{DownloadOutputAuthority, LibraryOpenFile, close_actual_library_file};
pub(crate) use download_output::{DownloadedPinReceipt, DownloadedReadReceipt};
#[cfg(all(test, windows))]
static RELATIVE_OPEN_COUNT: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum RootHealth {
    Unknown,
    Verified,
    Unavailable,
    NeedsReapproval,
}
impl RootHealth {
    fn load(value: &AtomicU8) -> Self {
        match value.load(Ordering::Acquire) {
            1 => Self::Verified,
            2 => Self::Unavailable,
            3 => Self::NeedsReapproval,
            _ => Self::Unknown,
        }
    }
}
#[derive(Default)]
struct RetirementScope {
    state: AtomicUsize,
}
const SCOPE_CLOSING: usize = 1usize << (usize::BITS - 1);
impl RetirementScope {
    fn pending(&self) -> usize {
        self.state.load(Ordering::Acquire) & !SCOPE_CLOSING
    }
    fn is_closing(&self) -> bool {
        self.state.load(Ordering::Acquire) & SCOPE_CLOSING != 0
    }
    fn begin_shutdown(&self) {
        self.state.fetch_or(SCOPE_CLOSING, Ordering::AcqRel);
    }
}
#[derive(Clone)]
pub struct RetirementBarrier {
    scope: Arc<RetirementScope>,
}
impl RetirementBarrier {
    pub fn wait(&self, timeout: Duration) -> bool {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        loop {
            if self.scope.pending() == 0 {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
struct ScopeReservation(Arc<RetirementScope>);
impl ScopeReservation {
    fn new(scope: Arc<RetirementScope>) -> Result<Self> {
        scope
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state & SCOPE_CLOSING == 0)
                    .then(|| state.checked_add(1))
                    .flatten()
                    .filter(|next| next & SCOPE_CLOSING == 0)
            })
            .map_err(|_| anyhow::anyhow!("Library authority is closing"))?;
        Ok(Self(scope))
    }
    fn for_retirement(scope: Arc<RetirementScope>) -> Result<Self> {
        scope
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                ((state & !SCOPE_CLOSING) < RETIREMENT_CAPACITY)
                    .then(|| state.checked_add(1))
                    .flatten()
            })
            .map_err(|_| anyhow::anyhow!("Library retirement scope capacity reached"))?;
        Ok(Self(scope))
    }
}
impl Drop for ScopeReservation {
    fn drop(&mut self) {
        self.0.state.fetch_sub(1, Ordering::AcqRel);
    }
}
struct DirectoryFiles {
    files: Vec<File>,
    _reservation: Reservation,
}
struct RetiredDirectories {
    _files: DirectoryFiles,
    _scopes: Vec<ScopeReservation>,
}
struct RetirementSlot {
    reserved: bool,
    payload: Option<RetirementPayload>,
}
enum RetirementPayload {
    Directories(RetiredDirectories),
    Download(download_output::DownloadRetirementPayload),
    DownloadGrant(download_output::DownloadGrantRetirement),
}
impl RetirementPayload {
    fn try_close(&mut self) -> Result<()> {
        match self {
            Self::Directories(payload) => {
                while let Some(file) = payload._files.files.pop() {
                    if let Err((retained, error)) = close_actual_library_file(file) {
                        payload._files.files.push(retained);
                        return Err(error.into());
                    }
                }
                Ok(())
            }
            Self::Download(payload) => payload.try_close(),
            Self::DownloadGrant(payload) => payload.try_close(),
        }
    }
}
struct RetirementState {
    slots: Vec<RetirementSlot>,
    ready: VecDeque<usize>,
}
struct RetirementQueue {
    state: Mutex<RetirementState>,
    wake: Condvar,
    _reservation: Reservation,
}
struct RetirementService {
    queue: Arc<RetirementQueue>,
    _worker: TrackedWorker,
}
static RETIREMENT: OnceLock<std::result::Result<RetirementService, String>> = OnceLock::new();
fn retirement_service() -> Result<&'static RetirementService> {
    let service = RETIREMENT.get_or_init(|| {
        (|| -> Result<RetirementService> {
            let reservation = Reservation::new(256 * 1024)?;
            let queue = Arc::new(RetirementQueue {
                state: Mutex::new(RetirementState {
                    slots: (0..RETIREMENT_CAPACITY)
                        .map(|_| RetirementSlot {
                            reserved: false,
                            payload: None,
                        })
                        .collect(),
                    ready: VecDeque::with_capacity(RETIREMENT_CAPACITY),
                }),
                wake: Condvar::new(),
                _reservation: reservation,
            });
            let permit = WorkerPermit::reserve(WorkerKind::LibraryRetirement)?;
            let owned = queue.clone();
            let handle = std::thread::Builder::new()
                .name("lx-library-fd-owner".into())
                .spawn(move || {
                    loop {
                        let (index, mut payload) = {
                            let mut state = owned
                                .state
                                .lock()
                                .unwrap_or_else(|error| error.into_inner());
                            while state.ready.is_empty() {
                                state = owned
                                    .wake
                                    .wait(state)
                                    .unwrap_or_else(|error| error.into_inner());
                            }
                            let index = state.ready.pop_front().unwrap();
                            (index, state.slots[index].payload.take().unwrap())
                        };
                        // Failed native close keeps the same descriptor, pins
                        // and scope charge in its already-reserved slot.
                        if payload.try_close().is_err() {
                            let mut state = owned
                                .state
                                .lock()
                                .unwrap_or_else(|error| error.into_inner());
                            state.slots[index].payload = Some(payload);
                            state.ready.push_back(index);
                            drop(state);
                            std::thread::sleep(Duration::from_millis(10));
                        } else {
                            drop(payload);
                            owned
                                .state
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .slots[index]
                                .reserved = false;
                        }
                        owned.wake.notify_all();
                    }
                })?;
            Ok(RetirementService {
                queue,
                _worker: permit.attach(handle),
            })
        })()
        .map_err(|error| error.to_string())
    });
    service
        .as_ref()
        .map_err(|message| anyhow::anyhow!(message.clone()))
}
struct RetirementTicket {
    queue: Arc<RetirementQueue>,
    index: usize,
    armed: bool,
}
impl RetirementTicket {
    fn new() -> Result<Self> {
        let queue = retirement_service()?.queue.clone();
        let index = {
            let mut state = queue
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let index = state
                .slots
                .iter()
                .position(|slot| !slot.reserved)
                .context("Library directory proof capacity reached")?;
            state.slots[index].reserved = true;
            index
        };
        Ok(Self {
            queue,
            index,
            armed: true,
        })
    }
    fn retire(self, payload: RetiredDirectories) {
        self.retire_payload(RetirementPayload::Directories(payload));
    }
    fn retire_payload(mut self, payload: RetirementPayload) {
        {
            let mut state = self
                .queue
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.slots[self.index].payload = Some(payload);
            state.ready.push_back(self.index);
        }
        self.armed = false;
        self.queue.wake.notify_one();
    }
}
impl Drop for RetirementTicket {
    fn drop(&mut self) {
        if self.armed {
            self.queue
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .slots[self.index]
                .reserved = false;
        }
    }
}
struct DirectoryTicket {
    ticket: RetirementTicket,
    scope: ScopeReservation,
    reservation: Reservation,
}
impl DirectoryTicket {
    fn new(scope: Arc<RetirementScope>) -> Result<Self> {
        Ok(Self {
            scope: ScopeReservation::new(scope)?,
            ticket: RetirementTicket::new()?,
            reservation: Reservation::new(MAX_COMPONENTS * std::mem::size_of::<File>() * 2 + 4096)?,
        })
    }
    fn capture(self, files: Vec<File>) -> DirectoryLease {
        DirectoryLease(Arc::new(DirectoryLeaseInner {
            files: ManuallyDrop::new(DirectoryFiles {
                files,
                _reservation: self.reservation,
            }),
            scopes: Mutex::new(vec![self.scope]),
            ticket: Some(self.ticket),
        }))
    }
}
struct DirectoryLeaseInner {
    files: ManuallyDrop<DirectoryFiles>,
    scopes: Mutex<Vec<ScopeReservation>>,
    ticket: Option<RetirementTicket>,
}
impl Drop for DirectoryLeaseInner {
    fn drop(&mut self) {
        let files = unsafe { ManuallyDrop::take(&mut self.files) };
        let scopes = std::mem::take(
            self.scopes
                .get_mut()
                .unwrap_or_else(|error| error.into_inner()),
        );
        self.ticket
            .take()
            .expect("directory proof has its reserved retirement slot")
            .retire(RetiredDirectories {
                _files: files,
                _scopes: scopes,
            });
    }
}
#[derive(Clone)]
struct DirectoryLease(Arc<DirectoryLeaseInner>);
impl Deref for DirectoryLease {
    type Target = [File];
    fn deref(&self) -> &Self::Target {
        &self.0.files.files
    }
}
impl DirectoryLease {
    fn adopt(&self, scope: Arc<RetirementScope>) -> Result<()> {
        let mut scopes = self
            .0
            .scopes
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !scopes
            .iter()
            .any(|existing| Arc::ptr_eq(&existing.0, &scope))
        {
            ensure!(scopes.len() < 128, "Directory proof owner budget exceeded");
            scopes.push(ScopeReservation::new(scope)?);
        }
        Ok(())
    }
}
fn path_policy() -> LibraryPathPolicy {
    #[cfg(windows)]
    {
        LibraryPathPolicy::WindowsInsensitive
    }
    #[cfg(not(windows))]
    {
        LibraryPathPolicy::Sensitive
    }
}
static AUTHORITY_BYTES: AtomicUsize = AtomicUsize::new(0);
struct Reservation(usize);
impl Reservation {
    fn new(bytes: usize) -> Result<Self> {
        AUTHORITY_BYTES
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current
                    .checked_add(bytes)
                    .filter(|total| *total <= MAX_AUTHORITY_BYTES)
            })
            .map_err(|_| anyhow::anyhow!("Library authority storage budget exceeded"))?;
        Ok(Self(bytes))
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        AUTHORITY_BYTES.fetch_sub(self.0, Ordering::AcqRel);
    }
}

fn validate_path(path: &Path) -> Result<()> {
    let text = path
        .to_str()
        .context("Library path must be lossless UTF-8")?;
    ensure!(
        path.is_absolute() && text.len() <= MAX_PATH_BYTES && !text.contains('\0'),
        "Invalid library path"
    );
    ensure!(
        path.components().count() <= MAX_COMPONENTS
            && !path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir)),
        "Unsafe library path component"
    );
    #[cfg(windows)]
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => ensure!(
                matches!(
                    prefix.kind(),
                    std::path::Prefix::Disk(_)
                        | std::path::Prefix::UNC(_, _)
                        | std::path::Prefix::VerbatimDisk(_)
                        | std::path::Prefix::VerbatimUNC(_, _)
                ),
                "Device paths are not authorized"
            ),
            std::path::Component::Normal(name) => {
                let name = name
                    .to_str()
                    .context("Windows library component must be UTF-8")?;
                let stem = name
                    .split('.')
                    .next()
                    .unwrap_or_default()
                    .to_ascii_uppercase();
                ensure!(
                    !name.contains(':')
                        && !name.ends_with('.')
                        && !name.ends_with(' ')
                        && !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$")
                        && !(stem.len() == 4
                            && (stem.starts_with("COM") || stem.starts_with("LPT"))
                            && matches!(stem.as_bytes()[3], b'1'..=b'9')),
                    "Unsafe Windows library component"
                );
            }
            _ => {}
        }
    }
    Ok(())
}

#[derive(Clone)]
struct Access {
    alive: Arc<AtomicBool>,
    scope_alive: Option<Arc<AtomicBool>>,
    cancel: Option<Arc<AtomicBool>>,
    deadline: Option<Instant>,
}
impl Access {
    fn check(&self) -> Result<()> {
        ensure!(self.alive.load(Ordering::Acquire), "Library grant revoked");
        ensure!(
            !self
                .scope_alive
                .as_ref()
                .is_some_and(|alive| !alive.load(Ordering::Acquire)),
            "Library preparation scope ended"
        );
        ensure!(
            !self
                .cancel
                .as_ref()
                .is_some_and(|cancel| cancel.load(Ordering::Acquire)),
            "Library scan cancelled"
        );
        ensure!(
            !self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline),
            "Library scan deadline exceeded"
        );
        Ok(())
    }
    fn durable() -> Self {
        Self {
            alive: Arc::new(AtomicBool::new(true)),
            scope_alive: None,
            cancel: None,
            deadline: None,
        }
    }
    fn scanning(context: &ScanContext) -> Self {
        Self {
            alive: Arc::new(AtomicBool::new(true)),
            scope_alive: None,
            cancel: Some(context.cancel.clone()),
            deadline: Some(context.deadline),
        }
    }
}

struct RootInner {
    draft: LibraryRootDraft,
    locator: Arc<RwLock<PathBuf>>,
    move_policy: Arc<RwLock<RootMovePolicy>>,
    projection_storage: Arc<Mutex<Option<Reservation>>>,
    access: Access,
    handles: Mutex<Option<DirectoryLease>>,
    health: Arc<AtomicU8>,
    owner: Arc<RetirementScope>,
    _reservation: Reservation,
}

#[derive(Clone)]
pub struct RootAuthority {
    inner: Arc<RootInner>,
}
impl RootAuthority {
    pub fn open(path: impl AsRef<Path>, recursive: bool, context: &ScanContext) -> Result<Self> {
        Self::open_scoped(
            path.as_ref(),
            recursive,
            context,
            Arc::new(RetirementScope::default()),
        )
    }
    fn open_scoped(
        path: &Path,
        recursive: bool,
        context: &ScanContext,
        owner: Arc<RetirementScope>,
    ) -> Result<Self> {
        context.check()?;
        validate_path(path)?;
        let reservation = Reservation::new(1024 + path.as_os_str().len() * 4)?;
        let ticket = DirectoryTicket::new(owner.clone())?;
        let (path, anchor, handles) = platform::open_root(path)?;
        context.check()?;
        let handles = ticket.capture(handles);
        Ok(Self {
            inner: Arc::new(RootInner {
                locator: Arc::new(RwLock::new(path.clone())),
                move_policy: Arc::new(RwLock::new(RootMovePolicy::Pause)),
                projection_storage: Arc::new(Mutex::new(None)),
                draft: LibraryRootDraft {
                    path,
                    recursive,
                    anchor,
                    path_policy: path_policy(),
                },
                access: Access::scanning(context),
                handles: Mutex::new(Some(handles)),
                health: Arc::new(AtomicU8::new(RootHealth::Verified as u8)),
                owner,
                _reservation: reservation,
            }),
        })
    }
    pub fn restore(root: &LibraryRoot, context: &ScanContext) -> Result<Self> {
        let authority = Self::from_record(
            root,
            Access::scanning(context),
            Arc::new(RetirementScope::default()),
        )?;
        authority.verify()?;
        Ok(authority)
    }
    fn from_record(
        root: &LibraryRoot,
        access: Access,
        owner: Arc<RetirementScope>,
    ) -> Result<Self> {
        validate_path(&root.path)?;
        root.validate()?;
        ensure!(
            root.path_policy == path_policy(),
            "UnsupportedCapability: library path policy"
        );
        Ok(Self {
            inner: Arc::new(RootInner {
                locator: Arc::new(RwLock::new(root.path.clone())),
                move_policy: Arc::new(RwLock::new(RootMovePolicy::Pause)),
                projection_storage: Arc::new(Mutex::new(None)),
                draft: LibraryRootDraft {
                    path: root.path.clone(),
                    recursive: root.recursive,
                    anchor: root.anchor.clone(),
                    path_policy: root.path_policy,
                },
                access,
                handles: Mutex::new(None),
                health: Arc::new(AtomicU8::new(RootHealth::Unknown as u8)),
                owner,
                _reservation: Reservation::new(1024 + root.path.as_os_str().len() * 4)?,
            }),
        })
    }
    pub fn draft(&self) -> LibraryRootDraft {
        let mut draft = self.inner.draft.clone();
        draft.path = self.current_path();
        draft
    }
    pub fn current_path(&self) -> PathBuf {
        self.inner
            .locator
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    fn physical_path(&self) -> Result<PathBuf> {
        self.check()?;
        let selected = self.current_path();
        let cached = self
            .inner
            .handles
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(handles) = cached.as_ref() else {
            return Ok(selected);
        };
        let root = handles.last().context("Library root handle missing")?;
        ensure!(
            platform::identity(root)? == self.inner.draft.anchor,
            RootAnchorChanged
        );
        let actual = platform::current_path(root)?;
        if !same_path(&actual, &selected)
            && *self
                .inner
                .move_policy
                .read()
                .unwrap_or_else(|error| error.into_inner())
                != RootMovePolicy::FollowSameObject
        {
            self.inner
                .health
                .store(RootHealth::NeedsReapproval as u8, Ordering::Release);
            return Err(RootAnchorChanged.into());
        }
        Ok(actual)
    }
    fn authorize_location(&self, actual: &Path, handles: &[File]) -> Result<()> {
        self.check()?;
        let root_path = self.physical_path()?;
        ensure!(
            within(actual, &root_path),
            "Opened file left its physical library root"
        );
        ensure!(
            self.inner.draft.recursive
                || actual
                    .parent()
                    .is_some_and(|parent| same_path(parent, &root_path)),
            "Opened file exceeds non-recursive library grant"
        );
        verify_ancestor(handles, &self.inner.draft.anchor, || self.check())?;
        self.check()
    }
    pub fn check(&self) -> Result<()> {
        ensure!(
            !self.inner.owner.is_closing(),
            "Library authority is closing"
        );
        self.inner.access.check()
    }
    pub fn health(&self) -> RootHealth {
        RootHealth::load(&self.inner.health)
    }
    pub fn retirement_barrier(&self) -> RetirementBarrier {
        RetirementBarrier {
            scope: self.inner.owner.clone(),
        }
    }
    pub fn proof(&self) -> Result<RootProof> {
        self.check()?;
        let handles = self
            .inner
            .handles
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .context("Library root has no verified proof")?
            .clone();
        ensure!(
            self.health() == RootHealth::Verified,
            "Library root has no current verified proof"
        );
        Ok(RootProof {
            draft: self.draft(),
            handles,
            access: self.inner.access.clone(),
        })
    }
    pub(crate) fn scan_guard(&self) -> RootScanGuard {
        RootScanGuard(self.clone())
    }
    pub fn verify(&self) -> Result<()> {
        self.check()?;
        let result = (|| -> Result<()> {
            let expected_path = self.physical_path()?;
            if let Some(proof) = self
                .inner
                .handles
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_ref()
            {
                let root = proof.last().context("Library root handle missing")?;
                platform::validate_directory(root)?;
                ensure!(
                    platform::identity(root)? == self.inner.draft.anchor
                        && same_path(&platform::current_path(root)?, &expected_path),
                    RootAnchorChanged
                );
                self.check()?;
                self.inner
                    .health
                    .store(RootHealth::Verified as u8, Ordering::Release);
                return Ok(());
            }
            let ticket = DirectoryTicket::new(self.inner.owner.clone())?;
            let (path, identity, handles) = platform::open_directory(&expected_path)?;
            ensure!(
                identity == self.inner.draft.anchor && same_path(&path, &expected_path),
                RootAnchorChanged
            );
            self.check()?;
            let proof = ticket.capture(handles);
            let mut cached = self
                .inner
                .handles
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            self.check()?;
            *cached = Some(proof);
            self.inner
                .health
                .store(RootHealth::Verified as u8, Ordering::Release);
            Ok(())
        })();
        if let Err(error) = &result
            && self.check().is_ok()
        {
            let health = if error.downcast_ref::<RootAnchorChanged>().is_some() {
                RootHealth::NeedsReapproval
            } else {
                RootHealth::Unavailable
            };
            self.inner.health.store(health as u8, Ordering::Release);
        }
        result
    }
    pub fn open_file(&self, path: impl AsRef<Path>) -> Result<AuthorizedFile> {
        self.verify()?;
        validate_path(path.as_ref())?;
        let path = normalize_local_path(path.as_ref());
        let root_path = self.physical_path()?;
        validate_path(&path)?;
        ensure!(within(&path, &root_path), "File is outside library root");
        if !self.inner.draft.recursive {
            ensure!(
                path.parent()
                    .is_some_and(|parent| same_path(parent, &root_path)),
                "File exceeds non-recursive library grant"
            );
        }
        let ticket = DirectoryTicket::new(self.inner.owner.clone())?;
        let (file, actual, identity, handles) = self.open_beneath(&path, false)?;
        verify_ancestor(&handles, &self.inner.draft.anchor, || self.check())?;
        ensure!(
            within(&actual, &root_path),
            "Opened file is outside library root"
        );
        self.verify()?;
        let root_handles = self
            .inner
            .handles
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .context("Library root handle missing")?
            .clone();
        let authorization = FileAuthorization {
            inner: Arc::new(AuthorizationInner {
                path: actual.clone(),
                binding: Arc::new(RwLock::new(actual.clone())),
                live_open: Arc::new(Mutex::new(VecDeque::with_capacity(
                    MAX_LIVE_OPENS_PER_ASSET,
                ))),
                expected_identity: Arc::new(RwLock::new(Some(identity.clone()))),
                explicit: None,
                roots: vec![self.clone()],
                gate: self.inner.access.alive.clone(),
                cancel: None,
                owner: self.inner.owner.clone(),
                download_output: None,
                _reservation: Reservation::new(actual.capacity() * 2 + 1024)?,
            }),
        };
        Ok(AuthorizedFile::new(
            file,
            actual,
            identity,
            vec![self.inner.access.clone()],
            vec![ticket.capture(handles), root_handles],
        )?
        .attach(authorization))
    }
    fn open_beneath(
        &self,
        path: &Path,
        pinned: bool,
    ) -> Result<(File, PathBuf, FileIdentity, Vec<File>)> {
        self.check()?;
        let actual_root = self.physical_path()?;
        ensure!(
            within(path, &actual_root),
            "File is outside the actual physical root"
        );
        let handles = self
            .inner
            .handles
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .context("Physical root proof missing")?
            .clone();
        let root = handles.last().context("Physical root handle missing")?;
        ensure!(
            platform::identity(root)? == self.inner.draft.anchor,
            RootAnchorChanged
        );
        platform::open_file_beneath(root, &actual_root, path, pinned)
    }
    pub fn open_directory(&self, path: impl AsRef<Path>) -> Result<AuthorizedDirectory> {
        self.verify()?;
        validate_path(path.as_ref())?;
        let path = normalize_local_path(path.as_ref());
        let root_path = self.physical_path()?;
        validate_path(&path)?;
        ensure!(
            within(&path, &root_path),
            "Directory is outside library root"
        );
        ensure!(
            self.inner.draft.recursive || same_path(&path, &root_path),
            "Directory exceeds non-recursive library grant"
        );
        let ticket = DirectoryTicket::new(self.inner.owner.clone())?;
        let (actual, _, handles) = platform::open_directory(&path)?;
        verify_ancestor(&handles, &self.inner.draft.anchor, || self.check())?;
        ensure!(
            same_path(&actual, &path),
            "Library directory changed while opening"
        );
        self.verify()?;
        Ok(AuthorizedDirectory {
            path: actual,
            _handles: ticket.capture(handles),
            access: self.inner.access.clone(),
            root: self.clone(),
        })
    }
}

#[derive(Debug)]
struct RootAnchorChanged;
impl std::fmt::Display for RootAnchorChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NeedsReapproval: library root identity changed")
    }
}
impl std::error::Error for RootAnchorChanged {}
pub struct RootProof {
    draft: LibraryRootDraft,
    handles: DirectoryLease,
    access: Access,
}
pub(crate) struct RootScanGuard(RootAuthority);
impl Drop for RootScanGuard {
    fn drop(&mut self) {
        if let Some(alive) = &self.0.inner.access.scope_alive {
            alive.store(false, Ordering::Release);
        }
        self.0
            .inner
            .handles
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }
}
impl std::fmt::Debug for RootProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RootProof")
            .field("draft", &self.draft)
            .finish()
    }
}

fn verify_ancestor(
    handles: &[File],
    expected: &FileIdentity,
    check: impl Fn() -> Result<()>,
) -> Result<()> {
    for directory in handles {
        check()?;
        if platform::identity(directory)? == *expected {
            return Ok(());
        }
    }
    anyhow::bail!("Opened path has no verified physical library root ancestor")
}

pub struct AuthorizedDirectory {
    path: PathBuf,
    _handles: DirectoryLease,
    access: Access,
    root: RootAuthority,
}
pub struct AuthorizedReadDir {
    inner: std::fs::ReadDir,
    _pins: Vec<File>,
}
impl Iterator for AuthorizedReadDir {
    type Item = io::Result<std::fs::DirEntry>;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}
impl AuthorizedDirectory {
    #[cfg(target_os = "linux")]
    pub(crate) fn native_file(&self) -> Result<&File> {
        self.access.check()?;
        self._handles
            .last()
            .context("Verified directory handle missing")
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn check(&self) -> Result<()> {
        self.access.check()?;
        self.root.verify()?;
        ensure!(
            same_path(
                &platform::current_path(
                    self._handles
                        .last()
                        .context("Library directory handle missing")?
                )?,
                &self.path
            ),
            "Library directory moved after verification"
        );
        Ok(())
    }
    pub fn read_dir(&self) -> Result<AuthorizedReadDir> {
        self.check()?;
        let (actual, _, pins) = platform::open_directory_pinned(&self.path)?;
        ensure!(
            same_path(&actual, &self.path),
            "Library directory changed before enumeration"
        );
        verify_ancestor(&pins, &self.root.inner.draft.anchor, || self.check())?;
        let inner = platform::read_directory(
            self._handles
                .last()
                .context("Library directory handle missing")?,
            &self.path,
        )?;
        self.check()?;
        Ok(AuthorizedReadDir { inner, _pins: pins })
    }
    pub fn metadata(&self) -> Result<Metadata> {
        self.check()?;
        Ok(self
            ._handles
            .last()
            .context("Library directory handle missing")?
            .metadata()?)
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        normalize_local_path(left)
            .to_string_lossy()
            .eq_ignore_ascii_case(&normalize_local_path(right).to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}
fn within(path: &Path, root: &Path) -> bool {
    #[cfg(windows)]
    {
        PathBuf::from(
            normalize_local_path(path)
                .to_string_lossy()
                .to_ascii_lowercase(),
        )
        .starts_with(PathBuf::from(
            normalize_local_path(root)
                .to_string_lossy()
                .to_ascii_lowercase(),
        ))
    }
    #[cfg(not(windows))]
    {
        path.starts_with(root)
    }
}
fn relative_suffix(path: &Path, root: &Path) -> Option<PathBuf> {
    if !within(path, root) {
        return None;
    }
    Some(path.components().skip(root.components().count()).collect())
}
fn old_locator_names_identity(path: &Path, expected: &FileIdentity) -> Result<bool> {
    match platform::open_file(path) {
        Ok((_, _, identity, _)) => Ok(&identity == expected),
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
        {
            Ok(false)
        }
        Err(error) => Err(error)
            .context("Cannot establish whether the original locator still names its object"),
    }
}

#[cfg(all(test, windows))]
mod proof_profile_tests {
    use super::*;
    #[test]
    fn root_relative_metadata_corpus_reports_actual_open_budget_and_time() {
        let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let nested = temp.path().join("batch");
        std::fs::create_dir(&nested).unwrap();
        let samples = 4410usize;
        let mut wave = Vec::new();
        wave.extend_from_slice(b"RIFF");
        wave.extend_from_slice(&(36u32 + samples as u32 * 2).to_le_bytes());
        wave.extend_from_slice(b"WAVEfmt ");
        wave.extend_from_slice(&16u32.to_le_bytes());
        wave.extend_from_slice(&1u16.to_le_bytes());
        wave.extend_from_slice(&1u16.to_le_bytes());
        wave.extend_from_slice(&44100u32.to_le_bytes());
        wave.extend_from_slice(&88200u32.to_le_bytes());
        wave.extend_from_slice(&2u16.to_le_bytes());
        wave.extend_from_slice(&16u16.to_le_bytes());
        wave.extend_from_slice(b"data");
        wave.extend_from_slice(&(samples as u32 * 2).to_le_bytes());
        for index in 0..samples {
            let sample =
                ((index as f32 + 1.0) * 997.0 * std::f32::consts::TAU / 44100.0).sin() * 12000.0;
            wave.extend_from_slice(&(sample as i16).to_le_bytes());
        }
        for index in 0..40 {
            std::fs::write(nested.join(format!("owned-{index:03}.wav")), &wave).unwrap();
        }
        let context = ScanContext::new(Duration::from_secs(30));
        let root = RootAuthority::open(temp.path(), true, &context).unwrap();
        let catalog = crate::catalog::Catalog::new().unwrap();
        RELATIVE_OPEN_COUNT.store(0, Ordering::Relaxed);
        let started = Instant::now();
        for index in 0..40 {
            let opened = root
                .open_file(nested.join(format!("owned-{index:03}.wav")))
                .unwrap();
            assert!(catalog.prepare_authorized(opened).unwrap().tags_verified());
        }
        let elapsed = started.elapsed();
        let opens = RELATIVE_OPEN_COUNT.load(Ordering::Relaxed);
        eprintln!(
            "actual metadata corpus files=40 bytes_per_file={} root_relative_opens={opens} elapsed_ms={}",
            wave.len(),
            elapsed.as_millis()
        );
        assert!(
            opens < 2000,
            "Small probes must not rebuild drive-root ancestry per read"
        );
    }
}

struct OpenPermit {
    file: LibraryOpenFile,
    path: PathBuf,
    identity: FileIdentity,
    stamp: RepresentationStamp,
    failed: AtomicBool,
    grants: Vec<Access>,
    _directories: Vec<DirectoryLease>,
    gate: Option<Arc<AtomicBool>>,
    authorization: Option<FileAuthorization>,
    _audio: Option<AuthorizedFile>,
    _reservation: Reservation,
}
#[derive(Clone, PartialEq, Eq)]
struct RepresentationStamp {
    size: u64,
    modified: std::time::SystemTime,
}
struct VerifiedContentEvidence {
    stamp: RepresentationStamp,
    #[cfg(target_os = "linux")]
    changed: (i64, i64),
}
impl VerifiedContentEvidence {
    fn capture(file: &File) -> Result<Self> {
        let stamp = RepresentationStamp::read(file)?;
        #[cfg(target_os = "linux")]
        let changed = {
            use std::os::unix::fs::MetadataExt;
            let metadata = file.metadata()?;
            (metadata.ctime(), metadata.ctime_nsec())
        };
        Ok(Self {
            stamp,
            #[cfg(target_os = "linux")]
            changed,
        })
    }
    fn check(&self, file: &File) -> Result<()> {
        let current = Self::capture(file)?;
        ensure!(
            current.stamp == self.stamp,
            "Verified content representation changed"
        );
        #[cfg(target_os = "linux")]
        ensure!(
            current.changed == self.changed,
            "Verified Linux file changed after content hashing"
        );
        Ok(())
    }
}
fn hash_verified_target(
    opened: &AuthorizedFile,
    expected_sha: &str,
) -> Result<VerifiedContentEvidence> {
    let evidence = VerifiedContentEvidence::capture(&opened.permit.file)?;
    ensure!(
        evidence.stamp.size <= MAX_CONTENT_HASH_BYTES,
        "Automatic content evidence exceeds 1 GiB"
    );
    let mut reader = opened.try_clone()?;
    reader.seek(SeekFrom::Start(0))?;
    use sha2::Digest;
    let mut digest = sha2::Sha256::new();
    let mut buffer = [0u8; MAX_READ_BYTES];
    let mut read_bytes = 0u64;
    let deadline = Instant::now() + Duration::from_secs(30);
    while read_bytes < evidence.stamp.size {
        ensure!(
            Instant::now() < deadline,
            "Automatic content verification deadline exceeded"
        );
        let remaining =
            usize::try_from((evidence.stamp.size - read_bytes).min(buffer.len() as u64))
                .context("Content evidence remaining size overflow")?;
        let count = reader.read(&mut buffer[..remaining])?;
        if count == 0 {
            break;
        }
        read_bytes = read_bytes
            .checked_add(count as u64)
            .context("Content evidence size overflow")?;
        ensure!(
            read_bytes <= evidence.stamp.size && read_bytes <= MAX_CONTENT_HASH_BYTES,
            "Automatic content evidence changed or exceeded its budget"
        );
        digest.update(&buffer[..count]);
    }
    ensure!(
        read_bytes == evidence.stamp.size,
        "Automatic content evidence length changed"
    );
    ensure!(
        format!("{:x}", digest.finalize()).eq_ignore_ascii_case(expected_sha),
        "Actual opened file does not match verified content evidence"
    );
    evidence.check(&opened.permit.file)?;
    Ok(evidence)
}
impl RepresentationStamp {
    fn read(file: &File) -> Result<Self> {
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file(),
            "Library object is no longer a regular file"
        );
        Ok(Self {
            size: metadata.len(),
            modified: metadata.modified()?,
        })
    }
}
struct ReadSnapshot {
    path: PathBuf,
    _file: LibraryOpenFile,
    directories: Vec<File>,
    _reservation: Reservation,
}
/// Clones retain the same verified handle, with independent logical cursors.
/// Reads use positional I/O and never reopen the original pathname.
pub struct AuthorizedFile {
    permit: Arc<OpenPermit>,
    position: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileChangeStamp {
    Windows { change_time_100ns: i64 },
    Unix { seconds: i64, nanoseconds: i64 },
}
#[derive(Debug)]
pub struct AuthorizedFileMetadata {
    pub metadata: Metadata,
    pub path: PathBuf,
    pub change_stamp: FileChangeStamp,
}
impl std::fmt::Debug for AuthorizedFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthorizedFile")
            .field("path", &self.permit.path)
            .field("identity", &self.permit.identity)
            .finish()
    }
}
impl AuthorizedFile {
    fn new(
        file: File,
        path: PathBuf,
        identity: FileIdentity,
        grants: Vec<Access>,
        directories: Vec<DirectoryLease>,
    ) -> Result<Self> {
        let reservation = Reservation::new(
            path.capacity() * 2 + 16 * 1024 + grants.capacity() * std::mem::size_of::<Access>(),
        )?;
        Ok(Self {
            permit: Arc::new(OpenPermit {
                stamp: RepresentationStamp::read(&file)?,
                failed: AtomicBool::new(false),
                file: file.into(),
                path,
                identity,
                grants,
                _directories: directories,
                gate: None,
                authorization: None,
                _audio: None,
                _reservation: reservation,
            }),
            position: 0,
        })
    }
    fn attach(mut self, authorization: FileAuthorization) -> Self {
        let permit = Arc::get_mut(&mut self.permit).expect("new file permit is unique");
        permit.gate = Some(authorization.inner.gate.clone());
        permit.authorization = Some(authorization);
        self
    }
    pub fn check(&self) -> Result<()> {
        self.read_snapshot()?;
        Ok(())
    }
    /// Inspects the retained native reader while its physical proof stays live.
    pub(crate) fn inspect_native<R>(&self, inspect: impl FnOnce(&File) -> Result<R>) -> Result<R> {
        let snapshot = self.read_snapshot()?;
        let result = inspect(&self.permit.file);
        self.finish_snapshot(&snapshot)?;
        result
    }
    fn check_gate(&self) -> Result<()> {
        ensure!(
            !self.permit.failed.load(Ordering::Acquire),
            "Opened library physical proof expired"
        );
        ensure!(
            !self
                .permit
                .gate
                .as_ref()
                .is_some_and(|gate| !gate.load(Ordering::Acquire)),
            "Library asset permission changed"
        );
        ensure!(
            self.permit.grants.iter().any(|grant| grant.check().is_ok())
                || self
                    .permit
                    .authorization
                    .as_ref()
                    .is_some_and(|authorization| authorization
                        .inner
                        .download_output
                        .as_ref()
                        .is_some_and(|download| download.verify().is_ok())),
            "Library file has no active verified grant"
        );
        if let Some(authorization) = &self.permit.authorization {
            ensure!(
                !authorization
                    .inner
                    .cancel
                    .as_ref()
                    .is_some_and(|cancel| cancel.load(Ordering::Acquire)),
                "Library read cancelled"
            );
            if let Some(identity) = &*authorization
                .inner
                .expected_identity
                .read()
                .unwrap_or_else(|error| error.into_inner())
            {
                ensure!(
                    &self.permit.identity == identity,
                    "NeedsReapproval: opened library identity changed"
                );
            }
        }
        if let Some(audio) = &self.permit._audio {
            audio.check()?;
        }
        Ok(())
    }
    fn read_snapshot(&self) -> Result<ReadSnapshot> {
        self.check_gate()?;
        if let Some(snapshot) = self.download_snapshot()? {
            return Ok(snapshot);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        let result = loop {
            self.check_gate()?;
            let attempt = (|| -> Result<ReadSnapshot> {
                ensure!(
                    platform::identity(&self.permit.file)? == self.permit.identity,
                    "Opened library file identity changed"
                );
                let claimed = self
                    .permit
                    .authorization
                    .as_ref()
                    .map_or_else(|| self.permit.path.clone(), FileAuthorization::current_path);
                let current = platform::current_path(&self.permit.file)?;
                let mut last_error = None;
                for path in [claimed.clone(), current] {
                    if self
                        .permit
                        .authorization
                        .as_ref()
                        .is_some_and(|authorization| !authorization.may_cover_location(&path))
                    {
                        continue;
                    }
                    let candidate = (|| -> Result<ReadSnapshot> {
                        let reservation = Reservation::new(
                            MAX_COMPONENTS * std::mem::size_of::<File>() * 2 + MAX_PATH_BYTES * 2,
                        )?;
                        let rooted = self
                            .permit
                            .authorization
                            .as_ref()
                            .and_then(|authorization| {
                                authorization.inner.roots.iter().find(|root| {
                                    root.physical_path()
                                        .is_ok_and(|actual| within(&path, &actual))
                                })
                            });
                        let cached_parent = self.permit._directories.iter().find(|lease| {
                            lease.last().is_some_and(|parent| {
                                path.parent().is_some_and(|wanted| {
                                    platform::current_path(parent)
                                        .is_ok_and(|actual| same_path(wanted, &actual))
                                })
                            })
                        });
                        let opened = if let Some(lease) = cached_parent {
                            platform::open_file_from_parent(lease, &path)
                        } else if let Some(root) = rooted {
                            root.open_beneath(&path, true)
                        } else {
                            platform::open_file_pinned(&path)
                        };
                        let (file, actual, identity, directories) = opened?;
                        ensure!(
                            identity == self.permit.identity && same_path(&actual, &path),
                            "Opened library alias differs from its original physical object"
                        );
                        let snapshot = ReadSnapshot {
                            path,
                            _file: file.into(),
                            directories,
                            _reservation: reservation,
                        };
                        self.verify_snapshot(&snapshot)?;
                        Ok(snapshot)
                    })();
                    match candidate {
                        Ok(snapshot) => return Ok(snapshot),
                        Err(error) => last_error = Some(error),
                    }
                }
                Err(last_error.unwrap_or_else(|| {
                    anyhow::anyhow!("Opened file has no applicable physical namespace grant")
                }))
            })();
            match attempt {
                Err(error)
                    if Instant::now() < deadline
                        && error.downcast_ref::<io::Error>().is_some_and(|error| {
                            error.kind() == io::ErrorKind::NotFound
                                || matches!(error.raw_os_error(), Some(32 | 33))
                        }) =>
                {
                    self.check_gate()?;
                    std::thread::sleep(Duration::from_millis(10));
                }
                result => break result,
            }
        };
        if result.is_err() {
            self.permit.failed.store(true, Ordering::Release);
        }
        result
    }
    fn verify_snapshot(&self, snapshot: &ReadSnapshot) -> Result<()> {
        self.check_gate()?;
        ensure!(
            platform::identity(&self.permit.file)? == self.permit.identity,
            "Opened library file identity changed"
        );
        ensure!(
            platform::identity(&snapshot._file)? == self.permit.identity,
            "Opened library alias changed identity during read"
        );
        let actual = platform::current_path(&snapshot._file)?;
        let actual = if platform::link_count(&snapshot._file)? > 1 {
            &snapshot.path
        } else {
            &actual
        };
        if let Some(authorization) = &self.permit.authorization {
            authorization.authorize_location(actual, &snapshot.directories)?;
        }
        self.check_gate()
    }
    fn finish_snapshot(&self, snapshot: &ReadSnapshot) -> Result<()> {
        let result = self.verify_snapshot(snapshot);
        if result.is_err() {
            self.permit.failed.store(true, Ordering::Release);
        }
        result
    }
    pub fn path(&self) -> &Path {
        &self.permit.path
    }
    pub fn current_path(&self) -> Result<PathBuf> {
        let snapshot = self.read_snapshot()?;
        Ok(snapshot.path)
    }
    pub fn authorization(&self) -> FileAuthorization {
        self.permit
            .authorization
            .as_ref()
            .expect("authorized file has its grant")
            .clone()
    }
    pub fn identity(&self) -> &FileIdentity {
        &self.permit.identity
    }
    pub fn metadata(&self) -> Result<Metadata> {
        let snapshot = self.read_snapshot()?;
        let metadata = self.permit.file.metadata()?;
        self.finish_snapshot(&snapshot)?;
        Ok(metadata)
    }
    pub fn change_stamp(&self) -> Result<FileChangeStamp> {
        let snapshot = self.read_snapshot()?;
        let stamp = platform::change_stamp(&self.permit.file)?;
        self.finish_snapshot(&snapshot)?;
        Ok(stamp)
    }
    /// One bounded metadata operation holds the same actual namespace proof
    /// across the kernel metadata and change-time queries. No handle escapes.
    pub fn metadata_snapshot(&self) -> Result<AuthorizedFileMetadata> {
        let snapshot = self.read_snapshot()?;
        let metadata = self.permit.file.metadata()?;
        let change_stamp = platform::change_stamp(&self.permit.file)?;
        self.finish_snapshot(&snapshot)?;
        Ok(AuthorizedFileMetadata {
            metadata,
            path: snapshot.path,
            change_stamp,
        })
    }
    pub fn try_clone(&self) -> Result<Self> {
        self.check()?;
        Ok(Self {
            permit: self.permit.clone(),
            position: self.position,
        })
    }
}
impl Read for AuthorizedFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let snapshot = self.read_snapshot().map_err(io::Error::other)?;
        let limit = buffer.len().min(MAX_READ_BYTES);
        let buffer = &mut buffer[..limit];
        #[cfg(windows)]
        let result = {
            use std::os::windows::fs::FileExt;
            self.permit.file.seek_read(buffer, self.position)
        };
        #[cfg(unix)]
        let result = {
            use std::os::unix::fs::FileExt;
            self.permit.file.read_at(buffer, self.position)
        };
        #[cfg(not(any(windows, unix)))]
        let result: io::Result<usize> = Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Verified positional file access is unsupported",
        ));
        let count = match result {
            Ok(count) => count,
            Err(error) => {
                buffer.fill(0);
                return Err(error);
            }
        };
        if let Err(error) = self.verify_snapshot(&snapshot) {
            buffer[..count].fill(0);
            self.permit.failed.store(true, Ordering::Release);
            return Err(io::Error::other(error));
        }
        self.position = self
            .position
            .checked_add(count as u64)
            .ok_or_else(|| io::Error::other("File cursor overflow"))?;
        Ok(count)
    }
}
impl Seek for AuthorizedFile {
    fn seek(&mut self, seek: SeekFrom) -> io::Result<u64> {
        let snapshot = self.read_snapshot().map_err(io::Error::other)?;
        let position = match seek {
            SeekFrom::Start(value) => value as i128,
            SeekFrom::Current(delta) => self.position as i128 + delta as i128,
            SeekFrom::End(delta) => self.permit.file.metadata()?.len() as i128 + delta as i128,
        };
        ensure_seek(position)?;
        self.finish_snapshot(&snapshot).map_err(io::Error::other)?;
        self.position = position as u64;
        Ok(self.position)
    }
}
fn ensure_seek(position: i128) -> io::Result<()> {
    if !(0..=u64::MAX as i128).contains(&position) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid file seek",
        ));
    }
    Ok(())
}

struct RootGrant {
    epoch: u64,
    authority: RootAuthority,
}
struct InstalledAsset {
    digest: String,
    identity: Option<FileIdentity>,
    authorization: FileAuthorization,
}
#[derive(Default)]
struct RegistryState {
    roots: BTreeMap<String, RootGrant>,
    explicit: BTreeMap<String, (u64, Access)>,
    assets: BTreeMap<String, InstalledAsset>,
    revoked_roots: BTreeMap<String, u64>,
    revoked_files: BTreeMap<String, u64>,
    // Epoch tombstones must remain bounded after their root handles are gone.
    // The reservation also covers each future revocation-map entry.
    _history: BTreeMap<String, Reservation>,
}
#[derive(Clone, Default)]
pub struct AuthorityRegistry {
    inner: Arc<RwLock<RegistryState>>,
    owner: Arc<RetirementScope>,
}
struct AuthorizationInner {
    path: PathBuf,
    binding: Arc<RwLock<PathBuf>>,
    live_open: Arc<Mutex<VecDeque<Weak<OpenPermit>>>>,
    expected_identity: Arc<RwLock<Option<FileIdentity>>>,
    explicit: Option<Access>,
    roots: Vec<RootAuthority>,
    gate: Arc<AtomicBool>,
    cancel: Option<Arc<AtomicBool>>,
    owner: Arc<RetirementScope>,
    download_output: Option<DownloadOutputAuthority>,
    _reservation: Reservation,
}
#[derive(Clone)]
pub struct FileAuthorization {
    inner: Arc<AuthorizationInner>,
}
#[derive(Clone, Debug)]
pub struct LiveRelocationHint {
    pub asset_id: String,
    pub old_path: PathBuf,
    pub current_path: PathBuf,
    pub file_identity: FileIdentity,
    pub requires_selection: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelocationAdmissionError {
    OriginalLiveProofUnavailable,
    OriginalRepresentationChanged,
    AmbiguousOriginalAlias,
}
impl std::fmt::Display for RelocationAdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::OriginalLiveProofUnavailable => {
                "Automatic relocation requires a live original physical proof"
            }
            Self::OriginalRepresentationChanged => {
                "Automatic relocation original representation changed"
            }
            Self::AmbiguousOriginalAlias => {
                "Automatic relocation requires selection for an original alias or hardlink"
            }
        })
    }
}
impl std::error::Error for RelocationAdmissionError {}

/// A replacement prepared without opening a read gate or changing live grants.
pub struct PreparedAuthorityInstall {
    record: LocalAssetAuthority,
    identity: Option<FileIdentity>,
    digest: String,
    expected_previous: Option<(String, Option<FileIdentity>)>,
    authorization: FileAuthorization,
    history: Vec<(String, Reservation)>,
    same_content: bool,
    keep_old_reader: Option<Arc<OpenPermit>>,
}
pub struct PreparedRootRelocation {
    root_id: String,
    epoch: u64,
    old_path: PathBuf,
    draft: LibraryRootDraft,
    authority: RootAuthority,
    proof: DirectoryLease,
    descendants: Vec<(FileAuthorization, PathBuf)>,
    _reservation: Reservation,
    manual: bool,
}
pub struct PreparedAssetRelocation {
    record: LocalAssetAuthority,
    identity: FileIdentity,
    digest: String,
    expected_digest: String,
    previous: FileAuthorization,
    authorization: FileAuthorization,
    _original: Option<Arc<OpenPermit>>,
    _target: AuthorizedFile,
    _reservation: Reservation,
    evidence: crate::domain::library_watch::RelinkEvidence,
    content: Option<VerifiedContentEvidence>,
    old_locator: Option<PathBuf>,
    expected_binding_revision: Option<u64>,
}
impl PreparedRootRelocation {
    pub fn draft(&self) -> LibraryRootDraft {
        self.draft.clone()
    }
    pub fn check(&self) -> Result<()> {
        self.authority.check()?;
        ensure!(
            self.manual
                || *self
                    .authority
                    .inner
                    .move_policy
                    .read()
                    .unwrap_or_else(|error| error.into_inner())
                    == RootMovePolicy::FollowSameObject,
            "Root move policy changed during preparation"
        );
        let root = self
            .proof
            .last()
            .context("Prepared root relocation handle missing")?;
        ensure!(
            platform::identity(root)? == self.draft.anchor
                && same_path(&platform::current_path(root)?, &self.draft.path),
            "Prepared root relocation moved or changed"
        );
        Ok(())
    }
}
impl std::fmt::Debug for PreparedRootRelocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedRootRelocation")
            .field("root_id", &self.root_id)
            .field("path", &self.draft.path)
            .finish()
    }
}
impl std::fmt::Debug for PreparedAssetRelocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedAssetRelocation")
            .field("asset_id", &self.record.asset_id)
            .field("path", &self.record.path)
            .finish()
    }
}
impl PreparedAssetRelocation {
    pub fn binding_revision(&self) -> Option<u64> {
        self.expected_binding_revision
    }
    pub fn evidence(&self) -> crate::domain::library_watch::RelinkEvidence {
        self.evidence
    }
    pub fn check(&self) -> Result<()> {
        if self._original.is_some() {
            self.previous.check()?;
        } else {
            self.previous.check_binding_gate()?;
        }
        self._target.check()?;
        ensure!(
            same_path(&self._target.current_path()?, &self.record.path)
                && platform::identity(&self._target.permit.file)? == self.identity
                && platform::link_count(&self._target.permit.file)? == 1,
            "Prepared asset relocation moved, changed or gained an ambiguous hardlink"
        );
        if let Some(original) = &self._original {
            ensure!(
                platform::identity(&original.file)? == self.identity
                    && same_path(&platform::current_path(&original.file)?, &self.record.path)
                    && RepresentationStamp::read(&original.file)? == original.stamp
                    && RepresentationStamp::read(&self._target.permit.file)? == original.stamp,
                "Prepared original opened representation changed"
            );
        }
        if let Some(old_locator) = &self.old_locator {
            ensure!(
                !old_locator_names_identity(old_locator, &self.identity)?,
                "Automatic relocation still has an original same-object alias"
            );
        }
        if let Some(content) = &self.content {
            content.check(&self._target.permit.file)?;
        }
        Ok(())
    }
}
impl std::fmt::Debug for FileAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileAuthorization")
            .field("path", &self.inner.path)
            .field(
                "expected_identity",
                &*self
                    .inner
                    .expected_identity
                    .read()
                    .unwrap_or_else(|error| error.into_inner()),
            )
            .field("active", &self.inner.gate.load(Ordering::Acquire))
            .finish()
    }
}
impl AuthorityRegistry {
    pub fn live_relocation_hints(
        &self,
        root_id: &str,
        grant_epoch: u64,
        limit: usize,
    ) -> Result<Vec<LiveRelocationHint>> {
        ensure!(
            limit <= MAX_RELOCATION_HINTS,
            "Live relocation hint limit exceeds 200"
        );
        ensure!(!self.owner.is_closing(), "Library authority is closing");
        let state = self.inner.read().unwrap_or_else(|error| error.into_inner());
        let active = state
            .roots
            .get(root_id)
            .filter(|root| root.epoch == grant_epoch)
            .context("Live relocation root grant expired")?;
        active.authority.check()?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut result = Vec::with_capacity(limit);
        let mut bytes = 0usize;
        for (asset_id, installed) in &state.assets {
            if result.len() == limit {
                break;
            }
            if !installed
                .authorization
                .inner
                .roots
                .iter()
                .any(|root| Arc::ptr_eq(&root.inner, &active.authority.inner))
                || installed.authorization.check_binding_gate().is_err()
            {
                continue;
            }
            let live: Vec<_> = installed
                .authorization
                .inner
                .live_open
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .iter()
                .rev()
                .filter_map(Weak::upgrade)
                .collect();
            let old_path = installed.authorization.current_path();
            for permit in live {
                let Ok(current_path) = platform::current_path(&permit.file) else {
                    continue;
                };
                if permit.failed.load(Ordering::Acquire) || same_path(&current_path, &old_path) {
                    continue;
                }
                let original = AuthorizedFile {
                    permit,
                    position: 0,
                };
                let Ok(snapshot) = original.read_snapshot() else {
                    continue;
                };
                if !same_path(&snapshot.path, &current_path) {
                    let Ok(root_path) = active.authority.physical_path() else {
                        continue;
                    };
                    if !within(&current_path, &root_path) {
                        continue;
                    }
                    let Ok((_, actual, identity, directories)) =
                        platform::open_file_pinned(&current_path)
                    else {
                        continue;
                    };
                    if identity != *original.identity()
                        || !same_path(&actual, &current_path)
                        || active
                            .authority
                            .authorize_location(&actual, &directories)
                            .is_err()
                    {
                        continue;
                    }
                } else if active
                    .authority
                    .authorize_location(&current_path, &snapshot.directories)
                    .is_err()
                {
                    continue;
                }
                let requires_selection = platform::link_count(&original.permit.file)? > 1
                    || old_locator_names_identity(&old_path, original.identity()).unwrap_or(true);
                let additional = asset_id.capacity()
                    + old_path.capacity()
                    + current_path.capacity()
                    + original.identity().volume.capacity()
                    + original.identity().file_id.capacity()
                    + 512;
                if bytes
                    .checked_add(additional)
                    .is_none_or(|total| total > MAX_RELOCATION_HINT_BYTES)
                {
                    return Ok(result);
                }
                bytes += additional;
                result.push(LiveRelocationHint {
                    asset_id: asset_id.clone(),
                    old_path,
                    current_path,
                    file_identity: original.identity().clone(),
                    requires_selection,
                });
                break;
            }
        }
        Ok(result)
    }
    pub fn apply_root_move_policy(
        &self,
        root_id: &str,
        grant_epoch: u64,
        policy: RootMovePolicy,
    ) -> Result<()> {
        ensure!(!self.owner.is_closing(), "Library authority is closing");
        let state = self.inner.read().unwrap_or_else(|error| error.into_inner());
        let root = state
            .roots
            .get(root_id)
            .filter(|root| root.epoch == grant_epoch)
            .context("Root move policy grant expired")?;
        root.authority.check()?;
        *root
            .authority
            .inner
            .move_policy
            .write()
            .unwrap_or_else(|error| error.into_inner()) = policy;
        Ok(())
    }
    pub fn prepare_root_relocation(
        &self,
        root: &LibraryRoot,
        new_path: &Path,
    ) -> Result<PreparedRootRelocation> {
        self.prepare_root_relocation_impl(root, new_path, false)
    }
    pub fn prepare_manual_root_relocation(
        &self,
        root: &LibraryRoot,
        proof: RootProof,
    ) -> Result<PreparedRootRelocation> {
        proof.access.check()?;
        ensure!(
            proof.draft.anchor == root.anchor
                && proof.draft.recursive == root.recursive
                && proof.draft.path_policy == root.path_policy,
            "Manual root relocation proof differs from the original root"
        );
        let selected = proof
            .handles
            .last()
            .context("Manual selected root handle missing")?;
        ensure!(
            platform::identity(selected)? == root.anchor
                && same_path(&platform::current_path(selected)?, &proof.draft.path),
            "Manual selected root proof changed"
        );
        self.prepare_root_relocation_impl(root, &proof.draft.path, true)
    }
    fn prepare_root_relocation_impl(
        &self,
        root: &LibraryRoot,
        new_path: &Path,
        manual: bool,
    ) -> Result<PreparedRootRelocation> {
        root.validate()?;
        validate_path(new_path)?;
        let state = self.inner.read().unwrap_or_else(|error| error.into_inner());
        let active = state
            .roots
            .get(&root.root_id)
            .filter(|active| active.epoch == root.grant_epoch)
            .context("Root relocation grant expired")?;
        active.authority.check()?;
        let draft = active.authority.draft();
        ensure!(
            draft.anchor == root.anchor
                && same_path(&draft.path, &root.path)
                && draft.recursive == root.recursive
                && draft.path_policy == root.path_policy,
            "Root relocation intent changed"
        );
        ensure!(
            manual
                || *active
                    .authority
                    .inner
                    .move_policy
                    .read()
                    .unwrap_or_else(|error| error.into_inner())
                    == RootMovePolicy::FollowSameObject,
            "Root relocation requires FollowSameObject"
        );
        let cached = active
            .authority
            .inner
            .handles
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .context("Root relocation requires a live original handle")?;
        let original = cached
            .last()
            .context("Root relocation original handle missing")?;
        ensure!(
            platform::identity(original)? == root.anchor
                && same_path(&platform::current_path(original)?, new_path),
            "Root relocation is not the original open object"
        );
        let ticket = DirectoryTicket::new(self.owner.clone())?;
        let (actual, anchor, handles) = platform::open_directory(new_path)?;
        ensure!(
            anchor == root.anchor && same_path(&actual, new_path),
            "Root relocation physical anchor changed"
        );
        let mut descendants = Vec::new();
        let mut bytes = actual.as_os_str().len() * 4 + 1024;
        for installed in state.assets.values() {
            let old = installed.authorization.current_path();
            if within(&old, &root.path)
                && installed
                    .authorization
                    .inner
                    .roots
                    .iter()
                    .any(|grant| Arc::ptr_eq(&grant.inner, &active.authority.inner))
            {
                let suffix = relative_suffix(&old, &root.path)
                    .context("Root descendant cannot be projected")?;
                let projected = actual.join(suffix);
                validate_path(&projected)?;
                bytes = bytes
                    .checked_add(
                        projected.capacity() * 2
                            + std::mem::size_of::<(FileAuthorization, PathBuf)>() * 2,
                    )
                    .context("Root relocation budget overflow")?;
                ensure!(
                    bytes <= MAX_AUTHORITY_BYTES,
                    "Root relocation budget exceeded"
                );
                descendants.push((installed.authorization.clone(), projected));
            }
        }
        let reservation = Reservation::new(bytes)?;
        ensure!(
            platform::identity(original)? == anchor
                && same_path(&platform::current_path(original)?, &actual),
            "Root moved again during relocation preparation"
        );
        Ok(PreparedRootRelocation {
            root_id: root.root_id.clone(),
            epoch: root.grant_epoch,
            old_path: root.path.clone(),
            draft: LibraryRootDraft {
                path: actual,
                recursive: root.recursive,
                anchor,
                path_policy: root.path_policy,
            },
            authority: active.authority.clone(),
            proof: ticket.capture(handles),
            descendants,
            _reservation: reservation,
            manual,
        })
    }
    pub fn publish_root_relocation(
        &self,
        prepared: PreparedRootRelocation,
        committed: &LibraryRoot,
    ) -> Result<()> {
        ensure!(
            committed.root_id == prepared.root_id
                && committed.grant_epoch == prepared.epoch
                && committed.anchor == prepared.draft.anchor
                && same_path(&committed.path, &prepared.draft.path)
                && committed.recursive == prepared.draft.recursive
                && committed.path_policy == prepared.draft.path_policy,
            "Committed root relocation differs from reserved proof"
        );
        ensure!(!self.owner.is_closing(), "Library authority is closing");
        let state = self
            .inner
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let active = state
            .roots
            .get(&prepared.root_id)
            .filter(|active| {
                active.epoch == prepared.epoch
                    && Arc::ptr_eq(&active.authority.inner, &prepared.authority.inner)
            })
            .context("Root relocation grant changed during commit")?;
        active.authority.check()?;
        ensure!(
            same_path(&active.authority.current_path(), &prepared.old_path),
            "Root locator changed during commit"
        );
        ensure!(
            prepared.manual
                || *active
                    .authority
                    .inner
                    .move_policy
                    .read()
                    .unwrap_or_else(|error| error.into_inner())
                    == RootMovePolicy::FollowSameObject,
            "Root move policy changed during commit"
        );
        *active
            .authority
            .inner
            .locator
            .write()
            .unwrap_or_else(|error| error.into_inner()) = prepared.draft.path;
        *active
            .authority
            .inner
            .handles
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(prepared.proof);
        for (authorization, path) in prepared.descendants {
            *authorization
                .inner
                .binding
                .write()
                .unwrap_or_else(|error| error.into_inner()) = path;
        }
        *active
            .authority
            .inner
            .projection_storage
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(prepared._reservation);
        active
            .authority
            .inner
            .health
            .store(RootHealth::Verified as u8, Ordering::Release);
        Ok(())
    }
    pub fn prepare_asset_relocation(
        &self,
        record: &LocalAssetAuthority,
        identity: &FileIdentity,
        opened: &AuthorizedFile,
    ) -> Result<PreparedAssetRelocation> {
        #[cfg(target_os = "linux")]
        {
            let _ = (record, identity, opened);
            anyhow::bail!(
                "UnsupportedCapability: automatic Linux relocation requires verified content evidence"
            );
        }
        #[cfg(not(target_os = "linux"))]
        self.prepare_asset_relocation_impl(record, identity, opened, None)
    }
    pub fn prepare_asset_relocation_with_content(
        &self,
        record: &LocalAssetAuthority,
        identity: &FileIdentity,
        opened: &AuthorizedFile,
        verified_old_sha: &str,
        new_verified_sha: &str,
    ) -> Result<PreparedAssetRelocation> {
        ensure!(
            verified_old_sha.len() == 64
                && verified_old_sha
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                && verified_old_sha.eq_ignore_ascii_case(new_verified_sha),
            "Automatic relocation content evidence does not match"
        );
        let mut prepared = self.prepare_asset_relocation_impl(record, identity, opened, None)?;
        let evidence = hash_verified_target(opened, new_verified_sha)?;
        prepared.evidence = crate::domain::library_watch::RelinkEvidence::SameFileAndContent;
        prepared.content = Some(evidence);
        prepared.check()?;
        Ok(prepared)
    }
    pub fn prepare_asset_relocation_from_binding(
        &self,
        old_binding: &LibraryAssetBinding,
        record: &LocalAssetAuthority,
        opened: &AuthorizedFile,
    ) -> Result<PreparedAssetRelocation> {
        #[cfg(not(windows))]
        {
            let _ = (old_binding, record, opened);
            anyhow::bail!(
                "UnsupportedCapability: no-live automatic relocation requires Windows 128-bit file identity and verified content"
            );
        }
        #[cfg(windows)]
        {
            ensure!(
                old_binding.asset_id == record.asset_id && old_binding.binding_revision > 0,
                "Historical relocation binding does not identify the original asset"
            );
            validate_path(&old_binding.path)?;
            validate_path(&record.path)?;
            let identity = old_binding
                .file_identity
                .as_ref()
                .context("Historical relocation has no verified original identity")?;
            identity.validate()?;
            ensure!(
                identity.kind == "windows_file_id" && identity.file_id.len() == 32,
                "Historical relocation requires the original 128-bit Windows file identity"
            );
            let sha = old_binding
                .content_sha256
                .as_deref()
                .context("Historical relocation requires verified original content")?;
            ensure!(
                sha.len() == 64 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "Invalid verified original SHA256"
            );
            ensure!(
                record.explicit_file_epoch.is_none(),
                "Automatic relocation cannot renew an exact file grant"
            );
            opened.check()?;
            let source = opened
                .permit
                .authorization
                .as_ref()
                .context("Historical relocation target has no actual root authorization")?;
            ensure!(
                source.inner.explicit.is_none() && !source.inner.roots.is_empty(),
                "Historical relocation target must be opened through a verified root"
            );
            let snapshot = opened.read_snapshot()?;
            ensure!(
                opened.identity() == identity && same_path(&snapshot.path, &record.path),
                "Historical relocation actual target identity or path changed"
            );
            let stamp = RepresentationStamp::read(&opened.permit.file)?;
            ensure!(
                old_binding.hash_size_bytes == Some(stamp.size),
                "Historical relocation verified hash length differs from target"
            );
            ensure!(
                platform::link_count(&opened.permit.file)? == 1
                    && !old_locator_names_identity(&old_binding.path, identity)?,
                "Historical relocation requires selection for an original alias or hardlink"
            );
            let mut prepared = {
                let state = self.inner.read().unwrap_or_else(|error| error.into_inner());
                let existing = state
                    .assets
                    .get(&record.asset_id)
                    .context("Historical relocation original asset is unavailable")?;
                existing.authorization.check_binding_gate()?;
                ensure!(
                    existing.identity.as_ref() == Some(identity)
                        && same_path(&existing.authorization.current_path(), &old_binding.path),
                    "Historical relocation binding changed in the live registry"
                );
                let mut roots = Vec::with_capacity(record.roots.len());
                for root in &record.roots {
                    let active = state
                        .roots
                        .get(&root.root_id)
                        .filter(|active| active.epoch == root.grant_epoch)
                        .context("Historical relocation root epoch expired")?;
                    let draft = active.authority.draft();
                    ensure!(
                        draft.anchor == root.anchor
                            && same_path(&draft.path, &root.path)
                            && draft.recursive == root.recursive
                            && draft.path_policy == root.path_policy,
                        "Historical relocation root intent changed"
                    );
                    active
                        .authority
                        .authorize_location(&snapshot.path, &snapshot.directories)?;
                    roots.push(active.authority.clone());
                }
                ensure!(
                    !roots.is_empty(),
                    "Historical relocation requires an existing actual root grant"
                );
                let reservation = Reservation::new(
                    record.path.capacity() * 4
                        + old_binding.path.capacity() * 2
                        + roots.capacity() * std::mem::size_of::<RootAuthority>() * 2
                        + 4096,
                )?;
                let authorization = FileAuthorization {
                    inner: Arc::new(AuthorizationInner {
                        path: existing.authorization.inner.path.clone(),
                        binding: existing.authorization.inner.binding.clone(),
                        live_open: existing.authorization.inner.live_open.clone(),
                        expected_identity: existing.authorization.inner.expected_identity.clone(),
                        explicit: None,
                        roots,
                        gate: existing.authorization.inner.gate.clone(),
                        cancel: None,
                        owner: self.owner.clone(),
                        download_output: None,
                        _reservation: Reservation::new(record.path.capacity() * 2 + 1024)?,
                    }),
                };
                PreparedAssetRelocation {
                    record: record.clone(),
                    identity: identity.clone(),
                    digest: record.permission_sha256()?,
                    expected_digest: existing.digest.clone(),
                    previous: existing.authorization.clone(),
                    authorization,
                    _original: None,
                    _target: opened.try_clone()?,
                    _reservation: reservation,
                    evidence: crate::domain::library_watch::RelinkEvidence::SameFileAndContent,
                    content: None,
                    old_locator: Some(old_binding.path.clone()),
                    expected_binding_revision: Some(old_binding.binding_revision),
                }
            };
            prepared.content = Some(hash_verified_target(opened, sha)?);
            prepared.check()?;
            Ok(prepared)
        }
    }
    fn prepare_asset_relocation_impl(
        &self,
        record: &LocalAssetAuthority,
        identity: &FileIdentity,
        opened: &AuthorizedFile,
        content: Option<VerifiedContentEvidence>,
    ) -> Result<PreparedAssetRelocation> {
        validate_path(&record.path)?;
        identity.validate()?;
        opened.check()?;
        let source_grant = opened
            .permit
            .authorization
            .as_ref()
            .context("Automatic relocation target has no physical root authorization")?;
        ensure!(
            source_grant.inner.explicit.is_none() && !source_grant.inner.roots.is_empty(),
            "Automatic relocation target must be opened through a verified root grant"
        );
        ensure!(
            opened.identity() == identity && same_path(&opened.current_path()?, &record.path),
            "Relocation target does not match its opened proof"
        );
        let state = self.inner.read().unwrap_or_else(|error| error.into_inner());
        let existing = state
            .assets
            .get(&record.asset_id)
            .context("Relocation original asset is unavailable")?;
        existing.authorization.check_binding_gate()?;
        ensure!(
            existing.identity.as_ref() == Some(identity),
            "Relocation original identity is unavailable"
        );
        let original = existing
            .authorization
            .inner
            .live_open
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .rev()
            .filter_map(Weak::upgrade)
            .find(|opened| !opened.failed.load(Ordering::Acquire) && opened.identity == *identity)
            .ok_or(RelocationAdmissionError::OriginalLiveProofUnavailable)?;
        ensure!(
            !original.failed.load(Ordering::Acquire)
                && platform::identity(&original.file)? == *identity
                && RepresentationStamp::read(&original.file)? == original.stamp
                && RepresentationStamp::read(&opened.permit.file)? == original.stamp,
            RelocationAdmissionError::OriginalRepresentationChanged
        );
        let target_snapshot = opened.read_snapshot()?;
        ensure!(
            platform::link_count(&opened.permit.file)? == 1
                && !old_locator_names_identity(&existing.authorization.current_path(), identity)?,
            RelocationAdmissionError::AmbiguousOriginalAlias
        );
        let mut roots = Vec::with_capacity(record.roots.len());
        for root in &record.roots {
            let active = state
                .roots
                .get(&root.root_id)
                .filter(|active| active.epoch == root.grant_epoch)
                .context("Relocation root grant expired")?;
            let draft = active.authority.draft();
            ensure!(
                draft.anchor == root.anchor
                    && same_path(&draft.path, &root.path)
                    && draft.recursive == root.recursive
                    && draft.path_policy == root.path_policy,
                "Relocation root intent changed"
            );
            active
                .authority
                .authorize_location(&target_snapshot.path, &target_snapshot.directories)?;
            roots.push(active.authority.clone());
        }
        ensure!(
            !roots.is_empty(),
            "Automatic relocation requires a verified root grant"
        );
        ensure!(
            same_path(
                &platform::current_path(&original.file)?,
                &target_snapshot.path
            ),
            "Live original object is not at the relocation target"
        );
        let old_snapshot = AuthorizedFile {
            permit: original.clone(),
            position: 0,
        }
        .read_snapshot()
        .map_err(|error| error.context(RelocationAdmissionError::OriginalLiveProofUnavailable))?;
        ensure!(
            same_path(&old_snapshot.path, &target_snapshot.path),
            "Original opened object moved during relocation proof"
        );
        let reservation = Reservation::new(
            record.path.capacity() * 4
                + roots.capacity() * std::mem::size_of::<RootAuthority>() * 2
                + 4096,
        )?;
        let authorization = FileAuthorization {
            inner: Arc::new(AuthorizationInner {
                path: existing.authorization.inner.path.clone(),
                binding: existing.authorization.inner.binding.clone(),
                live_open: existing.authorization.inner.live_open.clone(),
                expected_identity: existing.authorization.inner.expected_identity.clone(),
                explicit: None,
                roots,
                gate: existing.authorization.inner.gate.clone(),
                cancel: None,
                owner: self.owner.clone(),
                download_output: None,
                _reservation: Reservation::new(record.path.capacity() * 2 + 1024)?,
            }),
        };
        ensure!(
            record.explicit_file_epoch.is_none(),
            "Automatic relocation cannot renew an exact file grant"
        );
        let evidence = if content.is_some() {
            crate::domain::library_watch::RelinkEvidence::SameFileAndContent
        } else {
            crate::domain::library_watch::RelinkEvidence::SameOpenObject
        };
        Ok(PreparedAssetRelocation {
            record: record.clone(),
            identity: identity.clone(),
            digest: record.permission_sha256()?,
            expected_digest: existing.digest.clone(),
            previous: existing.authorization.clone(),
            authorization,
            _original: Some(original),
            _target: opened.try_clone()?,
            _reservation: reservation,
            evidence,
            content,
            old_locator: Some(existing.authorization.current_path()),
            expected_binding_revision: None,
        })
    }
    pub fn publish_asset_relocation(
        &self,
        prepared: PreparedAssetRelocation,
        committed: &LocalAssetAuthority,
    ) -> Result<FileAuthorization> {
        ensure!(
            &prepared.record == committed,
            "Committed asset relocation differs from reservation"
        );
        ensure!(!self.owner.is_closing(), "Library authority is closing");
        let mut state = self
            .inner
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let current = state
            .assets
            .get(&committed.asset_id)
            .context("Relocation asset disappeared during commit")?;
        ensure!(
            current.digest == prepared.expected_digest
                && Arc::ptr_eq(&current.authorization.inner, &prepared.previous.inner),
            "Relocation asset binding changed during commit"
        );
        if prepared._original.is_some() {
            current.authorization.check()?;
        } else {
            current.authorization.check_binding_gate()?;
        }
        for root in &committed.roots {
            ensure!(
                state
                    .roots
                    .get(&root.root_id)
                    .is_some_and(|active| active.epoch == root.grant_epoch
                        && active.authority.check().is_ok()),
                "Relocation root changed during commit"
            );
        }
        *prepared
            .authorization
            .inner
            .binding
            .write()
            .unwrap_or_else(|error| error.into_inner()) = committed.path.clone();
        state.assets.insert(
            committed.asset_id.clone(),
            InstalledAsset {
                digest: prepared.digest,
                identity: Some(prepared.identity),
                authorization: prepared.authorization.clone(),
            },
        );
        Ok(prepared.authorization)
    }
    /// Opens an explicit selection inside this profile's handle-retirement
    /// scope without adding it to the durable live asset registry.
    pub fn open_selection(&self, path: &Path) -> Result<AuthorizedFile> {
        validate_path(path)?;
        let path = std::fs::canonicalize(path)?;
        let track_id = crate::domain::music::local_track_id(&path);
        let asset_id = crate::domain::music::LocalAsset::new(track_id.clone(), path.clone(), 0).id;
        let selection = LocalAssetAuthority {
            asset_id,
            track_id,
            path,
            explicit_file_epoch: Some(1),
            roots: Vec::new(),
        };
        let temporary = Self {
            inner: Arc::new(RwLock::new(RegistryState::default())),
            owner: self.owner.clone(),
        };
        temporary.install(&selection, None)?.open()
    }

    pub fn prepare_replacement(
        &self,
        record: &LocalAssetAuthority,
        identity: Option<FileIdentity>,
    ) -> Result<PreparedAuthorityInstall> {
        let digest = record.permission_sha256()?;
        if let Some(identity) = &identity {
            identity.validate()?;
        }
        ensure!(!self.owner.is_closing(), "Library authority is closing");
        let state = self.inner.read().unwrap_or_else(|error| error.into_inner());
        ensure!(
            state.assets.contains_key(&record.asset_id) || state.assets.len() < 50_000,
            "Library authorization asset budget exceeded"
        );
        ensure!(
            record.explicit_file_epoch.is_some(),
            "Manual replacement requires an explicit file grant"
        );
        ensure!(
            state.explicit.contains_key(&record.asset_id) || state.explicit.len() < 50_000,
            "Library explicit grant budget exceeded"
        );
        let epoch = record.explicit_file_epoch.unwrap();
        ensure!(
            !state
                .explicit
                .get(&record.asset_id)
                .is_some_and(|current| epoch < current.0)
                && !state
                    .revoked_files
                    .get(&record.asset_id)
                    .is_some_and(|revoked| epoch <= *revoked),
            "Explicit library grant epoch is stale"
        );
        let mut roots = Vec::with_capacity(record.roots.len());
        for root in &record.roots {
            let current = state
                .roots
                .get(&root.root_id)
                .context("Replacement root intent is unavailable")?;
            let draft = current.authority.draft();
            ensure!(
                current.epoch == root.grant_epoch
                    && draft.anchor == root.anchor
                    && same_path(&draft.path, &root.path)
                    && draft.recursive == root.recursive
                    && draft.path_policy == root.path_policy,
                "Replacement root grant changed"
            );
            roots.push(current.authority.clone());
        }
        let mut history = Vec::new();
        for key in std::iter::once(format!("asset:{}", record.asset_id)).chain(
            record
                .roots
                .iter()
                .map(|root| format!("root:{}", root.root_id)),
        ) {
            if !state._history.contains_key(&key) && !history.iter().any(|(old, _)| old == &key) {
                let reservation = Reservation::new(key.len() * 4 + 512)?;
                history.push((key, reservation));
            }
        }
        ensure!(
            state._history.len() + history.len() <= 100_000,
            "Library grant history budget exceeded"
        );
        let reservation = Reservation::new(
            record.path.as_os_str().len() * 4
                + roots.capacity() * std::mem::size_of::<RootAuthority>() * 2
                + 1024,
        )?;
        let explicit = state
            .explicit
            .get(&record.asset_id)
            .filter(|grant| grant.0 == epoch)
            .map_or_else(Access::durable, |grant| grant.1.clone());
        let expected_previous = state
            .assets
            .get(&record.asset_id)
            .map(|old| (old.digest.clone(), old.identity.clone()));
        let authorization = FileAuthorization {
            inner: Arc::new(AuthorizationInner {
                path: record.path.clone(),
                binding: Arc::new(RwLock::new(record.path.clone())),
                live_open: Arc::new(Mutex::new(VecDeque::with_capacity(
                    MAX_LIVE_OPENS_PER_ASSET,
                ))),
                expected_identity: Arc::new(RwLock::new(identity.clone())),
                explicit: Some(explicit),
                roots,
                gate: Arc::new(AtomicBool::new(false)),
                cancel: None,
                owner: self.owner.clone(),
                download_output: None,
                _reservation: reservation,
            }),
        };
        Ok(PreparedAuthorityInstall {
            record: record.clone(),
            identity,
            digest,
            expected_previous,
            authorization,
            history,
            same_content: false,
            keep_old_reader: None,
        })
    }
    pub fn prepare_replacement_same_content(
        &self,
        record: &LocalAssetAuthority,
        identity: Option<FileIdentity>,
        verified_old_sha: &str,
        new_verified_sha: &str,
    ) -> Result<PreparedAuthorityInstall> {
        ensure!(
            verified_old_sha.len() == 64
                && verified_old_sha
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                && verified_old_sha.eq_ignore_ascii_case(new_verified_sha),
            "Manual same-content publication requires matching verified SHA256 evidence"
        );
        let mut prepared = self.prepare_replacement(record, identity)?;
        let state = self.inner.read().unwrap_or_else(|error| error.into_inner());
        if let Some(previous) = state.assets.get(&record.asset_id) {
            let live: Vec<_> = previous
                .authorization
                .inner
                .live_open
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .iter()
                .rev()
                .filter_map(Weak::upgrade)
                .collect();
            for permit in live {
                let original = AuthorizedFile {
                    permit: permit.clone(),
                    position: 0,
                };
                let Ok(snapshot) = original.read_snapshot() else {
                    continue;
                };
                let Some(authorization) = &permit.authorization else {
                    continue;
                };
                if authorization.inner.roots.iter().any(|root| {
                    root.authorize_location(&snapshot.path, &snapshot.directories)
                        .is_ok()
                }) {
                    prepared.keep_old_reader = Some(permit);
                    break;
                }
            }
        }
        prepared.same_content = true;
        Ok(prepared)
    }

    pub fn publish_replacement(
        &self,
        prepared: PreparedAuthorityInstall,
        committed: &LocalAssetAuthority,
    ) -> Result<FileAuthorization> {
        ensure!(
            !prepared.same_content,
            "Same-content replacement requires its dedicated publication API"
        );
        self.publish_replacement_inner(prepared, committed)
    }
    pub fn publish_replacement_same_content(
        &self,
        prepared: PreparedAuthorityInstall,
        committed: &LocalAssetAuthority,
    ) -> Result<FileAuthorization> {
        ensure!(
            prepared.same_content,
            "Same-content replacement has no verified reservation"
        );
        self.publish_replacement_inner(prepared, committed)
    }
    fn publish_replacement_inner(
        &self,
        prepared: PreparedAuthorityInstall,
        committed: &LocalAssetAuthority,
    ) -> Result<FileAuthorization> {
        ensure!(
            &prepared.record == committed,
            "Committed replacement does not match its reservation"
        );
        ensure!(!self.owner.is_closing(), "Library authority is closing");
        let mut state = self
            .inner
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let previous = state
            .assets
            .get(&committed.asset_id)
            .map(|old| (old.digest.clone(), old.identity.clone()));
        if let Some(existing) = state.assets.get(&committed.asset_id)
            && existing.digest == prepared.digest
            && existing.identity == prepared.identity
            && existing.authorization.check().is_ok()
        {
            return Ok(existing.authorization.clone());
        }
        ensure!(
            previous == prepared.expected_previous,
            "Replacement authority changed during commit"
        );
        for root in &committed.roots {
            ensure!(
                state
                    .roots
                    .get(&root.root_id)
                    .is_some_and(|grant| grant.epoch == root.grant_epoch),
                "Replacement root grant changed during commit"
            );
        }
        let epoch = committed
            .explicit_file_epoch
            .context("Committed replacement has no explicit grant")?;
        let explicit = prepared
            .authorization
            .inner
            .explicit
            .as_ref()
            .unwrap()
            .clone();
        if let Some((old_epoch, old)) = state
            .explicit
            .insert(committed.asset_id.clone(), (epoch, explicit))
            && old_epoch != epoch
        {
            old.alive.store(false, Ordering::Release);
            state
                .revoked_files
                .insert(committed.asset_id.clone(), old_epoch);
        }
        for (key, reservation) in prepared.history {
            state._history.entry(key).or_insert(reservation);
        }
        if let Some(old) = state.assets.insert(
            committed.asset_id.clone(),
            InstalledAsset {
                digest: prepared.digest,
                identity: prepared.identity,
                authorization: prepared.authorization.clone(),
            },
        ) && prepared.keep_old_reader.is_none()
        {
            old.authorization.inner.gate.store(false, Ordering::Release);
        }
        prepared
            .authorization
            .inner
            .gate
            .store(true, Ordering::Release);
        Ok(prepared.authorization)
    }

    pub fn install_root(&self, root: &LibraryRoot) -> Result<()> {
        root.validate()?;
        ensure!(!self.owner.is_closing(), "Library authority is closing");
        let mut state = self
            .inner
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(current) = state.roots.get(&root.root_id) {
            ensure!(
                root.grant_epoch >= current.epoch,
                "Stale library root grant epoch"
            );
            if root.grant_epoch == current.epoch {
                let draft = current.authority.draft();
                ensure!(
                    draft.anchor == root.anchor
                        && same_path(&draft.path, &root.path)
                        && draft.recursive == root.recursive
                        && draft.path_policy == root.path_policy,
                    "Library root intent changed without a new grant epoch"
                );
                return current.authority.check();
            }
        }
        ensure!(
            !state
                .revoked_roots
                .get(&root.root_id)
                .is_some_and(|epoch| root.grant_epoch <= *epoch),
            "Library root grant was revoked"
        );
        ensure!(
            state.roots.contains_key(&root.root_id) || state.roots.len() < 128,
            "Library root budget exceeded"
        );
        let authority = RootAuthority::from_record(root, Access::durable(), self.owner.clone())?;
        let key = format!("root:{}", root.root_id);
        let history = if state._history.contains_key(&key) {
            None
        } else {
            Some(Reservation::new(key.len() * 4 + 512)?)
        };
        if let Some(reservation) = history {
            ensure!(
                state._history.len() < 100_000,
                "Library grant history budget exceeded"
            );
            state._history.insert(key, reservation);
        }
        if let Some(old) = state.roots.insert(
            root.root_id.clone(),
            RootGrant {
                epoch: root.grant_epoch,
                authority,
            },
        ) {
            old.authority
                .inner
                .access
                .alive
                .store(false, Ordering::Release);
            old.authority
                .inner
                .health
                .store(RootHealth::Unavailable as u8, Ordering::Release);
            old.authority
                .inner
                .handles
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            state.revoked_roots.insert(root.root_id.clone(), old.epoch);
        }
        Ok(())
    }
    pub fn root_health(&self, root_id: &str, epoch: u64) -> RootHealth {
        let state = self.inner.read().unwrap_or_else(|error| error.into_inner());
        state
            .roots
            .get(root_id)
            .filter(|root| root.epoch == epoch && root.authority.check().is_ok())
            .map_or(RootHealth::Unknown, |root| root.authority.health())
    }
    pub fn effective_authority(&self, record: &LocalAssetAuthority) -> bool {
        if self.owner.is_closing() {
            return false;
        }
        let state = self.inner.read().unwrap_or_else(|error| error.into_inner());
        let explicit = record.explicit_file_epoch.is_some_and(|epoch| {
            state
                .explicit
                .get(&record.asset_id)
                .is_some_and(|(active, access)| *active == epoch && access.check().is_ok())
        });
        explicit
            || record.roots.iter().any(|root| {
                state.roots.get(&root.root_id).is_some_and(|active| {
                    active.epoch == root.grant_epoch
                        && active.authority.check().is_ok()
                        && active.authority.health() == RootHealth::Verified
                })
            })
    }
    pub fn accept_root_proof(&self, root: &LibraryRoot, proof: RootProof) -> Result<()> {
        root.validate()?;
        proof.access.check()?;
        ensure!(
            proof.draft.anchor == root.anchor
                && same_path(&proof.draft.path, &root.path)
                && proof.draft.recursive == root.recursive
                && proof.draft.path_policy == root.path_policy,
            "Root proof does not match persisted intent"
        );
        self.install_root(root)?;
        let state = self.inner.read().unwrap_or_else(|error| error.into_inner());
        let current = state
            .roots
            .get(&root.root_id)
            .filter(|current| current.epoch == root.grant_epoch)
            .context("Root proof grant epoch expired")?;
        current.authority.check()?;
        proof.handles.adopt(self.owner.clone())?;
        let mut cached = current
            .authority
            .inner
            .handles
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        current.authority.check()?;
        *cached = Some(proof.handles);
        current
            .authority
            .inner
            .health
            .store(RootHealth::Verified as u8, Ordering::Release);
        Ok(())
    }
    pub fn verify_root(&self, root: &LibraryRoot, context: &ScanContext) -> Result<RootHealth> {
        context.check()?;
        self.install_root(root)?;
        let authority = self
            .inner
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .roots
            .get(&root.root_id)
            .context("Library root intent missing")?
            .authority
            .clone();
        let result = authority.verify();
        context.check()?;
        let state = self.inner.read().unwrap_or_else(|error| error.into_inner());
        ensure!(
            state
                .roots
                .get(&root.root_id)
                .is_some_and(|active| active.epoch == root.grant_epoch
                    && Arc::ptr_eq(&active.authority.inner, &authority.inner)),
            "Library root verification epoch expired"
        );
        result?;
        Ok(authority.health())
    }
    pub(crate) fn open_root_for_validation(
        &self,
        path: &Path,
        recursive: bool,
        context: &ScanContext,
    ) -> Result<RootAuthority> {
        RootAuthority::open_scoped(path, recursive, context, self.owner.clone())
    }
    /// Watch preparation verifies an existing grant; it never installs an intent.
    pub(crate) fn watch_authority(&self, root: &LibraryRoot) -> Result<RootAuthority> {
        root.validate()?;
        let authority = {
            let state = self.inner.read().unwrap_or_else(|error| error.into_inner());
            let active = state
                .roots
                .get(&root.root_id)
                .filter(|active| active.epoch == root.grant_epoch)
                .context("Library watch root grant is not active")?;
            ensure!(
                active.authority.draft().anchor == root.anchor
                    && same_path(&active.authority.draft().path, &root.path)
                    && active.authority.draft().recursive == root.recursive,
                "Library watch root intent changed"
            );
            active.authority.clone()
        };
        authority.verify()?;
        authority.check()?;
        authority.proof()?;
        Ok(authority)
    }
    pub(crate) fn scan_authority(
        &self,
        root: &LibraryRoot,
        context: &ScanContext,
    ) -> Result<RootAuthority> {
        self.verify_root(root, context)?;
        let state = self.inner.read().unwrap_or_else(|error| error.into_inner());
        let durable = state
            .roots
            .get(&root.root_id)
            .filter(|active| active.epoch == root.grant_epoch)
            .context("Library root scan epoch expired")?;
        let handles = durable
            .authority
            .inner
            .handles
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        Ok(RootAuthority {
            inner: Arc::new(RootInner {
                draft: durable.authority.draft(),
                locator: durable.authority.inner.locator.clone(),
                move_policy: durable.authority.inner.move_policy.clone(),
                projection_storage: durable.authority.inner.projection_storage.clone(),
                access: Access {
                    alive: durable.authority.inner.access.alive.clone(),
                    scope_alive: Some(Arc::new(AtomicBool::new(true))),
                    cancel: Some(context.cancel.clone()),
                    deadline: Some(context.deadline),
                },
                handles: Mutex::new(handles),
                health: durable.authority.inner.health.clone(),
                owner: self.owner.clone(),
                _reservation: Reservation::new(root.path.as_os_str().len() * 4 + 1024)?,
            }),
        })
    }
    pub fn begin_shutdown(&self) {
        self.owner.begin_shutdown();
        let mut state = self
            .inner
            .write()
            .unwrap_or_else(|error| error.into_inner());
        for root in state.roots.values() {
            root.authority
                .inner
                .access
                .alive
                .store(false, Ordering::Release);
            root.authority
                .inner
                .health
                .store(RootHealth::Unavailable as u8, Ordering::Release);
            root.authority
                .inner
                .handles
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
        }
        for (_, access) in state.explicit.values() {
            access.alive.store(false, Ordering::Release);
        }
        for asset in state.assets.values() {
            asset
                .authorization
                .inner
                .gate
                .store(false, Ordering::Release);
        }
        let assets = std::mem::take(&mut state.assets);
        drop(state);
        // Native download readers retire through their reserved owner tickets.
        drop(assets);
    }
    pub fn retirement_pending(&self) -> usize {
        self.owner.pending()
    }
    pub fn retirement_barrier(&self) -> RetirementBarrier {
        RetirementBarrier {
            scope: self.owner.clone(),
        }
    }
    pub(crate) fn prepare_retirement(&self) -> Result<()> {
        retirement_service()?;
        Ok(())
    }
    /// Installs persisted intent only. Physical roots are checked when opened.
    pub fn install(
        &self,
        record: &LocalAssetAuthority,
        expected_identity: Option<FileIdentity>,
    ) -> Result<FileAuthorization> {
        validate_path(&record.path)?;
        ensure!(!self.owner.is_closing(), "Library authority is closing");
        if let Some(identity) = &expected_identity {
            identity.validate()?;
        }
        ensure!(
            record.roots.len() <= 128,
            "Library grant count exceeds limit"
        );
        let digest = record.permission_sha256()?;
        let mut state = self
            .inner
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let expected_identity = expected_identity.or_else(|| {
            state
                .assets
                .get(&record.asset_id)
                .filter(|old| same_path(&old.authorization.current_path(), &record.path))
                .and_then(|old| old.identity.clone())
        });
        ensure!(
            state.assets.contains_key(&record.asset_id) || state.assets.len() < 50_000,
            "Library authorization asset budget exceeded"
        );
        let new_roots = record
            .roots
            .iter()
            .filter(|root| !state.roots.contains_key(&root.root_id))
            .count();
        ensure!(
            state.roots.len() + new_roots <= 128,
            "Library authorization root budget exceeded"
        );
        ensure!(
            record.explicit_file_epoch.is_none()
                || state.explicit.contains_key(&record.asset_id)
                || state.explicit.len() < 50_000,
            "Library explicit grant budget exceeded"
        );
        for root in &record.roots {
            if let Some(current) = state.roots.get(&root.root_id) {
                ensure!(
                    root.grant_epoch >= current.epoch,
                    "Stale library root grant epoch"
                );
                if root.grant_epoch == current.epoch {
                    let draft = current.authority.draft();
                    ensure!(
                        same_path(&draft.path, &root.path)
                            && draft.anchor == root.anchor
                            && draft.recursive == root.recursive
                            && draft.path_policy == root.path_policy,
                        "Library root intent changed without a new grant epoch"
                    );
                }
            }
            ensure!(
                !state
                    .revoked_roots
                    .get(&root.root_id)
                    .is_some_and(|epoch| root.grant_epoch <= *epoch),
                "Library root grant was revoked"
            );
        }
        if let Some(epoch) = record.explicit_file_epoch {
            ensure!(
                !state
                    .explicit
                    .get(&record.asset_id)
                    .is_some_and(|current| epoch < current.0),
                "Stale explicit library grant epoch"
            );
            ensure!(
                !state
                    .revoked_files
                    .get(&record.asset_id)
                    .is_some_and(|revoked| epoch <= *revoked),
                "Explicit library grant was revoked"
            );
        }
        if let Some(existing) = state.assets.get(&record.asset_id)
            && existing.digest == digest
            && existing.identity == expected_identity
            && existing.authorization.check().is_ok()
        {
            return Ok(existing.authorization.clone());
        }
        let mut history = Vec::new();
        let keys = std::iter::once(format!("asset:{}", record.asset_id)).chain(
            record
                .roots
                .iter()
                .map(|root| format!("root:{}", root.root_id)),
        );
        for key in keys {
            if !state._history.contains_key(&key) && !history.iter().any(|(old, _)| old == &key) {
                let reservation = Reservation::new(key.len() * 4 + 512)?;
                history.push((key, reservation));
            }
        }
        ensure!(
            state._history.len() + history.len() <= 100_000,
            "Library grant history budget exceeded"
        );
        let reservation = Reservation::new(
            record.path.as_os_str().len() * 4
                + record.roots.len() * std::mem::size_of::<RootAuthority>() * 2
                + 1024,
        )?;
        let mut pending_roots = Vec::new();
        for root in &record.roots {
            if !state
                .roots
                .get(&root.root_id)
                .is_some_and(|grant| grant.epoch == root.grant_epoch)
            {
                pending_roots.push((
                    root,
                    RootAuthority::from_record(root, Access::durable(), self.owner.clone())?,
                ));
            }
        }
        for (key, reservation) in history {
            state._history.insert(key, reservation);
        }
        let explicit = if let Some(epoch) = record.explicit_file_epoch {
            let grant = state
                .explicit
                .entry(record.asset_id.clone())
                .or_insert_with(|| (epoch, Access::durable()));
            if grant.0 != epoch {
                let previous_epoch = grant.0;
                grant.1.alive.store(false, Ordering::Release);
                *grant = (epoch, Access::durable());
                state
                    .revoked_files
                    .insert(record.asset_id.clone(), previous_epoch);
            }
            Some(state.explicit.get(&record.asset_id).unwrap().1.clone())
        } else {
            if let Some((_, grant)) = state.explicit.remove(&record.asset_id) {
                grant.alive.store(false, Ordering::Release);
            }
            None
        };
        for (root, authority) in pending_roots {
            if let Some(previous) = state.roots.insert(
                root.root_id.clone(),
                RootGrant {
                    epoch: root.grant_epoch,
                    authority,
                },
            ) {
                previous
                    .authority
                    .inner
                    .access
                    .alive
                    .store(false, Ordering::Release);
                previous
                    .authority
                    .inner
                    .health
                    .store(RootHealth::Unavailable as u8, Ordering::Release);
                previous
                    .authority
                    .inner
                    .handles
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                state
                    .revoked_roots
                    .insert(root.root_id.clone(), previous.epoch);
            }
        }
        let mut roots = Vec::with_capacity(record.roots.len());
        for root in &record.roots {
            roots.push(state.roots.get(&root.root_id).unwrap().authority.clone());
        }
        let gate = state
            .assets
            .get(&record.asset_id)
            .filter(|old| same_path(&old.authorization.current_path(), &record.path))
            .map_or_else(
                || Arc::new(AtomicBool::new(true)),
                |old| old.authorization.inner.gate.clone(),
            );
        let authorization = FileAuthorization {
            inner: Arc::new(AuthorizationInner {
                path: record.path.clone(),
                binding: state
                    .assets
                    .get(&record.asset_id)
                    .filter(|old| same_path(&old.authorization.current_path(), &record.path))
                    .map_or_else(
                        || Arc::new(RwLock::new(record.path.clone())),
                        |old| old.authorization.inner.binding.clone(),
                    ),
                live_open: state
                    .assets
                    .get(&record.asset_id)
                    .filter(|old| same_path(&old.authorization.current_path(), &record.path))
                    .map_or_else(
                        || {
                            Arc::new(Mutex::new(VecDeque::with_capacity(
                                MAX_LIVE_OPENS_PER_ASSET,
                            )))
                        },
                        |old| old.authorization.inner.live_open.clone(),
                    ),
                expected_identity: state
                    .assets
                    .get(&record.asset_id)
                    .filter(|old| same_path(&old.authorization.current_path(), &record.path))
                    .map_or_else(
                        || Arc::new(RwLock::new(expected_identity.clone())),
                        |old| old.authorization.inner.expected_identity.clone(),
                    ),
                explicit,
                roots,
                gate,
                cancel: None,
                owner: self.owner.clone(),
                download_output: None,
                _reservation: reservation,
            }),
        };
        *authorization
            .inner
            .expected_identity
            .write()
            .unwrap_or_else(|error| error.into_inner()) = expected_identity.clone();
        if let Some(previous) = state.assets.insert(
            record.asset_id.clone(),
            InstalledAsset {
                digest,
                identity: expected_identity,
                authorization: authorization.clone(),
            },
        ) && !same_path(
            &previous.authorization.current_path(),
            &authorization.current_path(),
        ) {
            previous
                .authorization
                .inner
                .gate
                .store(false, Ordering::Release);
        }
        Ok(authorization)
    }
    pub fn revoke_root(&self, root_id: &str) {
        let mut state = self
            .inner
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(root) = state.roots.remove(root_id) {
            root.authority
                .inner
                .access
                .alive
                .store(false, Ordering::Release);
            root.authority
                .inner
                .health
                .store(RootHealth::Unavailable as u8, Ordering::Release);
            root.authority
                .inner
                .handles
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            state.revoked_roots.insert(root_id.to_owned(), root.epoch);
        }
    }
    pub fn revoke_file(&self, asset_id: &str) {
        let mut state = self
            .inner
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if let Some((epoch, explicit)) = state.explicit.remove(asset_id) {
            explicit.alive.store(false, Ordering::Release);
            state.revoked_files.insert(asset_id.to_owned(), epoch);
        }
    }
}
impl FileAuthorization {
    pub fn path(&self) -> &Path {
        &self.inner.path
    }
    pub fn current_path(&self) -> PathBuf {
        self.inner
            .binding
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    fn may_cover_location(&self, path: &Path) -> bool {
        if let Some(download) = &self.inner.download_output {
            return download.verify().is_ok() && same_path(path, &self.inner.path);
        }
        self.inner
            .explicit
            .as_ref()
            .is_some_and(|access| access.check().is_ok() && same_path(path, &self.inner.path))
            || self.inner.roots.iter().any(|root| {
                root.physical_path().is_ok_and(|actual| {
                    within(path, &actual)
                        && (root.inner.draft.recursive
                            || path
                                .parent()
                                .is_some_and(|parent| same_path(parent, &actual)))
                })
            })
    }
    fn authorize_location(&self, actual: &Path, handles: &[File]) -> Result<()> {
        self.check_intent()?;
        if self.check_download_location(actual)? {
            return Ok(());
        }
        if self
            .inner
            .roots
            .iter()
            .any(|root| root.authorize_location(actual, handles).is_ok())
        {
            return Ok(());
        }
        ensure!(
            self.inner
                .explicit
                .as_ref()
                .is_some_and(|grant| grant.check().is_ok())
                && same_path(actual, &self.inner.path),
            "Opened file has no applicable physical library grant"
        );
        Ok(())
    }
    pub fn with_cancel(&self, cancel: Arc<AtomicBool>) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(AuthorizationInner {
                path: self.inner.path.clone(),
                binding: self.inner.binding.clone(),
                live_open: self.inner.live_open.clone(),
                expected_identity: self.inner.expected_identity.clone(),
                explicit: self.inner.explicit.clone(),
                roots: self.inner.roots.clone(),
                gate: self.inner.gate.clone(),
                cancel: Some(cancel),
                owner: self.inner.owner.clone(),
                download_output: self.inner.download_output.clone(),
                _reservation: Reservation::new(
                    self.inner.path.capacity() * 2
                        + self.inner.roots.len() * std::mem::size_of::<RootAuthority>() * 2
                        + 1024,
                )?,
            }),
        })
    }
    pub fn check(&self) -> Result<()> {
        self.check_intent()?;
        let live: Vec<_> = self
            .inner
            .live_open
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        if live.is_empty() {
            return self.check_cached_namespace();
        }
        ensure!(
            live.into_iter().any(|permit| AuthorizedFile {
                permit,
                position: 0
            }
            .check()
            .is_ok()),
            "Library asset has no current physical opened proof"
        );
        Ok(())
    }
    /// Rechecks a consumed selection against its continuously retained actual
    /// file. This does not reopen an immutable reader beside a writable owner.
    /// The original grant still supplies revocation and namespace authority.
    #[cfg(windows)]
    pub(crate) fn check_retained_file(&self, file: &File) -> Result<()> {
        self.check_intent()?;
        let expected = self
            .inner
            .expected_identity
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .context("Retained selection has no original actual file identity")?;
        ensure!(
            platform::identity(file)? == expected && platform::link_count(file)? == 1,
            "Retained file differs from the original single-link selection"
        );
        let actual = platform::current_path(file)?;
        let parent = actual
            .parent()
            .context("Retained selected file has no parent")?;
        let _reservation = Reservation::new(
            MAX_COMPONENTS * std::mem::size_of::<File>() * 2 + 4096 + actual.capacity() * 2,
        )?;
        let (actual_parent, _, directories) = platform::open_directory_pinned(parent)?;
        ensure!(
            same_path(&actual_parent, parent),
            "Retained file namespace changed while pinning"
        );
        for directory in &directories {
            platform::validate_directory(directory)?;
        }
        self.authorize_location(&actual, &directories)?;
        ensure!(
            platform::identity(file)? == expected
                && platform::link_count(file)? == 1
                && same_path(&platform::current_path(file)?, &actual),
            "Retained selected object or namespace changed during validation"
        );
        self.check_intent()
    }
    fn check_cached_namespace(&self) -> Result<()> {
        if let Some(download) = &self.inner.download_output {
            download.verify()?;
            return self.check_binding_gate();
        }
        let Some(expected) = self
            .inner
            .expected_identity
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
        else {
            return Ok(());
        };
        let current = self.current_path();
        for root in &self.inner.roots {
            let Ok(physical) = root.physical_path() else {
                continue;
            };
            let declared = root.current_path();
            let candidate = if !same_path(&physical, &declared) {
                relative_suffix(&current, &declared).map(|relative| physical.join(relative))
            } else {
                Some(current.clone())
            };
            if let Some(candidate) = candidate
                && let Ok((_, actual, identity, directories)) = root.open_beneath(&candidate, false)
                && identity == expected
                && root.authorize_location(&actual, &directories).is_ok()
            {
                return self.check_binding_gate();
            }
            #[cfg(windows)]
            {
                let proof = root
                    .inner
                    .handles
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone();
                let Some(proof) = proof else {
                    continue;
                };
                let Some(hint) = proof.last() else {
                    continue;
                };
                let Ok(metadata_file) = platform::lookup_metadata_by_identity(hint, &expected)
                else {
                    continue;
                };
                let Ok(actual) = platform::current_path(&metadata_file) else {
                    continue;
                };
                if !within(&actual, &physical) {
                    continue;
                }
                if let Ok((_, namespace, identity, directories)) = root.open_beneath(&actual, false)
                    && identity == expected
                    && root.authorize_location(&namespace, &directories).is_ok()
                {
                    return self.check_binding_gate();
                }
            }
        }
        if self
            .inner
            .explicit
            .as_ref()
            .is_some_and(|grant| grant.check().is_ok())
            && same_path(&current, &self.inner.path)
            && let Ok((_, actual, identity, _)) = platform::open_file(&current)
            && identity == expected
            && same_path(&actual, &self.inner.path)
        {
            return self.check_binding_gate();
        }
        anyhow::bail!("Cached library object has no current physical namespace grant")
    }
    fn check_intent(&self) -> Result<()> {
        self.check_binding_gate()?;
        ensure!(
            self.inner
                .explicit
                .as_ref()
                .is_some_and(|grant| grant.check().is_ok())
                || self
                    .inner
                    .download_output
                    .as_ref()
                    .is_some_and(|download| download.verify().is_ok())
                || self
                    .inner
                    .roots
                    .iter()
                    .any(|root| root.physical_path().is_ok()),
            "Library file has no active grant"
        );
        Ok(())
    }
    fn check_binding_gate(&self) -> Result<()> {
        ensure!(
            !self.inner.owner.is_closing(),
            "Library authority is closing"
        );
        ensure!(
            self.inner.gate.load(Ordering::Acquire),
            "Library asset permission changed"
        );
        ensure!(
            !self
                .inner
                .cancel
                .as_ref()
                .is_some_and(|cancel| cancel.load(Ordering::Acquire)),
            "Library read cancelled"
        );
        Ok(())
    }
    pub fn open(&self) -> Result<AuthorizedFile> {
        self.check_intent()?;
        if let Some(opened) = self.open_download_output()? {
            return Ok(opened);
        }
        let current_path = self.current_path();
        let mut grants = Vec::new();
        let mut directories = Vec::new();
        let mut selected: Option<AuthorizedFile> = None;
        let mut last_error = None;
        for root in &self.inner.roots {
            if root.check().is_err() {
                continue;
            }
            let candidate = (|| -> Result<PathBuf> {
                let committed_root = root.current_path();
                let physical_root = root.physical_path()?;
                if !same_path(&physical_root, &committed_root) {
                    let relative = relative_suffix(&current_path, &committed_root)
                        .context("File locator is outside the relocated root")?;
                    Ok(physical_root.join(relative))
                } else {
                    Ok(current_path.clone())
                }
            })();
            let candidate = match candidate {
                Ok(path) => path,
                Err(error) => {
                    last_error = Some(error);
                    continue;
                }
            };
            match root.open_file(&candidate) {
                Ok(opened) => {
                    if let Some(first) = &selected {
                        ensure!(
                            first.identity() == opened.identity(),
                            "Library grants verified different physical files"
                        );
                    }
                    grants.push(root.inner.access.clone());
                    directories.extend(opened.permit._directories.iter().cloned());
                    if selected.is_none() {
                        selected = Some(opened);
                    }
                }
                Err(error) => {
                    last_error = Some(error);
                }
            }
        }
        if let Some(explicit) = &self.inner.explicit
            && explicit.check().is_ok()
            && same_path(&current_path, &self.inner.path)
        {
            let ticket = DirectoryTicket::new(self.inner.owner.clone())?;
            match platform::open_file(&current_path) {
                Ok((file, path, identity, handles)) => {
                    grants.push(explicit.clone());
                    directories.push(ticket.capture(handles));
                    if let Some(first) = &selected {
                        ensure!(
                            first.identity() == &identity,
                            "Library grants verified different physical files"
                        );
                    }
                    if selected.is_none() {
                        selected = Some(AuthorizedFile::new(
                            file,
                            path,
                            identity,
                            Vec::new(),
                            Vec::new(),
                        )?);
                    }
                }
                Err(error) => {
                    last_error = Some(error);
                }
            }
        }
        let selected = selected.ok_or_else(|| {
            last_error.unwrap_or_else(|| anyhow::anyhow!("Library file has no verified grant"))
        })?;
        {
            let mut expected = self
                .inner
                .expected_identity
                .write()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(identity) = &*expected {
                ensure!(
                    selected.identity() == identity,
                    "NeedsReapproval: library file identity changed"
                );
            } else {
                *expected = Some(selected.identity().clone());
            }
        }
        let file = selected.permit.file.try_clone()?;
        let opened = AuthorizedFile {
            permit: Arc::new(OpenPermit {
                stamp: RepresentationStamp::read(&file)?,
                failed: AtomicBool::new(false),
                file,
                path: selected.path().to_path_buf(),
                identity: selected.identity().clone(),
                grants,
                _directories: directories,
                gate: Some(self.inner.gate.clone()),
                authorization: Some(self.clone()),
                _audio: None,
                _reservation: Reservation::new(selected.path().as_os_str().len() * 4 + 32 * 1024)?,
            }),
            position: 0,
        };
        opened.check()?;
        let mut live = self
            .inner
            .live_open
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        live.retain(|opened| opened.strong_count() > 0);
        if live.len() == MAX_LIVE_OPENS_PER_ASSET {
            live.pop_front();
        }
        live.push_back(Arc::downgrade(&opened.permit));
        drop(live);
        Ok(opened)
    }
    /// A file grant derives access only to these exact same-stem siblings.
    pub fn open_sidecar(&self, extension: &str) -> Result<AuthorizedFile> {
        ensure!(
            matches!(extension, "lrc" | "krc" | "jpg" | "png"),
            "Unsupported library sidecar extension"
        );
        let audio = self.open().map_err(|error| {
            crate::domain::library::LibraryError::new(
                crate::domain::library::LibraryErrorKind::Unavailable,
                format!("Cannot verify audio for sidecar access: {error}"),
            )
        })?;
        let path = audio.current_path()?.with_extension(extension);
        let reservation = Reservation::new(
            path.capacity() * 2
                + self.inner.roots.len() * std::mem::size_of::<RootAuthority>() * 2
                + 1024,
        )?;
        let derived = Self {
            inner: Arc::new(AuthorizationInner {
                binding: Arc::new(RwLock::new(path.clone())),
                live_open: Arc::new(Mutex::new(VecDeque::with_capacity(
                    MAX_LIVE_OPENS_PER_ASSET,
                ))),
                path,
                expected_identity: Arc::new(RwLock::new(None)),
                explicit: self.inner.explicit.clone(),
                roots: self.inner.roots.clone(),
                gate: self.inner.gate.clone(),
                cancel: self.inner.cancel.clone(),
                owner: self.inner.owner.clone(),
                download_output: None,
                _reservation: reservation,
            }),
        };
        let mut sidecar = match derived.open() {
            Ok(sidecar) => sidecar,
            Err(error) => {
                audio.check().map_err(|error| {
                    crate::domain::library::LibraryError::new(
                        crate::domain::library::LibraryErrorKind::Unavailable,
                        format!("Audio grant changed while opening sidecar: {error}"),
                    )
                })?;
                return Err(error);
            }
        };
        ensure!(
            sidecar.path().parent() == audio.path().parent(),
            "Sidecar is outside its verified audio directory"
        );
        audio.check()?;
        derived
            .inner
            .live_open
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        Arc::get_mut(&mut sidecar.permit)
            .context("Sidecar permit is not unique")?
            ._audio = Some(audio);
        derived
            .inner
            .live_open
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_back(Arc::downgrade(&sidecar.permit));
        Ok(sidecar)
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    };
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
        FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
    };
    use windows_sys::Win32::Foundation::{
        GENERIC_READ, INVALID_HANDLE_VALUE, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError,
        UNICODE_STRING,
    };
    use windows_sys::Win32::Storage::FileSystem::*;
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
    fn share(directory: bool, pinned: bool) -> u32 {
        FILE_SHARE_READ
            | (if directory { FILE_SHARE_WRITE } else { 0 })
            | (if pinned { 0 } else { FILE_SHARE_DELETE })
    }
    fn open(path: &Path, directory: bool, pinned: bool) -> Result<File> {
        validate_path(path)?;
        let ordinary = normalize_local_path(path);
        let value = ordinary
            .to_str()
            .context("Library Windows path is not UTF-8")?;
        let extended = if let Some(unc) = value.strip_prefix("\\\\") {
            format!("\\\\?\\UNC\\{unc}")
        } else {
            format!("\\\\?\\{value}")
        };
        let text: Vec<u16> = std::ffi::OsStr::new(&extended)
            .encode_wide()
            .chain(Some(0))
            .collect();
        let access = if directory {
            FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY
        } else {
            GENERIC_READ
        };
        let handle = unsafe {
            CreateFileW(
                text.as_ptr(),
                access,
                share(directory, pinned),
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error()).context("Cannot open verified library path");
        }
        let file = unsafe { File::from_raw_handle(handle) };
        validate_file(&file, directory)?;
        if pinned {
            Ok(file)
        } else {
            reopen_by_id(&file, directory)
        }
    }
    fn open_relative(
        parent: &File,
        name: &std::ffi::OsStr,
        directory: bool,
        pinned: bool,
        expected: Option<&Path>,
    ) -> Result<File> {
        #[cfg(test)]
        RELATIVE_OPEN_COUNT.fetch_add(1, Ordering::Relaxed);
        if let Some(expected) = expected {
            ensure!(
                expected
                    .parent()
                    .is_some_and(|ancestor| current_path(parent)
                        .is_ok_and(|actual| same_path(ancestor, &actual))),
                "Selected alias parent moved before relative opening"
            );
        }
        let mut text: Vec<u16> = name.encode_wide().collect();
        ensure!(
            !text.is_empty() && text.len() <= u16::MAX as usize / 2,
            "Invalid relative library filename"
        );
        let name = UNICODE_STRING {
            Length: (text.len() * 2) as u16,
            MaximumLength: (text.len() * 2) as u16,
            Buffer: text.as_mut_ptr(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: parent.as_raw_handle(),
            ObjectName: &name,
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: std::ptr::null(),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut handle = std::ptr::null_mut();
        let mut status_block = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                (if directory {
                    FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY
                } else {
                    GENERIC_READ
                }) | SYNCHRONIZE,
                &attributes,
                &mut status_block,
                std::ptr::null(),
                0,
                share(directory, pinned),
                FILE_OPEN,
                FILE_OPEN_REPARSE_POINT
                    | FILE_SYNCHRONOUS_IO_NONALERT
                    | if directory {
                        FILE_DIRECTORY_FILE
                    } else {
                        FILE_NON_DIRECTORY_FILE
                    },
                std::ptr::null(),
                0,
            )
        };
        if status < 0 {
            return Err(io::Error::from_raw_os_error(
                unsafe { RtlNtStatusToDosError(status) } as i32,
            ))
            .context("Cannot open relative verified library path");
        }
        ensure!(
            !handle.is_null() && handle != INVALID_HANDLE_VALUE,
            "Invalid native library handle"
        );
        let file = unsafe { File::from_raw_handle(handle) };
        validate_file(&file, directory)?;
        if let Some(expected) = expected {
            ensure!(
                same_path(&current_path(&file)?, expected)
                    || (!directory && link_count(&file)? > 1),
                "Selected alias changed during relative opening"
            );
        }
        if pinned {
            Ok(file)
        } else {
            reopen_by_id(&file, directory)
        }
    }
    fn reopen_by_id(original: &File, directory: bool) -> Result<File> {
        reopen_by_id_mode(original, directory, false)
    }
    fn reopen_by_id_mode(original: &File, directory: bool, pinned: bool) -> Result<File> {
        // NTFS pathname handles to children block parent renaming. Retain the
        // same physical object by ID after the relative namespace proof.
        let mut info = FILE_ID_INFO::default();
        let success = unsafe {
            GetFileInformationByHandleEx(
                original.as_raw_handle(),
                FileIdInfo,
                &mut info as *mut _ as *mut _,
                std::mem::size_of_val(&info) as u32,
            )
        };
        ensure!(
            success != 0,
            "Cannot identify original relative library handle: {}",
            io::Error::last_os_error()
        );
        let descriptor = FILE_ID_DESCRIPTOR {
            dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
            Type: ExtendedFileIdType,
            Anonymous: FILE_ID_DESCRIPTOR_0 {
                ExtendedFileId: info.FileId,
            },
        };
        let handle = unsafe {
            OpenFileById(
                original.as_raw_handle(),
                &descriptor,
                if directory {
                    FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY
                } else {
                    GENERIC_READ
                },
                share(directory, pinned),
                std::ptr::null(),
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error()).context(
                "UnsupportedCapability: path-independent library object handles are unavailable",
            );
        }
        let file = unsafe { File::from_raw_handle(handle) };
        validate_file(&file, directory)?;
        ensure!(
            identity(&file)? == identity(original)?
                && (same_path(&current_path(&file)?, &current_path(original)?)
                    || (!directory && link_count(original)? > 1)),
            "Path-independent library handle differs from its original object"
        );
        Ok(file)
    }
    pub(super) fn lookup_metadata_by_identity(
        hint: &File,
        expected: &FileIdentity,
    ) -> Result<File> {
        ensure!(
            expected.kind == "windows_file_id"
                && expected.file_id.len() == 32
                && identity(hint)?.volume == expected.volume,
            "Cached object is not on its original approved volume"
        );
        let mut bytes = [0u8; 16];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&expected.file_id[index * 2..index * 2 + 2], 16)?;
        }
        let descriptor = FILE_ID_DESCRIPTOR {
            dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
            Type: ExtendedFileIdType,
            Anonymous: FILE_ID_DESCRIPTOR_0 {
                ExtendedFileId: FILE_ID_128 { Identifier: bytes },
            },
        };
        let handle = unsafe {
            OpenFileById(
                hint.as_raw_handle(),
                &descriptor,
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error()).context("Cached object physical lookup failed");
        }
        let file = unsafe { File::from_raw_handle(handle) };
        validate_file(&file, false)?;
        ensure!(
            identity(&file)? == *expected,
            "Cached object actual identity changed"
        );
        Ok(file)
    }
    pub(super) fn validate_directory(file: &File) -> Result<()> {
        validate_file(file, true)
    }
    pub(super) fn open_file_from_parent(
        existing: &[File],
        path: &Path,
    ) -> Result<(File, PathBuf, FileIdentity, Vec<File>)> {
        let mut handles = Vec::with_capacity(existing.len());
        for directory in existing {
            validate_directory(directory)?;
            handles.push(directory.try_clone()?);
        }
        let parent = handles.last().context("Cached physical parent missing")?;
        let file = open_relative(
            parent,
            path.file_name().context("Filename missing")?,
            false,
            true,
            Some(path),
        )?;
        let actual = current_path(&file)?;
        let actual = if !same_path(&actual, path) && link_count(&file)? > 1 {
            path.to_path_buf()
        } else {
            actual
        };
        ensure!(
            same_path(&actual, path),
            "Cached-parent namespace moved during opening"
        );
        let identity = identity(&file)?;
        Ok((file, actual, identity, handles))
    }
    pub(super) fn open_file_beneath(
        root: &File,
        root_path: &Path,
        path: &Path,
        pinned: bool,
    ) -> Result<(File, PathBuf, FileIdentity, Vec<File>)> {
        validate_path(path)?;
        let relative = relative_suffix(path, root_path).context("File is outside physical root")?;
        let mut components = relative.components().peekable();
        validate_directory(root)?;
        // The actual root object is retained by its existing verified ID handle.
        // The named leaf and nested-directory pins bound namespace changes;
        // root relocation is checked again against the actual root after I/O.
        let mut handles = vec![root.try_clone()?];
        let mut current = root_path.to_path_buf();
        while let Some(component) = components.next() {
            let std::path::Component::Normal(name) = component else {
                anyhow::bail!("Unsafe root-relative file component")
            };
            current.push(name);
            ensure!(
                handles.len() <= MAX_COMPONENTS,
                "Root-relative directory budget exceeded"
            );
            if components.peek().is_none() {
                let file =
                    open_relative(handles.last().unwrap(), name, false, pinned, Some(&current))?;
                let actual = current_path(&file)?;
                let actual = if !same_path(&actual, &current) && link_count(&file)? > 1 {
                    current.clone()
                } else {
                    actual
                };
                ensure!(
                    same_path(&actual, &current),
                    "Root-relative file moved during opening"
                );
                let identity = identity(&file)?;
                return Ok((file, actual, identity, handles));
            }
            handles.push(open_relative(
                handles.last().unwrap(),
                name,
                true,
                pinned,
                Some(&current),
            )?);
        }
        anyhow::bail!("Root-relative filename missing")
    }
    fn validate_file(file: &File, directory: bool) -> Result<()> {
        let metadata = file.metadata()?;
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "Nested reparse point is not authorized"
        );
        ensure!(
            metadata.is_dir() == directory && (directory || metadata.is_file()),
            "Library path has the wrong file kind"
        );
        if directory {
            let mut sensitive = FILE_CASE_SENSITIVE_INFO::default();
            let success = unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileCaseSensitiveInfo,
                    &mut sensitive as *mut _ as *mut _,
                    std::mem::size_of_val(&sensitive) as u32,
                )
            };
            ensure!(
                success != 0,
                "UnsupportedCapability: cannot verify directory case policy: {}",
                io::Error::last_os_error()
            );
            ensure!(
                sensitive.Flags & 1 == 0,
                "UnsupportedCapability: case-sensitive library directory"
            );
        }
        Ok(())
    }
    pub(super) fn identity(file: &File) -> Result<FileIdentity> {
        let mut info = FILE_ID_INFO::default();
        let success = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                &mut info as *mut _ as *mut _,
                std::mem::size_of_val(&info) as u32,
            )
        };
        ensure!(
            success != 0,
            "Cannot read library file identity: {}",
            io::Error::last_os_error()
        );
        Ok(FileIdentity {
            kind: "windows_file_id".into(),
            volume: format!("{:016x}", info.VolumeSerialNumber),
            file_id: info
                .FileId
                .Identifier
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        })
    }
    pub(super) fn link_count(file: &File) -> Result<u64> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        let success = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
        ensure!(
            success != 0,
            "Cannot query actual library link count: {}",
            io::Error::last_os_error()
        );
        Ok(info.nNumberOfLinks as u64)
    }
    pub(super) fn change_stamp(file: &File) -> Result<FileChangeStamp> {
        let mut info = FILE_BASIC_INFO::default();
        let success = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileBasicInfo,
                &mut info as *mut _ as *mut _,
                std::mem::size_of_val(&info) as u32,
            )
        };
        ensure!(
            success != 0,
            "Cannot query actual file change time: {}",
            io::Error::last_os_error()
        );
        Ok(FileChangeStamp::Windows {
            change_time_100ns: info.ChangeTime,
        })
    }
    pub(super) fn current_path(file: &File) -> Result<PathBuf> {
        let mut buffer = vec![0u16; MAX_PATH_BYTES + 1];
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        } as usize;
        ensure!(
            length > 0 && length < buffer.len(),
            "Cannot obtain bounded final library path: {}",
            io::Error::last_os_error()
        );
        let path = PathBuf::from(
            String::from_utf16(&buffer[..length]).context("Library final path is not UTF-8")?,
        );
        let path = normalize_local_path(&path);
        validate_path(&path)?;
        Ok(path)
    }
    fn directories(path: &Path, pinned: bool) -> Result<Vec<File>> {
        validate_path(path)?;
        let mut handles = Vec::new();
        let mut current = PathBuf::new();
        for component in path.components() {
            ensure!(
                handles.len() < MAX_COMPONENTS,
                "Library path component budget exceeded"
            );
            current.push(component.as_os_str());
            if let std::path::Component::Normal(name) = component {
                let parent = handles
                    .last()
                    .context("Library directory has no physical parent")?;
                let file = open_relative(parent, name, true, pinned, Some(&current))?;
                ensure!(
                    same_path(&current_path(&file)?, &current),
                    "Library ancestor moved while opening"
                );
                handles.push(file);
            } else if current.has_root() && current.is_absolute() && handles.is_empty() {
                handles.push(open(&current, true, pinned)?);
            }
        }
        Ok(handles)
    }
    pub(super) fn open_root(path: &Path) -> Result<(PathBuf, FileIdentity, Vec<File>)> {
        // An explicitly chosen alias may resolve once; the persisted grant anchors
        // the actual target. Subsequent component opens do not follow reparse points.
        let target =
            std::fs::canonicalize(path).context("Cannot resolve selected library directory")?;
        let target = normalize_local_path(&target);
        open_directory(&target)
    }
    pub(super) fn open_directory(path: &Path) -> Result<(PathBuf, FileIdentity, Vec<File>)> {
        open_directory_mode(path, false)
    }
    pub(super) fn open_directory_pinned(path: &Path) -> Result<(PathBuf, FileIdentity, Vec<File>)> {
        open_directory_mode(path, true)
    }
    fn open_directory_mode(
        path: &Path,
        pinned: bool,
    ) -> Result<(PathBuf, FileIdentity, Vec<File>)> {
        validate_path(path)?;
        let target = normalize_local_path(path);
        let handles = directories(&target, pinned)?;
        let root = handles.last().context("Library root handle missing")?;
        let actual = current_path(root)?;
        ensure!(
            same_path(&actual, &target),
            "Library root changed while opening"
        );
        Ok((actual, identity(root)?, handles))
    }
    pub(super) fn open_file(path: &Path) -> Result<(File, PathBuf, FileIdentity, Vec<File>)> {
        open_file_mode(path, false)
    }
    pub(super) fn open_file_pinned(
        path: &Path,
    ) -> Result<(File, PathBuf, FileIdentity, Vec<File>)> {
        open_file_mode(path, true)
    }
    fn open_file_mode(
        path: &Path,
        pinned: bool,
    ) -> Result<(File, PathBuf, FileIdentity, Vec<File>)> {
        validate_path(path)?;
        let path = normalize_local_path(path);
        validate_path(&path)?;
        let handles = directories(path.parent().context("Library file has no parent")?, pinned)?;
        let file = open_relative(
            handles.last().context("Library file parent missing")?,
            path.file_name().context("Library filename missing")?,
            false,
            pinned,
            Some(&path),
        )?;
        let actual = current_path(&file)?;
        let actual = if !pinned && !same_path(&actual, &path) && link_count(&file)? > 1 {
            path.clone()
        } else {
            actual
        };
        ensure!(
            same_path(&actual, &path),
            "Library path changed while opening"
        );
        let identity = identity(&file)?;
        Ok((file, actual, identity, handles))
    }
    pub(super) fn read_directory(file: &File, path: &Path) -> Result<std::fs::ReadDir> {
        ensure!(
            same_path(&current_path(file)?, path),
            "Library directory moved before enumeration"
        );
        Ok(std::fs::read_dir(path)?)
    }
}
#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::ffi::CString;
    use std::os::unix::{
        ffi::OsStrExt,
        fs::MetadataExt,
        io::{AsRawFd, FromRawFd},
    };
    fn open_at(parent: i32, name: &std::ffi::OsStr, directory: bool) -> Result<File> {
        let name = CString::new(name.as_bytes()).context("Invalid Unix library filename")?;
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if directory { libc::O_DIRECTORY } else { 0 };
        let descriptor = unsafe { libc::openat(parent, name.as_ptr(), flags) };
        if descriptor < 0 {
            return Err(io::Error::last_os_error()).context("Cannot open verified library path");
        }
        let file = unsafe { File::from_raw_fd(descriptor) };
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_dir() == directory && (directory || metadata.is_file()),
            "Library path has the wrong file kind"
        );
        Ok(file)
    }
    pub(super) fn validate_directory(file: &File) -> Result<()> {
        ensure!(
            file.metadata()?.is_dir(),
            "Physical root is not a directory"
        );
        Ok(())
    }
    pub(super) fn open_file_from_parent(
        existing: &[File],
        path: &Path,
    ) -> Result<(File, PathBuf, FileIdentity, Vec<File>)> {
        let mut handles = Vec::with_capacity(existing.len());
        for directory in existing {
            validate_directory(directory)?;
            handles.push(directory.try_clone()?);
        }
        let parent = handles.last().context("Cached physical parent missing")?;
        ensure!(
            path.parent().is_some_and(
                |wanted| current_path(parent).is_ok_and(|actual| same_path(wanted, &actual))
            ),
            "Cached parent left its namespace"
        );
        let file = open_at(
            parent.as_raw_fd(),
            path.file_name().context("Filename missing")?,
            false,
        )?;
        let actual = current_path(&file)?;
        ensure!(
            same_path(&actual, path),
            "Cached-parent namespace moved during opening"
        );
        let identity = identity(&file)?;
        Ok((file, actual, identity, handles))
    }
    pub(super) fn open_file_beneath(
        root: &File,
        root_path: &Path,
        path: &Path,
        _: bool,
    ) -> Result<(File, PathBuf, FileIdentity, Vec<File>)> {
        validate_path(path)?;
        let relative = relative_suffix(path, root_path).context("File is outside physical root")?;
        let mut components = relative.components().peekable();
        let mut handles = vec![root.try_clone()?];
        while let Some(component) = components.next() {
            let std::path::Component::Normal(name) = component else {
                anyhow::bail!("Unsafe root-relative file component")
            };
            ensure!(
                handles.len() <= MAX_COMPONENTS,
                "Root-relative directory budget exceeded"
            );
            let file = open_at(
                handles.last().unwrap().as_raw_fd(),
                name,
                components.peek().is_some(),
            )?;
            if components.peek().is_none() {
                let actual = current_path(&file)?;
                ensure!(
                    same_path(&actual, path),
                    "Root-relative file moved during opening"
                );
                let identity = identity(&file)?;
                return Ok((file, actual, identity, handles));
            }
            handles.push(file);
        }
        anyhow::bail!("Root-relative filename missing")
    }
    pub(super) fn identity(file: &File) -> Result<FileIdentity> {
        let metadata = file.metadata()?;
        Ok(FileIdentity {
            kind: "unix_inode".into(),
            volume: format!("{:016x}", metadata.dev()),
            file_id: format!("{:016x}", metadata.ino()),
        })
    }
    pub(super) fn link_count(file: &File) -> Result<u64> {
        Ok(file.metadata()?.nlink())
    }
    pub(super) fn change_stamp(file: &File) -> Result<FileChangeStamp> {
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file(),
            "Change stamp object is not a regular file"
        );
        Ok(FileChangeStamp::Unix {
            seconds: metadata.ctime(),
            nanoseconds: metadata.ctime_nsec(),
        })
    }
    pub(super) fn current_path(file: &File) -> Result<PathBuf> {
        let path = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
            .context("UnsupportedCapability: Linux descriptor paths are unavailable")?;
        validate_path(&path)?;
        Ok(path)
    }
    fn directories(path: &Path) -> Result<Vec<File>> {
        validate_path(path)?;
        let mut handles = vec![open_at(libc::AT_FDCWD, std::ffi::OsStr::new("/"), true)?];
        for component in path.components() {
            if let std::path::Component::Normal(name) = component {
                ensure!(
                    handles.len() < MAX_COMPONENTS,
                    "Library path component budget exceeded"
                );
                let parent = handles.last().unwrap().as_raw_fd();
                handles.push(open_at(parent, name, true)?);
            }
        }
        Ok(handles)
    }
    pub(super) fn open_root(path: &Path) -> Result<(PathBuf, FileIdentity, Vec<File>)> {
        let target =
            std::fs::canonicalize(path).context("Cannot resolve selected library directory")?;
        open_directory(&target)
    }
    pub(super) fn open_directory(path: &Path) -> Result<(PathBuf, FileIdentity, Vec<File>)> {
        let target = normalize_local_path(path);
        let handles = directories(&target)?;
        let root = handles.last().context("Library directory handle missing")?;
        let actual = current_path(root)?;
        ensure!(actual == target, "Library directory changed while opening");
        Ok((actual, identity(root)?, handles))
    }
    pub(super) fn open_file(path: &Path) -> Result<(File, PathBuf, FileIdentity, Vec<File>)> {
        let target = normalize_local_path(path);
        let handles = directories(target.parent().context("Library file has no parent")?)?;
        let file = open_at(
            handles.last().unwrap().as_raw_fd(),
            target.file_name().context("Library filename missing")?,
            false,
        )?;
        let actual = current_path(&file)?;
        ensure!(actual == target, "Library file changed while opening");
        let identity = identity(&file)?;
        Ok((file, actual, identity, handles))
    }
    pub(super) fn read_directory(file: &File, path: &Path) -> Result<std::fs::ReadDir> {
        ensure!(
            current_path(file)? == path,
            "Library directory moved after verification"
        );
        Ok(std::fs::read_dir(format!(
            "/proc/self/fd/{}",
            file.as_raw_fd()
        ))?)
    }
    pub(super) fn open_directory_pinned(path: &Path) -> Result<(PathBuf, FileIdentity, Vec<File>)> {
        open_directory(path)
    }
    pub(super) fn open_file_pinned(
        path: &Path,
    ) -> Result<(File, PathBuf, FileIdentity, Vec<File>)> {
        open_file(path)
    }
}
#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use super::*;
    pub(super) fn open_root(_: &Path) -> Result<(PathBuf, FileIdentity, Vec<File>)> {
        anyhow::bail!(
            "UnsupportedCapability: anchored library roots require Windows file identity support"
        )
    }
    pub(super) fn validate_directory(_: &File) -> Result<()> {
        anyhow::bail!("UnsupportedCapability: physical root validation")
    }
    pub(super) fn open_file_from_parent(
        _: &[File],
        path: &Path,
    ) -> Result<(File, PathBuf, FileIdentity, Vec<File>)> {
        open_file(path)
    }
    pub(super) fn open_file_beneath(
        _: &File,
        _: &Path,
        path: &Path,
        _: bool,
    ) -> Result<(File, PathBuf, FileIdentity, Vec<File>)> {
        open_file(path)
    }
    pub(super) fn open_directory(_: &Path) -> Result<(PathBuf, FileIdentity, Vec<File>)> {
        anyhow::bail!(
            "UnsupportedCapability: anchored library directories require Windows file identity support"
        )
    }
    pub(super) fn open_file(_: &Path) -> Result<(File, PathBuf, FileIdentity, Vec<File>)> {
        anyhow::bail!(
            "UnsupportedCapability: verified library file access requires Windows file identity support"
        )
    }
    pub(super) fn read_directory(_: &File, _: &Path) -> Result<std::fs::ReadDir> {
        anyhow::bail!("UnsupportedCapability: verified library directory enumeration")
    }
    pub(super) fn current_path(_: &File) -> Result<PathBuf> {
        anyhow::bail!("UnsupportedCapability: verified library descriptor locator")
    }
    pub(super) fn open_directory_pinned(path: &Path) -> Result<(PathBuf, FileIdentity, Vec<File>)> {
        open_directory(path)
    }
    pub(super) fn open_file_pinned(
        path: &Path,
    ) -> Result<(File, PathBuf, FileIdentity, Vec<File>)> {
        open_file(path)
    }
    pub(super) fn identity(_: &File) -> Result<FileIdentity> {
        anyhow::bail!("UnsupportedCapability: physical library file identity")
    }
    pub(super) fn link_count(_: &File) -> Result<u64> {
        anyhow::bail!("UnsupportedCapability: actual library link count")
    }
    pub(super) fn change_stamp(_: &File) -> Result<FileChangeStamp> {
        anyhow::bail!("UnsupportedCapability: actual file change time")
    }
}
