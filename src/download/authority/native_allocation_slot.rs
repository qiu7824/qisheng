//! Native allocation captures its descriptor before validation or first write.

use super::*;
use std::sync::OnceLock;

const CAPACITY: usize = crate::download::MAX_DOWNLOAD_TASKS + crate::download::MAX_DOWNLOAD_WORKERS;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AllocationPhase {
    Reserved,
    NativeAttempted,
    NativeCaptured,
    CreationAcknowledged,
    HandlesClosed,
}
struct AllocationState {
    plan: DownloadPlan,
    context: crate::download::DownloadContext,
    kind: crate::download::artifact::OwnedArtifactKind,
    nonce: String,
    basename: String,
    maximum_bytes: u64,
    phase: AllocationPhase,
    file: Option<File>,
    record: Option<OwnedPartRecord>,
    written: u64,
}
static ALLOCATIONS: OnceLock<Mutex<Vec<Option<Arc<Mutex<AllocationState>>>>>> = OnceLock::new();
fn allocations() -> &'static Mutex<Vec<Option<Arc<Mutex<AllocationState>>>>> {
    ALLOCATIONS.get_or_init(|| Mutex::new((0..CAPACITY).map(|_| None).collect()))
}
pub(crate) struct NativeAllocationSlot {
    index: usize,
    state: Arc<Mutex<AllocationState>>,
}
impl NativeAllocationSlot {
    pub(crate) fn reserve(
        plan: DownloadPlan,
        context: crate::download::DownloadContext,
        kind: crate::download::artifact::OwnedArtifactKind,
        nonce: String,
        maximum_bytes: u64,
    ) -> Result<Self> {
        context.validate()?;
        plan.verify()?;
        ensure!(
            nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Invalid native allocation nonce"
        );
        ensure!(
            maximum_bytes > 0 && maximum_bytes <= MAX_DOWNLOAD_BYTES,
            "Invalid native allocation byte reservation"
        );
        let mut registry = allocations()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let index = registry
            .iter()
            .position(Option::is_none)
            .context("Actual download object capacity reached")?;
        let charged = registry
            .iter()
            .flatten()
            .map(|entry| {
                entry
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .maximum_bytes
            })
            .sum::<u64>();
        ensure!(
            charged
                .checked_add(maximum_bytes)
                .is_some_and(|bytes| bytes <= crate::download::MAX_OWNED_PART_BYTES),
            "Actual download byte reservation exhausted"
        );
        let basename = format!(".lx-download-{nonce}.part");
        let state = Arc::new(Mutex::new(AllocationState {
            plan,
            context,
            kind,
            nonce,
            basename,
            maximum_bytes,
            phase: AllocationPhase::Reserved,
            file: None,
            record: None,
            written: 0,
        }));
        registry[index] = Some(state.clone());
        Ok(Self { index, state })
    }

    /// The durable intent is acknowledged before FILE_CREATE. The same slot
    /// is already strongly retained when the native API returns its handle.
    pub(crate) fn create(
        &self,
        acknowledge_intent: impl FnOnce(
            &crate::download::DownloadContext,
            &str,
            &DestinationGrant,
            u64,
        ) -> Result<()>,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        ensure!(
            state.phase == AllocationPhase::Reserved,
            "Native allocation already attempted"
        );
        state.plan.verify()?;
        acknowledge_intent(
            &state.context,
            &state.basename,
            &state.plan.grant,
            state.maximum_bytes,
        )?;
        state.plan.verify()?;
        state.phase = AllocationPhase::NativeAttempted;
        capture_native_create(&mut state)?;
        Ok(())
    }

    pub(crate) fn acknowledge_created(
        &self,
        journal: &dyn crate::download::artifact::OwnedArtifactJournal,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        ensure!(
            state.phase == AllocationPhase::NativeCaptured,
            "Actual native allocation is unavailable"
        );
        let file = state
            .file
            .as_ref()
            .context("Captured native descriptor unavailable")?;
        state.plan.verify()?;
        platform::validate(file, false)?;
        ensure!(
            file.metadata()?.len() == 0 && platform::link_count(file)? == 1,
            "Created download must be its original zero-length single-link object"
        );
        let identity = platform::identity(file)?;
        platform::verify_file_name(state.plan.grant.parent(), &state.basename, &identity)?;
        let record = OwnedPartRecord {
            basename: state.basename.clone(),
            nonce: state.nonce.clone(),
            identity,
            destination_identity: state.plan.root_identity.clone(),
            destination_epoch: state.plan.epoch,
            final_basename: state.plan.basename.clone(),
        };
        state.record = Some(record.clone());
        let group = state
            .plan
            .grant
            .0
            .parent
            .as_ref()
            .map(|_| {
                state
                    .plan
                    .grant
                    .path()
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("Invalid actual download group")
                    .map(str::to_owned)
            })
            .transpose()?;
        let allocation = crate::download::artifact::OwnedArtifactAllocation {
            kind: state.kind,
            record,
            working_directory_path: state.plan.grant.path().to_path_buf(),
            working_directory_identity: state.plan.grant.working_directory_identity().clone(),
            selected_root_identity: state.plan.grant.identity().clone(),
            relative_group: group,
            reserved_max_bytes: state.maximum_bytes,
            context: state.context.clone(),
        };
        allocation.validate()?;
        // A rejected or panicking journal cannot drop the original descriptor.
        journal.record_created(&allocation)?;
        state.plan.verify()?;
        state.phase = AllocationPhase::CreationAcknowledged;
        Ok(())
    }

    pub(crate) fn append(
        &self,
        bytes: &[u8],
        execution: &crate::download::DownloadExecutionLease,
        cancel: &DownloadCancel,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        ensure!(
            state.phase == AllocationPhase::CreationAcknowledged,
            "First body write has no actual creation ACK"
        );
        execution.check(&state.context)?;
        cancel.check()?;
        state.plan.verify()?;
        let next = state
            .written
            .checked_add(bytes.len() as u64)
            .context("Native body byte count overflow")?;
        ensure!(
            next <= state.maximum_bytes,
            "Native body exceeds its unchanged reservation"
        );
        let offset = state.written;
        let file = state
            .file
            .as_mut()
            .context("Native body owner unavailable")?;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(bytes)?;
        file.sync_data()?;
        state.written = next;
        execution.check(&state.context)?;
        cancel.check()?;
        state.plan.verify()?;
        Ok(())
    }

    /// Cancellation closes only this actual descriptor. Artifact deletion and
    /// persistent reservation release require their separate native receipts.
    pub(crate) fn close_file(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(file) = state.file.take() {
            if let Err((retained, error)) = close_captured_file(file) {
                state.file = Some(retained);
                return Err(error.into());
            }
        }
        state.phase = AllocationPhase::HandlesClosed;
        Ok(())
    }

    /// Ownership inspection is internal and cannot return or duplicate a raw
    /// descriptor. The keeper keeps its actual file across callback panic.
    pub(crate) fn inspect<R>(
        &self,
        inspect: impl FnOnce(&File, &DestinationGrant) -> Result<R>,
    ) -> Result<R> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        inspect(
            state
                .file
                .as_ref()
                .context("Native descriptor unavailable")?,
            &state.plan.grant,
        )
    }
}

#[cfg(windows)]
fn capture_native_create(state: &mut AllocationState) -> Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::{
        Wdk::{
            Foundation::OBJECT_ATTRIBUTES,
            Storage::FileSystem::{
                FILE_CREATE, FILE_NON_DIRECTORY_FILE, FILE_OPEN_REPARSE_POINT,
                FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
            },
        },
        Win32::{
            Foundation::{
                GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, OBJ_CASE_INSENSITIVE,
                RtlNtStatusToDosError, UNICODE_STRING,
            },
            Storage::FileSystem::{DELETE, FILE_SHARE_READ, SYNCHRONIZE},
            System::IO::IO_STATUS_BLOCK,
        },
    };
    let mut text: Vec<u16> = state.basename.encode_utf16().collect();
    let name = UNICODE_STRING {
        Length: (text.len() * 2) as u16,
        MaximumLength: (text.len() * 2) as u16,
        Buffer: text.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: state.plan.grant.parent().as_raw_handle(),
        ObjectName: &name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut raw = std::ptr::null_mut();
    let mut status_block = IO_STATUS_BLOCK::default();
    let status = unsafe {
        NtCreateFile(
            &mut raw,
            GENERIC_READ | GENERIC_WRITE | DELETE | SYNCHRONIZE,
            &attributes,
            &mut status_block,
            std::ptr::null(),
            0,
            FILE_SHARE_READ,
            FILE_CREATE,
            FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT | FILE_NON_DIRECTORY_FILE,
            std::ptr::null(),
            0,
        )
    };
    // Capture first: no validation, identity lookup, SQL, allocation or callback
    // is allowed between the successful kernel return and the admitted slot.
    if !raw.is_null() && raw != INVALID_HANDLE_VALUE {
        state.file = Some(unsafe { File::from_raw_handle(raw) });
        state.phase = AllocationPhase::NativeCaptured;
    }
    ensure!(
        status >= 0,
        "Native download allocation failed: {}",
        io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)
    );
    ensure!(
        state.file.is_some(),
        "Native allocation returned no descriptor"
    );
    Ok(())
}
#[cfg(not(windows))]
fn capture_native_create(_: &mut AllocationState) -> Result<()> {
    bail!("UnsupportedCapability: native download allocation capture requires Windows")
}

#[cfg(windows)]
fn close_captured_file(file: File) -> std::result::Result<(), (File, io::Error)> {
    use std::os::windows::io::{FromRawHandle, IntoRawHandle};
    let raw = file.into_raw_handle();
    if unsafe { windows_sys::Win32::Foundation::CloseHandle(raw) } == 0 {
        let error = io::Error::last_os_error();
        return Err((unsafe { File::from_raw_handle(raw) }, error));
    }
    Ok(())
}
#[cfg(not(windows))]
fn close_captured_file(file: File) -> std::result::Result<(), (File, io::Error)> {
    Err((
        file,
        io::Error::new(
            io::ErrorKind::Unsupported,
            "Actual native close requires Windows",
        ),
    ))
}
