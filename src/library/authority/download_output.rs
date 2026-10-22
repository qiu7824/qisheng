//! Same-object read authority for acknowledged download output.

use super::*;
use crate::download::{CommittedAudio, CommittedSidecar, DestinationGrant, DownloadProducerObject};

#[derive(Clone, Serialize)]
enum DownloadReadFacts {
    Audio(CommittedAudio),
    Sidecar(CommittedSidecar),
}
impl DownloadReadFacts {
    fn path(&self) -> &PathBuf {
        match self {
            Self::Audio(facts) => &facts.path,
            Self::Sidecar(facts) => &facts.path,
        }
    }
    fn identity(&self) -> &crate::download::FileIdentity {
        match self {
            Self::Audio(facts) => &facts.identity,
            Self::Sidecar(facts) => &facts.identity,
        }
    }
    fn bytes(&self) -> u64 {
        match self {
            Self::Audio(facts) => facts.checkpoint.bytes,
            Self::Sidecar(facts) => facts.bytes,
        }
    }
}

struct SharedDownloadGrant {
    native: Mutex<SharedDownloadPins>,
    loans: AtomicUsize,
}
struct SharedDownloadPins {
    grant: Option<DestinationGrant>,
    parent: Option<DownloadPinLoan>,
    ticket: Option<RetirementTicket>,
    scope: ScopeReservation,
    _reservation: Reservation,
    queued: bool,
}
struct DownloadPinLoan(Arc<SharedDownloadGrant>);
impl DownloadPinLoan {
    fn acquire(owner: Arc<SharedDownloadGrant>) -> Result<Self> {
        let native = owner
            .native
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ensure!(
            native.grant.is_some(),
            "Native download pins already retired"
        );
        owner.loans.fetch_add(1, Ordering::AcqRel);
        drop(native);
        Ok(Self(owner))
    }
    fn inspect<R>(&self, inspect: impl FnOnce(&DestinationGrant) -> Result<R>) -> Result<R> {
        let native = self
            .0
            .native
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        inspect(
            native
                .grant
                .as_ref()
                .context("Missing retained native pins")?,
        )
    }
}
impl Clone for DownloadPinLoan {
    fn clone(&self) -> Self {
        self.0.loans.fetch_add(1, Ordering::AcqRel);
        Self(self.0.clone())
    }
}
impl Drop for DownloadPinLoan {
    fn drop(&mut self) {
        if self.0.loans.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        let ticket = {
            let mut native = self
                .0
                .native
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if self.0.loans.load(Ordering::Acquire) != 0 || native.queued {
                return;
            }
            native.queued = true;
            native
                .ticket
                .take()
                .expect("Reserved shared pin retirement ticket")
        };
        ticket.retire_payload(RetirementPayload::DownloadGrant(DownloadGrantRetirement {
            owner: self.0.clone(),
        }));
    }
}
static DOWNLOAD_GRANTS: OnceLock<Mutex<Vec<Weak<SharedDownloadGrant>>>> = OnceLock::new();
fn shared_download_grant(
    grant: &DestinationGrant,
    owner: Arc<RetirementScope>,
    retirement_only: bool,
) -> Result<DownloadPinLoan> {
    let pool = DOWNLOAD_GRANTS.get_or_init(|| Mutex::new(Vec::with_capacity(RETIREMENT_CAPACITY)));
    let mut pool = pool.lock().unwrap_or_else(|error| error.into_inner());
    pool.retain(|entry| entry.strong_count() > 0);
    share_download_grant_tree(grant, &owner, &mut pool, retirement_only)
}
fn share_download_grant_tree(
    grant: &DestinationGrant,
    owner: &Arc<RetirementScope>,
    pool: &mut Vec<Weak<SharedDownloadGrant>>,
    retirement_only: bool,
) -> Result<DownloadPinLoan> {
    for entry in pool.iter().filter_map(Weak::upgrade) {
        let native = entry
            .native
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if native
            .grant
            .as_ref()
            .is_some_and(|actual| actual.same_native_owner(grant))
        {
            ensure!(
                Arc::ptr_eq(&native.scope.0, owner),
                "Native download pins belong to another Library owner"
            );
            entry.loans.fetch_add(1, Ordering::AcqRel);
            drop(native);
            return Ok(DownloadPinLoan(entry));
        }
    }
    ensure!(
        pool.len() < RETIREMENT_CAPACITY,
        "Native downloaded pin owner capacity reached"
    );
    let reservation = Reservation::new(4096)?;
    let ticket = RetirementTicket::new()?;
    let scope = if retirement_only {
        ScopeReservation::for_retirement(owner.clone())?
    } else {
        ScopeReservation::new(owner.clone())?
    };
    let parent = grant
        .retained_parent()
        .map(|actual| share_download_grant_tree(actual, owner, pool, retirement_only))
        .transpose()?;
    ensure!(
        pool.len() < RETIREMENT_CAPACITY,
        "Native downloaded pin owner capacity reached"
    );
    let shared = Arc::new(SharedDownloadGrant {
        native: Mutex::new(SharedDownloadPins {
            grant: Some(grant.clone()),
            parent,
            ticket: Some(ticket),
            scope,
            _reservation: reservation,
            queued: false,
        }),
        loans: AtomicUsize::new(0),
    });
    pool.push(Arc::downgrade(&shared));
    DownloadPinLoan::acquire(shared)
}
pub(super) struct DownloadGrantRetirement {
    owner: Arc<SharedDownloadGrant>,
}
impl DownloadGrantRetirement {
    pub(super) fn try_close(&mut self) -> Result<()> {
        let mut native = self
            .owner
            .native
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ensure!(
            self.owner.loans.load(Ordering::Acquire) == 0,
            "Native destination pins still have a recipient"
        );
        let SharedDownloadPins { grant, parent, .. } = &mut *native;
        if let Some(grant) = grant {
            match parent.as_ref() {
                Some(parent) => {
                    parent.inspect(|receiver| grant.close_retained_leaf(Some(receiver)))?
                }
                None => grant.close_retained_leaf(None)?,
            }
        }
        grant.take();
        let parent = parent.take();
        drop(native);
        drop(parent);
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct DownloadedReadReceipt {
    authority: DownloadOutputAuthority,
}
/// A strong loan of the original namespace pins, without reader access.
#[derive(Clone)]
pub(crate) struct DownloadedPinReceipt {
    grant: DownloadPinLoan,
}
impl DownloadedPinReceipt {
    pub(crate) fn matches_grant(&self, expected: &DestinationGrant) -> Result<()> {
        self.grant.inspect(|actual| {
            ensure!(
                actual.same_native_owner(expected),
                "Destination pin receiver differs from the original native owner"
            );
            Ok(())
        })
    }
}
impl DownloadedReadReceipt {
    pub(crate) fn pin_receipt(&self) -> Result<DownloadedPinReceipt> {
        let grant = self
            .authority
            .0
            .grant
            .as_ref()
            .context("Missing retained destination pin receiver")?
            .clone();
        Ok(DownloadedPinReceipt { grant })
    }
    pub(crate) fn audio_facts(&self) -> Result<&CommittedAudio> {
        match &self.authority.0.facts {
            DownloadReadFacts::Audio(facts) => Ok(facts),
            _ => anyhow::bail!("Audio read receipt required"),
        }
    }
    pub(crate) fn sidecar_facts(&self) -> Result<&CommittedSidecar> {
        match &self.authority.0.facts {
            DownloadReadFacts::Sidecar(facts) => Ok(facts),
            _ => anyhow::bail!("Sidecar read receipt required"),
        }
    }
    pub(crate) fn is_same_reader(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.authority.0, &other.authority.0)
    }
    pub(crate) fn acknowledge_repository_commit(
        &self,
        commit: &crate::repository::DownloadReadCommit,
    ) -> Result<()> {
        self.verify()?;
        commit.verify_reader(self)?;
        self.authority
            .0
            .repository_ack
            .store(true, Ordering::Release);
        Ok(())
    }
    pub(crate) fn matches_audio(&self, expected: &CommittedAudio) -> Result<()> {
        self.verify()?;
        let actual = self.audio_facts()?;
        ensure!(
            self.authority.0.repository_ack.load(Ordering::Acquire)
                && actual.identity == expected.identity
                && same_path(&actual.path, &expected.path)
                && actual.checkpoint.completed_body
                && expected.checkpoint.completed_body
                && actual.checkpoint.bytes == expected.checkpoint.bytes
                && actual.checkpoint.sha256 == expected.checkpoint.sha256,
            "Audio read adoption has no matching actual repository ACK"
        );
        Ok(())
    }
    pub(crate) fn matches_sidecar(&self, expected: &CommittedSidecar) -> Result<()> {
        self.verify()?;
        let actual = self.sidecar_facts()?;
        ensure!(
            self.authority.0.repository_ack.load(Ordering::Acquire)
                && actual.identity == expected.identity
                && same_path(&actual.path, &expected.path)
                && actual.bytes == expected.bytes
                && actual.sha256 == expected.sha256,
            "Sidecar read adoption has no matching actual repository ACK"
        );
        Ok(())
    }
    pub(crate) fn nonce(&self) -> &str {
        &self.authority.0.nonce
    }
    pub(crate) fn origin(&self) -> &crate::download::DownloadContext {
        &self.authority.0.origin
    }
    pub(crate) fn file_identity(&self) -> FileIdentity {
        self.authority.identity()
    }
    pub(crate) fn verify(&self) -> Result<()> {
        self.authority.verify()
    }
    pub(crate) fn matches_source(
        &self,
        identity: &crate::download::FileIdentity,
        nonce: &str,
        grant: &DestinationGrant,
    ) -> Result<()> {
        self.verify()?;
        let received = self
            .authority
            .0
            .grant
            .as_ref()
            .context("Missing original destination pins")?;
        received.inspect(|received| {
            ensure!(
                self.authority.0.facts.identity() == identity
                    && self.authority.0.nonce == nonce
                    && received.same_native_owner(grant)
                    && received.identity() == grant.identity()
                    && received.working_directory_identity() == grant.working_directory_identity()
                    && received.epoch() == grant.epoch()
                    && same_path(received.path(), grant.path()),
                "Read recipient differs from the original download object or namespace"
            );
            Ok(())
        })
    }
}

#[derive(Clone)]
pub(super) struct DownloadOutputAuthority(Arc<DownloadReadInner>);
struct DownloadReadInner {
    file: Option<ManuallyDrop<File>>,
    grant: Option<DownloadPinLoan>,
    stamp: Option<FileChangeStamp>,
    facts: DownloadReadFacts,
    identity: FileIdentity,
    repository_ack: AtomicBool,
    origin: crate::download::DownloadContext,
    nonce: String,
    ticket: Option<RetirementTicket>,
    scope: Option<ScopeReservation>,
    reservation: Option<Reservation>,
}
impl Drop for DownloadReadInner {
    fn drop(&mut self) {
        if let Some(mut native) = self.file.take() {
            let file = unsafe { ManuallyDrop::take(&mut native) };
            self.ticket
                .take()
                .expect("Reserved download retirement ticket")
                .retire_payload(RetirementPayload::Download(DownloadRetirementPayload {
                    file: Some(file),
                    grant: self.grant.take(),
                    _scope: self.scope.take().expect("Reserved download read scope"),
                    _reservation: self
                        .reservation
                        .take()
                        .expect("Reserved download read bytes"),
                }));
        }
    }
}
impl DownloadOutputAuthority {
    fn file(&self) -> &File {
        self.0
            .file
            .as_ref()
            .expect("Published read authority has its native reader")
    }
    fn identity(&self) -> FileIdentity {
        self.0.identity.clone()
    }
    pub(super) fn verify(&self) -> Result<()> {
        ensure!(
            !self
                .0
                .scope
                .as_ref()
                .context("Missing read owner scope")?
                .0
                .is_closing(),
            "Downloaded read authority is closing"
        );
        let grant = self
            .0
            .grant
            .as_ref()
            .context("Read authority has no original pins")?;
        grant.inspect(DestinationGrant::verify)?;
        let file = self
            .0
            .file
            .as_ref()
            .context("Read authority has no actual native reader")?;
        ensure!(
            platform::identity(file)? == self.identity()
                && platform::link_count(file)? == 1
                && file.metadata()?.len() == self.0.facts.bytes()
                && same_path(&platform::current_path(file)?, self.0.facts.path())
                && self.0.stamp == Some(platform::change_stamp(file)?),
            "Actual downloaded read object or its representation changed"
        );
        Ok(())
    }
}

pub(super) struct DownloadRetirementPayload {
    file: Option<File>,
    grant: Option<DownloadPinLoan>,
    _scope: ScopeReservation,
    _reservation: Reservation,
}
impl DownloadRetirementPayload {
    pub(super) fn try_close(&mut self) -> Result<()> {
        if let Some(file) = self.file.take() {
            if let Err((retained, error)) = close_actual_library_file(file) {
                self.file = Some(retained);
                return Err(error.into());
            }
        }
        // Returning the final reader borrow schedules one checked pin owner.
        self.grant.take();
        Ok(())
    }
}

pub(super) enum LibraryOpenFile {
    Native(File),
    Download(DownloadOutputAuthority),
}
impl From<File> for LibraryOpenFile {
    fn from(file: File) -> Self {
        Self::Native(file)
    }
}
impl Deref for LibraryOpenFile {
    type Target = File;
    fn deref(&self) -> &File {
        match self {
            Self::Native(file) => file,
            Self::Download(download) => download.file(),
        }
    }
}
impl LibraryOpenFile {
    pub(super) fn try_clone(&self) -> io::Result<Self> {
        match self {
            Self::Native(file) => file.try_clone().map(Self::Native),
            Self::Download(download) => {
                download.verify().map_err(io::Error::other)?;
                Ok(Self::Download(download.clone()))
            }
        }
    }
}

impl AuthorityRegistry {
    pub(crate) fn download_authorization(
        &self,
        asset_id: &str,
    ) -> Result<Option<FileAuthorization>> {
        let authorization = self
            .inner
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .assets
            .get(asset_id)
            .filter(|asset| asset.authorization.inner.download_output.is_some())
            .map(|asset| asset.authorization.clone());
        if let Some(authorization) = &authorization {
            authorization.check_intent()?;
            authorization
                .inner
                .download_output
                .as_ref()
                .context("Missing downloaded native authority")?
                .verify()?;
        }
        Ok(authorization)
    }
    pub(crate) fn stage_download_pin_retirement(
        &self,
        producer: &DownloadProducerObject,
    ) -> Result<DownloadedPinReceipt> {
        producer.inspect_retained_pins_after_join(|grant| {
            Ok(DownloadedPinReceipt {
                grant: shared_download_grant(grant, self.owner.clone(), true)?,
            })
        })
    }
    /// Stages only native read access; neither metadata nor a DTO is a grant.
    pub(crate) fn stage_download_output(
        &self,
        producer: &DownloadProducerObject,
        facts: &CommittedAudio,
        nonce: &str,
    ) -> Result<(FileAuthorization, DownloadedReadReceipt)> {
        self.stage_actual_download_output(producer, DownloadReadFacts::Audio(facts.clone()), nonce)
    }
    pub(crate) fn stage_download_sidecar(
        &self,
        producer: &DownloadProducerObject,
        facts: &CommittedSidecar,
        nonce: &str,
    ) -> Result<(FileAuthorization, DownloadedReadReceipt)> {
        self.stage_actual_download_output(
            producer,
            DownloadReadFacts::Sidecar(facts.clone()),
            nonce,
        )
    }
    fn stage_actual_download_output(
        &self,
        producer: &DownloadProducerObject,
        facts: DownloadReadFacts,
        nonce: &str,
    ) -> Result<(FileAuthorization, DownloadedReadReceipt)> {
        validate_path(facts.path())?;
        ensure!(
            nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Invalid download read nonce"
        );
        ensure!(
            facts.bytes() > 0 && facts.bytes() <= MAX_CONTENT_HASH_BYTES,
            "Downloaded read output exceeds its bound"
        );
        let authority_reservation = Reservation::new(
            facts.path().capacity() * 2 + serde_json::to_vec(&facts)?.len() * 2 + 32 * 1024,
        )?;
        let authorization_reservation = Reservation::new(facts.path().capacity() * 2 + 4096)?;
        let ticket = RetirementTicket::new()?;
        let scope = ScopeReservation::new(self.owner.clone())?;
        let origin = producer.context_binding()?;
        let mut authority = DownloadOutputAuthority(Arc::new(DownloadReadInner {
            file: None,
            grant: None,
            stamp: None,
            facts: facts.clone(),
            identity: FileIdentity::from_native_parts(
                facts.identity().device,
                &facts.identity().object,
            )?,
            repository_ack: AtomicBool::new(false),
            origin,
            nonce: nonce.into(),
            ticket: Some(ticket),
            scope: Some(scope),
            reservation: Some(authority_reservation),
        }));
        let expected_identity = authority.identity();
        let recipient = Arc::get_mut(&mut authority.0)
            .context("Native read recipient was shared before capture")?;
        let capture = |source: &File, grant: &DestinationGrant| -> Result<()> {
            recipient.grant = Some(shared_download_grant(grant, self.owner.clone(), false)?);
            capture_readonly_same_object(source, &mut recipient.file)?;
            let file = recipient
                .file
                .as_ref()
                .context("Read duplicate unavailable")?;
            ensure!(
                platform::identity(file)? == expected_identity && platform::link_count(file)? == 1,
                "Read duplicate differs from the original single-link object"
            );
            ensure!(
                same_path(&platform::current_path(file)?, facts.path())
                    && file.metadata()?.len() == facts.bytes(),
                "Read duplicate namespace or length changed"
            );
            recipient.stamp = Some(platform::change_stamp(file)?);
            Ok(())
        };
        match &facts {
            DownloadReadFacts::Audio(facts) => {
                producer.inspect_published_after_join(facts, capture)?
            }
            DownloadReadFacts::Sidecar(facts) => {
                producer.inspect_sidecar_after_join(facts, capture)?
            }
        }
        authority.verify()?;
        let receipt = DownloadedReadReceipt {
            authority: authority.clone(),
        };
        let authorization = FileAuthorization {
            inner: Arc::new(AuthorizationInner {
                path: facts.path().clone(),
                binding: Arc::new(RwLock::new(facts.path().clone())),
                live_open: Arc::new(Mutex::new(VecDeque::with_capacity(
                    MAX_LIVE_OPENS_PER_ASSET,
                ))),
                expected_identity: Arc::new(RwLock::new(Some(authority.identity()))),
                explicit: None,
                roots: Vec::new(),
                gate: Arc::new(AtomicBool::new(true)),
                cancel: None,
                owner: self.owner.clone(),
                download_output: Some(authority),
                _reservation: authorization_reservation,
            }),
        };
        Ok((authorization, receipt))
    }

    /// Associates logical IDs after metadata commit, without an explicit-file grant.
    pub(crate) fn install_download_output(
        &self,
        asset_id: &str,
        binding_revision: u64,
        authorization: &FileAuthorization,
    ) -> Result<FileAuthorization> {
        crate::domain::library::validate_library_id(asset_id)?;
        ensure!(
            binding_revision > 0 && Arc::ptr_eq(&self.owner, &authorization.inner.owner),
            "Invalid downloaded catalog binding"
        );
        authorization.check()?;
        let download = authorization
            .inner
            .download_output
            .as_ref()
            .context("Missing same-object download authority")?;
        download.verify()?;
        let digest = format!(
            "download-read:{}:{}:{}",
            binding_revision,
            download.0.nonce,
            match &download.0.facts {
                DownloadReadFacts::Audio(facts) => &facts.checkpoint.sha256,
                DownloadReadFacts::Sidecar(facts) => &facts.sha256,
            }
        );
        let mut state = self
            .inner
            .write()
            .unwrap_or_else(|error| error.into_inner());
        ensure!(
            state.assets.contains_key(asset_id) || state.assets.len() < 50_000,
            "Library authorization asset budget exceeded"
        );
        if let Some(previous) = state.assets.get(asset_id) {
            if !Arc::ptr_eq(
                &previous.authorization.inner.gate,
                &authorization.inner.gate,
            ) {
                previous
                    .authorization
                    .inner
                    .gate
                    .store(false, Ordering::Release);
            }
        }
        state.assets.insert(
            asset_id.into(),
            InstalledAsset {
                digest,
                identity: Some(download.identity()),
                authorization: authorization.clone(),
            },
        );
        Ok(authorization.clone())
    }
}

impl FileAuthorization {
    pub(crate) fn has_download_read_binding(&self) -> bool {
        self.inner.download_output.is_some()
    }
    pub(crate) fn is_download_read(&self) -> bool {
        self.inner
            .download_output
            .as_ref()
            .is_some_and(|download| self.check_binding_gate().is_ok() && download.verify().is_ok())
    }
    pub(super) fn open_download_output(&self) -> Result<Option<AuthorizedFile>> {
        let Some(download) = &self.inner.download_output else {
            return Ok(None);
        };
        self.check_intent()?;
        download.verify()?;
        ensure!(
            same_path(&self.current_path(), download.0.facts.path()),
            "Downloaded read binding changed"
        );
        let reservation = Reservation::new(self.inner.path.capacity() * 2 + 32 * 1024)?;
        let mut live = self
            .inner
            .live_open
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        live.retain(|permit| permit.strong_count() > 0);
        ensure!(
            live.len() < MAX_LIVE_OPENS_PER_ASSET,
            "Downloaded read capacity reached"
        );
        let file = LibraryOpenFile::Download(download.clone());
        let opened = AuthorizedFile {
            permit: Arc::new(OpenPermit {
                stamp: RepresentationStamp::read(&file)?,
                file,
                path: download.0.facts.path().clone(),
                identity: download.identity(),
                failed: AtomicBool::new(false),
                grants: Vec::new(),
                _directories: Vec::new(),
                gate: Some(self.inner.gate.clone()),
                authorization: Some(self.clone()),
                _audio: None,
                _reservation: reservation,
            }),
            position: 0,
        };
        live.push_back(Arc::downgrade(&opened.permit));
        drop(live);
        opened.check()?;
        Ok(Some(opened))
    }
    pub(super) fn check_download_location(&self, actual: &Path) -> Result<bool> {
        let Some(download) = &self.inner.download_output else {
            return Ok(false);
        };
        self.check_binding_gate()?;
        download.verify()?;
        ensure!(
            same_path(actual, download.0.facts.path()) && same_path(actual, &self.current_path()),
            "Downloaded native location changed"
        );
        Ok(true)
    }
}
impl AuthorizedFile {
    pub(super) fn download_snapshot(&self) -> Result<Option<ReadSnapshot>> {
        let LibraryOpenFile::Download(download) = &self.permit.file else {
            return Ok(None);
        };
        self.check_gate()?;
        download.verify()?;
        let snapshot = ReadSnapshot {
            path: download.0.facts.path().clone(),
            _file: LibraryOpenFile::Download(download.clone()),
            directories: Vec::new(),
            _reservation: Reservation::new(self.permit.path.capacity() * 2 + 4096)?,
        };
        self.verify_snapshot(&snapshot)?;
        Ok(Some(snapshot))
    }
}

#[cfg(windows)]
fn capture_readonly_same_object(
    source: &File,
    recipient: &mut Option<ManuallyDrop<File>>,
) -> Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::{
        Foundation::{CompareObjectHandles, DuplicateHandle, INVALID_HANDLE_VALUE},
        Storage::FileSystem::FILE_GENERIC_READ,
        System::Threading::GetCurrentProcess,
    };
    let mut raw = std::ptr::null_mut();
    ensure!(
        recipient.is_none(),
        "Native read recipient is already occupied"
    );
    let process = unsafe { GetCurrentProcess() };
    let result = unsafe {
        DuplicateHandle(
            process,
            source.as_raw_handle(),
            process,
            &mut raw,
            FILE_GENERIC_READ,
            0,
            0,
        )
    };
    // No FID check, callback or body operation precedes the recipient capture.
    if !raw.is_null() && raw != INVALID_HANDLE_VALUE {
        *recipient = Some(ManuallyDrop::new(unsafe { File::from_raw_handle(raw) }));
    }
    ensure!(
        result != 0,
        "Cannot create restricted same-object reader: {}",
        io::Error::last_os_error()
    );
    let reader = recipient
        .as_ref()
        .context("Native read operation returned no descriptor")?;
    ensure!(
        unsafe { CompareObjectHandles(source.as_raw_handle(), reader.as_raw_handle()) } != 0,
        "Read duplicate is not the original FILE_OBJECT"
    );
    Ok(())
}

#[cfg(all(test, windows))]
mod pin_retirement_tests {
    use super::*;

    #[test]
    fn closing_scope_charges_a_new_native_retirement_owner_before_release() {
        let directory = tempfile::tempdir().unwrap();
        let selected = DestinationGrant::open_selected(directory.path(), 1).unwrap();
        let registry = AuthorityRegistry::default();
        registry.begin_shutdown();
        assert!(registry.retirement_barrier().wait(Duration::ZERO));
        assert!(shared_download_grant(&selected, registry.owner.clone(), false).is_err());
        let pins = shared_download_grant(&selected, registry.owner.clone(), true).unwrap();
        assert!(!registry.retirement_barrier().wait(Duration::ZERO));
        drop(selected);
        drop(pins);
        assert!(registry.retirement_barrier().wait(Duration::from_secs(5)));
    }

    #[test]
    fn queued_native_pin_owner_accepts_the_same_actual_borrow_again() {
        let directory = tempfile::tempdir().unwrap();
        let selected = DestinationGrant::open_selected(directory.path(), 1).unwrap();
        let registry = AuthorityRegistry::default();
        let first = shared_download_grant(&selected, registry.owner.clone(), false).unwrap();
        let actual_owner = Arc::downgrade(&first.0);
        drop(first);
        registry.begin_shutdown();
        let second = shared_download_grant(&selected, registry.owner.clone(), true).unwrap();
        assert!(Arc::ptr_eq(&actual_owner.upgrade().unwrap(), &second.0));
        assert!(
            shared_download_grant(&selected, Arc::new(RetirementScope::default()), true).is_err()
        );
        drop(selected);
        drop(second);
        assert!(registry.retirement_barrier().wait(Duration::from_secs(5)));
    }

    #[test]
    fn grouped_native_pin_borrows_retire_through_one_actual_parent() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("first")).unwrap();
        std::fs::create_dir(directory.path().join("second")).unwrap();
        let selected = DestinationGrant::open_selected(directory.path(), 1).unwrap();
        let first = selected.open_existing_group("first").unwrap();
        let second = selected.open_existing_group("second").unwrap();
        let registry = AuthorityRegistry::default();
        let first_owner = shared_download_grant(&first, registry.owner.clone(), false).unwrap();
        let second_owner = shared_download_grant(&second, registry.owner.clone(), false).unwrap();
        assert!(!Arc::ptr_eq(&first_owner.0, &second_owner.0));
        assert!(Arc::ptr_eq(
            &first_owner
                .0
                .native
                .lock()
                .unwrap()
                .parent
                .as_ref()
                .unwrap()
                .0,
            &second_owner
                .0
                .native
                .lock()
                .unwrap()
                .parent
                .as_ref()
                .unwrap()
                .0
        ));
        let pins = DownloadedPinReceipt {
            grant: first_owner.clone(),
        };
        pins.matches_grant(&first).unwrap();
        assert!(pins.matches_grant(&second).is_err());
        first.revoke();
        pins.matches_grant(&first).unwrap();
        assert!(first.verify().is_err());
        drop(pins);
        drop(first);
        drop(second);
        drop(selected);
        drop(first_owner);
        assert!(!registry.retirement_barrier().wait(Duration::ZERO));
        drop(second_owner);
        registry.begin_shutdown();
        assert!(registry.retirement_barrier().wait(Duration::from_secs(5)));
        std::fs::remove_dir(directory.path().join("first")).unwrap();
        std::fs::remove_dir(directory.path().join("second")).unwrap();
    }
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn capture_readonly_same_object(
    source: &File,
    recipient: &mut Option<ManuallyDrop<File>>,
) -> Result<()> {
    use std::{
        ffi::CString,
        os::unix::{
            ffi::OsStrExt,
            io::{AsRawFd, FromRawFd},
        },
    };
    ensure!(
        recipient.is_none(),
        "Native read recipient is already occupied"
    );
    #[cfg(target_os = "linux")]
    let path = std::path::PathBuf::from(format!("/proc/self/fd/{}", source.as_raw_fd()));
    #[cfg(target_os = "macos")]
    let path = {
        let mut bytes = [0u8; libc::PATH_MAX as usize];
        ensure!(
            unsafe { libc::fcntl(source.as_raw_fd(), libc::F_GETPATH, bytes.as_mut_ptr()) } >= 0,
            "Cannot resolve published download descriptor: {}",
            io::Error::last_os_error()
        );
        let end = bytes
            .iter()
            .position(|b| *b == 0)
            .context("Unterminated native download path")?;
        std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&bytes[..end]))
    };
    let name = CString::new(path.as_os_str().as_bytes())?;
    let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NONBLOCK;
    #[cfg(target_os = "macos")]
    let flags = flags | libc::O_NOFOLLOW;
    let descriptor = unsafe { libc::open(name.as_ptr(), flags) };
    if descriptor >= 0 {
        *recipient = Some(ManuallyDrop::new(unsafe { File::from_raw_fd(descriptor) }));
    }
    ensure!(
        descriptor >= 0,
        "Cannot open a restricted download reader: {}",
        io::Error::last_os_error()
    );
    let reader = recipient
        .as_ref()
        .context("Native read recipient has no descriptor")?;
    ensure!(
        platform::identity(reader)? == platform::identity(source)?,
        "Read recipient identifies another native file"
    );
    let access = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) };
    ensure!(
        access >= 0 && access & libc::O_ACCMODE == libc::O_RDONLY,
        "Native download reader has writable access"
    );
    Ok(())
}
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn capture_readonly_same_object(_: &File, _: &mut Option<ManuallyDrop<File>>) -> Result<()> {
    anyhow::bail!("UnsupportedCapability: native same-object download adoption requires Windows")
}

#[cfg(windows)]
pub(super) fn close_actual_library_file(file: File) -> std::result::Result<(), (File, io::Error)> {
    use std::os::windows::io::{FromRawHandle, IntoRawHandle};
    let raw = file.into_raw_handle();
    if unsafe { windows_sys::Win32::Foundation::CloseHandle(raw) } == 0 {
        let error = io::Error::last_os_error();
        return Err((unsafe { File::from_raw_handle(raw) }, error));
    }
    Ok(())
}
#[cfg(not(windows))]
pub(super) fn close_actual_library_file(file: File) -> std::result::Result<(), (File, io::Error)> {
    drop(file);
    Ok(())
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod unix_reader_tests {
    use super::*;
    use std::io::{Read, Write};
    #[test]
    fn published_reader_has_same_identity_but_cannot_write() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("已完成.flac");
        let mut source = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        source.write_all(b"published content").unwrap();
        source.sync_all().unwrap();
        let mut captured = None;
        capture_readonly_same_object(&source, &mut captured).unwrap();
        let mut reader = ManuallyDrop::into_inner(captured.take().unwrap());
        assert_eq!(
            platform::identity(&reader).unwrap(),
            platform::identity(&source).unwrap()
        );
        assert!(reader.write_all(b"overwrite").is_err());
        let mut content = String::new();
        reader.read_to_string(&mut content).unwrap();
        assert_eq!(content, "published content");
        close_actual_library_file(reader).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"published content");
    }
}
