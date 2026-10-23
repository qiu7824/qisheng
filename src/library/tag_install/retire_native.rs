//! Checked native closure and calibrated exact-FileID retirement.
use super::*;

#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle};
#[cfg(windows)]
use windows_sys::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FILE_OPEN, FILE_OPEN_BY_FILE_ID, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
            NtCreateFile,
        },
    },
    Win32::{
        Foundation::{
            CloseHandle, FILETIME, INVALID_HANDLE_VALUE, OBJ_CASE_INSENSITIVE, UNICODE_STRING,
        },
        Storage::FileSystem::*,
        System::IO::IO_STATUS_BLOCK,
    },
};

pub(super) fn close(file: &mut Option<File>) -> Result<()> {
    #[cfg(windows)]
    {
        if let Some(owned) = file.take() {
            let handle = owned.into_raw_handle();
            if unsafe { CloseHandle(handle) } == 0 {
                let error = io::Error::last_os_error();
                *file = Some(unsafe { File::from_raw_handle(handle) });
                return Err(error).context("Actual native tag owner close failed");
            }
        }
        Ok(())
    }
    #[cfg(unix)]
    {
        use std::os::unix::io::IntoRawFd;
        if let Some(owned) = file.take() {
            let fd = owned.into_raw_fd();
            // A failed Unix close must not be retried against a possibly reused fd.
            if unsafe { libc::close(fd) } != 0 {
                return Err(io::Error::last_os_error())
                    .context("Native tag descriptor close failed");
            }
        }
        Ok(())
    }
}

pub(super) fn freeze_write_time(file: &File) -> Result<()> {
    #[cfg(windows)]
    {
        let freeze = FILETIME {
            dwLowDateTime: u32::MAX,
            dwHighDateTime: u32::MAX,
        };
        ensure!(
            unsafe {
                SetFileTime(
                    file.as_raw_handle(),
                    std::ptr::null(),
                    std::ptr::null(),
                    &freeze,
                )
            } != 0,
            "Cannot stabilize actual selected writer time: {}",
            io::Error::last_os_error()
        );
        Ok(())
    }
    #[cfg(unix)]
    {
        file.sync_all()?;
        Ok(())
    }
}
pub(super) fn settle_write_time(file: &File) -> Result<()> {
    #[cfg(windows)]
    {
        let ticks = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_nanos()
            / 100;
        let ticks = u64::try_from(ticks)?
            .checked_add(116_444_736_000_000_000)
            .context("Tag native timestamp overflow")?;
        let actual = FILETIME {
            dwLowDateTime: ticks as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        };
        ensure!(
            unsafe {
                SetFileTime(
                    file.as_raw_handle(),
                    std::ptr::null(),
                    std::ptr::null(),
                    &actual,
                )
            } != 0,
            "Cannot settle actual selected writer time: {}",
            io::Error::last_os_error()
        );
        freeze_write_time(file)?;
        file.sync_all()?;
        Ok(())
    }
    #[cfg(unix)]
    {
        file.sync_all()?;
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct Calibration {
    probe: Option<File>,
    reader: Option<File>,
    controls: Vec<Option<File>>,
    probe_identity: Option<FileIdentity>,
    probe_pending: bool,
    volume: Option<String>,
    directory: Option<FileIdentity>,
    complete: bool,
    #[cfg(unix)]
    unix_object: Option<File>,
}

#[derive(Default)]
pub(super) struct OriginalCalibration {
    control: Option<File>,
    reader: Option<File>,
    stability: Option<File>,
    control_excluded_reader: bool,
    control_closed: bool,
    reader_excluded_control: bool,
    source_volume: Option<String>,
}
impl OriginalCalibration {
    pub(super) fn has_live_handles(&self) -> bool {
        self.control.is_some() || self.reader.is_some() || self.stability.is_some()
    }
    pub(super) fn close(&mut self) -> Result<()> {
        close(&mut self.reader)?;
        close(&mut self.control)?;
        close(&mut self.stability)
    }
    #[cfg(windows)]
    pub(super) fn prove(
        &mut self,
        selection: &TagWriteSelection,
        intent: &TagOriginalRecoveryIntent,
        context: &ScanContext,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<(File, String)> {
        use std::os::windows::fs::OpenOptionsExt;
        let open = |write: bool| -> Result<File> {
            OpenOptions::new().read(true).write(write).share_mode(0)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(&selection.path)
                .map_err(|error| if matches!(error.raw_os_error(), Some(32 | 33)) {
                    LibraryError::new(LibraryErrorKind::Busy,
                        "Actual original has an active reader or writer during recovery calibration").into()
                } else { error.into() })
        };
        let scope = |file: &File, expected_volume: &mut Option<String>| -> Result<()> {
            ensure!(
                normal_file(file)?
                    && links(file)? == 1
                    && intent.observation.identity == selection.expected_identity
                    && normalize_local_path(&intent.observation.path)
                        == normalize_local_path(&selection.path)
                    && observe_file(file)? == intent.observation,
                LibraryError::conflict(
                    "Original recovery control differs from durable retirement intent"
                )
            );
            let actual_volume = volume(file)?;
            if let Some(expected) = expected_volume.as_ref() {
                ensure!(
                    *expected == actual_volume,
                    "Original recovery native volume changed between controls"
                );
            } else {
                *expected_volume = Some(actual_volume);
            }
            Ok(())
        };
        context.check()?;
        check()?;
        if !self.control_closed {
            if self.control.is_none() {
                self.control = Some(open(true)?);
            }
            scope(self.control.as_ref().unwrap(), &mut self.source_volume)?;
            if !self.control_excluded_reader {
                match open(false) {
                    Err(error)
                        if error
                            .downcast_ref::<LibraryError>()
                            .is_some_and(|e| e.kind == LibraryErrorKind::Busy) =>
                    {
                        self.control_excluded_reader = true
                    }
                    Err(error) => return Err(error),
                    Ok(file) => {
                        self.reader = Some(file);
                        anyhow::bail!("Native original control did not exclude a competing reader");
                    }
                }
            }
            check()?;
            close(&mut self.control)?;
            self.control_closed = true;
        }
        context.check()?;
        check()?;
        if self.reader.is_none() {
            self.reader = Some(open(false)?);
        }
        scope(self.reader.as_ref().unwrap(), &mut self.source_volume)?;
        if !self.reader_excluded_control {
            match open(true) {
                Err(error)
                    if error
                        .downcast_ref::<LibraryError>()
                        .is_some_and(|e| e.kind == LibraryErrorKind::Busy) =>
                {
                    self.reader_excluded_control = true
                }
                Err(error) => return Err(error),
                Ok(file) => {
                    self.control = Some(file);
                    anyhow::bail!("Native original reader did not exclude a competing writer");
                }
            }
        }
        let sha256 = hash_body(self.reader.as_mut().unwrap(), context)?;
        scope(self.reader.as_ref().unwrap(), &mut self.source_volume)?;
        ensure!(
            sha256 == intent.facts.new_body_sha256,
            LibraryError::conflict("Actual original body differs from durable retirement intent")
        );
        check()?;
        close(&mut self.reader)?;
        if self.stability.is_none() {
            self.stability = Some(
                OpenOptions::new()
                    .read(true)
                    .share_mode(FILE_SHARE_READ)
                    .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                    .open(&selection.path)?,
            );
        }
        // READ_DATA participates in native sharing. Revalidate everything after
        // the exclusive-reader close/admission gap under this new actual lease.
        scope(self.stability.as_ref().unwrap(), &mut self.source_volume)?;
        let stable_sha256 = hash_body(self.stability.as_mut().unwrap(), context)?;
        scope(self.stability.as_ref().unwrap(), &mut self.source_volume)?;
        ensure!(
            stable_sha256 == sha256 && stable_sha256 == intent.facts.new_body_sha256,
            LibraryError::conflict("Original body changed before the actual stability lease")
        );
        check()?;
        Ok((self.stability.take().unwrap(), stable_sha256))
    }
    #[cfg(not(windows))]
    pub(super) fn prove(
        &mut self,
        _: &TagWriteSelection,
        _: &TagOriginalRecoveryIntent,
        _: &ScanContext,
        _: &dyn Fn() -> Result<()>,
    ) -> Result<(File, String)> {
        anyhow::bail!("UnsupportedCapability: calibrated original retirement requires Windows")
    }
}

#[cfg(windows)]
fn volume(file: &File) -> Result<String> {
    let mut filesystem = [0u16; 32];
    let (mut serial, mut maximum, mut flags) = (0, 0, 0);
    ensure!(
        unsafe {
            GetVolumeInformationByHandleW(
                file.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                &mut serial,
                &mut maximum,
                &mut flags,
                filesystem.as_mut_ptr(),
                filesystem.len() as u32,
            )
        } != 0,
        "Cannot inspect actual tag cleanup filesystem: {}",
        io::Error::last_os_error()
    );
    let end = filesystem
        .iter()
        .position(|unit| *unit == 0)
        .context("Unbounded tag cleanup filesystem")?;
    ensure!(
        String::from_utf16(&filesystem[..end])? == "NTFS",
        "UnsupportedCapability: exact tag cleanup requires NTFS calibration"
    );
    let mut text = [0u16; 32768];
    let length = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            text.as_mut_ptr(),
            text.len() as u32,
            FILE_NAME_NORMALIZED | VOLUME_NAME_GUID,
        )
    } as usize;
    ensure!(
        length > 0 && length < text.len(),
        "Cannot read actual tag cleanup volume GUID"
    );
    let value = String::from_utf16(&text[..length])?;
    ensure!(
        value.starts_with("\\\\?\\Volume{"),
        "Tag cleanup volume is not native"
    );
    let end = value
        .find("}\\")
        .context("Malformed actual tag cleanup volume")?
        + 2;
    Ok(value[..end].to_ascii_lowercase())
}
#[cfg(windows)]
fn identifier(identity: &FileIdentity) -> Result<[u8; 16]> {
    ensure!(
        identity.kind == "windows_file_id"
            && identity.file_id.len() == 32
            && identity
                .file_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()),
        "Tag cleanup requires actual full FileID"
    );
    let mut bytes = [0; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&identity.file_id[index * 2..index * 2 + 2], 16)?;
    }
    ensure!(
        bytes[8..].iter().all(|byte| *byte == 0),
        "UnsupportedCapability: tag FileID layout is not calibrated"
    );
    Ok(bytes)
}
#[cfg(windows)]
fn lookup(parent: &File, identity: &FileIdentity, length: u16) -> std::result::Result<File, i32> {
    let bytes = identifier(identity).map_err(|_| 0xc000000du32 as i32)?;
    let mut buffer = [0u16; 8];
    for (index, unit) in buffer.iter_mut().enumerate() {
        *unit = u16::from_le_bytes([bytes[index * 2], bytes[index * 2 + 1]]);
    }
    let name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: buffer.as_mut_ptr(),
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
            FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            &attributes,
            &mut status_block,
            std::ptr::null(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            FILE_OPEN_BY_FILE_ID | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        return Err(status);
    }
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(0xc0000008u32 as i32);
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}
#[cfg(windows)]
pub(super) fn disposition(file: &File) -> Result<()> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    ensure!(
        unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfo,
                &info as *const _ as *const _,
                std::mem::size_of_val(&info) as u32,
            )
        } != 0,
        "Actual same-handle tag disposition failed: {}",
        io::Error::last_os_error()
    );
    Ok(())
}
#[cfg(unix)]
pub(super) fn disposition(file: &File) -> Result<()> {
    use std::os::unix::{ffi::OsStrExt, io::AsRawFd};
    let path = raw_actual_path(file)?;
    let pins = pin_directory(path.parent().context("Tag artifact parent missing")?)?;
    let parent = pins.last().context("Tag artifact parent unpinned")?;
    let name = std::ffi::CString::new(
        path.file_name()
            .context("Tag artifact name missing")?
            .as_bytes(),
    )?;
    let named = File::open(&path)?;
    ensure!(
        raw_actual_identity(&named)? == raw_actual_identity(file)? && raw_links(file)? == 1,
        "Tag artifact changed or gained a hardlink before unlink"
    );
    ensure!(
        unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } == 0,
        "Native tag unlink failed: {}",
        io::Error::last_os_error()
    );
    ensure!(
        raw_links(file)? == 0,
        "Tag artifact namespace changed during unlink"
    );
    parent.sync_all()?;
    Ok(())
}
#[cfg(windows)]
pub(super) fn zero_links(file: &File, expected: &FileIdentity) -> Result<()> {
    let mut info = FILE_STANDARD_INFO::default();
    ensure!(
        unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileStandardInfo,
                &mut info as *mut _ as *mut _,
                std::mem::size_of_val(&info) as u32,
            )
        } != 0,
        "Cannot inspect actual tag disposition: {}",
        io::Error::last_os_error()
    );
    ensure!(
        info.DeletePending && info.NumberOfLinks == 0 && actual_identity(file)? == *expected,
        "Actual tag disposition lacks same-FH pending zero-link evidence"
    );
    Ok(())
}
#[cfg(unix)]
pub(super) fn zero_links(file: &File, expected: &FileIdentity) -> Result<()> {
    ensure!(
        &raw_actual_identity(file)? == expected && raw_links(file)? == 0,
        "Tag unlink has no same-descriptor zero-link witness"
    );
    Ok(())
}

impl Calibration {
    pub(super) fn has_live_handles(&self) -> bool {
        self.probe.is_some()
            || self.reader.is_some()
            || self.controls.iter().any(Option::is_some)
            || {
                #[cfg(unix)]
                {
                    self.unix_object.is_some()
                }
                #[cfg(not(unix))]
                {
                    false
                }
            }
    }
    #[cfg(windows)]
    fn scope(&self, parent: &File) -> Result<()> {
        ensure!(
            parent.metadata()?.is_dir()
                && self.directory.as_ref() == Some(&actual_identity(parent)?)
                && self.volume.as_ref() == Some(&volume(parent)?),
            "Actual tag cleanup scope changed"
        );
        Ok(())
    }
    #[cfg(windows)]
    pub(super) fn present(&mut self, parent: &File, expected: &FileIdentity) -> Result<()> {
        self.scope(parent)?;
        identifier(expected)?;
        for control in &mut self.controls {
            close(control)?;
        }
        self.controls.clear();
        for length in [8, 16] {
            let file = lookup(parent, expected, length).map_err(|status| {
                io::Error::from_raw_os_error(unsafe {
                    windows_sys::Win32::Foundation::RtlNtStatusToDosError(status)
                } as i32)
            })?;
            self.controls.push(Some(file));
            let file = self.controls.last().unwrap().as_ref().unwrap();
            ensure!(
                actual_identity(file)? == *expected && self.volume.as_ref() == Some(&volume(file)?),
                "Tag cleanup positive control differs from its exact FileID/volume"
            );
            close(self.controls.last_mut().unwrap())?;
        }
        self.controls.clear();
        self.scope(parent)
    }
    #[cfg(windows)]
    pub(super) fn status(
        &mut self,
        parent: &File,
        expected: &FileIdentity,
        pending: bool,
    ) -> Result<()> {
        self.scope(parent)?;
        identifier(expected)?;
        let expected_status = if pending {
            0xc0000056u32 as i32
        } else {
            0xc000000du32 as i32
        };
        for length in [8, 16] {
            match lookup(parent, expected, length) {
                Err(actual) if actual == expected_status => {}
                Err(actual) => {
                    anyhow::bail!("Exact tag FileID cleanup status differs: {actual:#010x}")
                }
                Ok(file) => {
                    self.controls.push(Some(file));
                    close(self.controls.last_mut().unwrap())?;
                    anyhow::bail!(
                        "Actual tag FileID still exists, including aliases or unfinished readers"
                    );
                }
            }
        }
        self.scope(parent)
    }
    #[cfg(windows)]
    pub(super) fn prepare(&mut self, parent: &File) -> Result<()> {
        if self.volume.is_none() {
            self.volume = Some(volume(parent)?);
            self.directory = Some(actual_identity(parent)?);
        }
        self.scope(parent)?;
        if self.complete {
            return Ok(());
        }
        let root = self.directory.clone().unwrap();
        self.present(parent, &root)?;
        if self.probe_identity.is_none() && self.probe.is_none() {
            let mut bytes = [0; 16];
            getrandom::fill(&mut bytes).map_err(|error| {
                anyhow::anyhow!("Cannot allocate tag cleanup calibration nonce: {error}")
            })?;
            let nonce: String = bytes.iter().map(|value| format!("{value:02x}")).collect();
            self.probe = Some(create_owned_file(
                &actual_path(parent)?.join(format!(".lx-tag-cleanup-{nonce}.tmp")),
            )?);
        }
        if self.probe_identity.is_none() {
            self.probe_identity = Some(actual_identity(self.probe.as_ref().unwrap())?);
        }
        let expected = self.probe_identity.clone().unwrap();
        if !self.probe_pending {
            let probe = self
                .probe
                .as_ref()
                .context("Tag cleanup calibration owner is missing")?;
            ensure!(
                probe.metadata()?.len() == 0
                    && links(probe)? == 1
                    && expected.volume == root.volume,
                "Actual calibration probe is not its empty single-link owner"
            );
            self.present(parent, &expected)?;
            if self.reader.is_none() {
                self.reader = Some(lookup(parent, &expected, 16).map_err(|status| {
                    io::Error::from_raw_os_error(unsafe {
                        windows_sys::Win32::Foundation::RtlNtStatusToDosError(status)
                    } as i32)
                })?);
            }
            disposition(self.probe.as_ref().unwrap())?;
            self.probe_pending = true;
        }
        if let Some(probe) = &self.probe {
            zero_links(probe, &expected)?;
            self.status(parent, &expected, true)?;
            close(&mut self.probe)?;
        }
        if let Some(reader) = &self.reader {
            zero_links(reader, &expected)?;
            self.status(parent, &expected, true)?;
            close(&mut self.reader)?;
        }
        self.status(parent, &expected, false)?;
        self.present(parent, &root)?;
        self.complete = true;
        Ok(())
    }
    /// Absence is compared with this fresh same-volume calibration and bracketed
    /// by successful exact-ID directory controls. A missing name is not queried.
    #[cfg(windows)]
    pub(super) fn absent(&mut self, parent: &File, expected: &FileIdentity) -> Result<bool> {
        self.prepare(parent)?;
        identifier(expected)?;
        let directory = self
            .directory
            .clone()
            .context("Tag calibration directory is absent")?;
        ensure!(
            expected.volume == directory.volume,
            "Cold tag retirement FileID belongs to another volume"
        );
        self.present(parent, &directory)?;
        let mut states = Vec::with_capacity(2);
        for length in [8, 16] {
            match lookup(parent, expected, length) {
                Err(status) if status == 0xc000000du32 as i32 => states.push(true),
                Err(status) => anyhow::bail!(
                    "Cold exact tag FileID status differs from calibrated absence: {status:#010x}"
                ),
                Ok(file) => {
                    self.controls.push(Some(file));
                    let control = self.controls.last().unwrap().as_ref().unwrap();
                    ensure!(
                        actual_identity(control)? == *expected
                            && self.volume.as_ref() == Some(&volume(control)?),
                        "Cold tag positive FileID control differs from the selected object"
                    );
                    close(self.controls.last_mut().unwrap())?;
                    states.push(false);
                }
            }
        }
        self.controls.clear();
        self.present(parent, &directory)?;
        ensure!(
            states[0] == states[1],
            "Cold tag FileID forms disagree about actual absence"
        );
        Ok(states[0])
    }
    #[cfg(unix)]
    pub(super) fn capture(
        &mut self,
        parent: &File,
        file: &File,
        expected: &FileIdentity,
    ) -> Result<()> {
        self.prepare(parent)?;
        ensure!(
            &raw_actual_identity(file)? == expected
                && raw_links(file)? == 1
                && raw_actual_identity(parent)?.volume == expected.volume,
            "Tag retirement requires its actual same-volume owned inode"
        );
        ensure!(
            self.unix_object.is_none(),
            "Previous tag unlink descriptor remains retained"
        );
        self.unix_object = Some(file.try_clone()?);
        Ok(())
    }
    #[cfg(unix)]
    pub(super) fn absent(&mut self, _parent: &File, _expected: &FileIdentity) -> Result<bool> {
        anyhow::bail!(
            "Unix cold tag retirement requires a durable zero-link witness; missing names are insufficient"
        )
    }
    #[cfg(unix)]
    pub(super) fn prepare(&mut self, parent: &File) -> Result<()> {
        ensure!(
            parent.metadata()?.is_dir(),
            "Tag retirement parent is not a directory"
        );
        let identity = raw_actual_identity(parent)?;
        if let Some(expected) = &self.directory {
            ensure!(
                &identity == expected,
                "Tag retirement directory identity changed"
            );
        } else {
            self.directory = Some(identity);
        }
        Ok(())
    }
    #[cfg(unix)]
    pub(super) fn present(&mut self, parent: &File, expected: &FileIdentity) -> Result<()> {
        self.prepare(parent)?;
        if self.directory.as_ref() == Some(expected) {
            return Ok(());
        }
        let file = self
            .unix_object
            .as_ref()
            .context("Tag retirement has no actual retained inode")?;
        ensure!(
            &raw_actual_identity(file)? == expected && raw_links(file)? == 1,
            "Actual tag retirement inode changed before unlink"
        );
        Ok(())
    }
    #[cfg(unix)]
    pub(super) fn status(
        &mut self,
        parent: &File,
        expected: &FileIdentity,
        pending: bool,
    ) -> Result<()> {
        self.prepare(parent)?;
        zero_links(
            self.unix_object
                .as_ref()
                .context("Tag retirement has no held unlink witness")?,
            expected,
        )?;
        if !pending {
            close(&mut self.unix_object)?;
        }
        Ok(())
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_PROTECT_FROM_CLOSE, SetHandleInformation};
    fn fixture() -> (tempfile::TempDir, Vec<File>, Option<File>) {
        let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let pins = pin_directory(temp.path()).unwrap();
        let mut file = create_owned_file(&temp.path().join("owned-native-test.part")).unwrap();
        file.write_all(b"native-owned-body").unwrap();
        file.sync_all().unwrap();
        (temp, pins, Some(file))
    }
    fn original_fixture() -> (
        tempfile::TempDir,
        TagWriteSelection,
        TagOriginalRecoveryIntent,
    ) {
        let (temp, pins, mut file) = fixture();
        settle_write_time(file.as_ref().unwrap()).unwrap();
        let observation = observe_file(file.as_ref().unwrap()).unwrap();
        let sha256 = hash_body(
            file.as_mut().unwrap(),
            &ScanContext::new(Duration::from_secs(30)),
        )
        .unwrap();
        let selection = TagWriteSelection {
            operation_id: "actual-original-calibration".into(),
            asset_id: "actual-asset".into(),
            path: observation.path.clone(),
            expected_identity: observation.identity.clone(),
            root_id: None,
            grant_epoch: 1,
            expected_binding_revision: 1,
            expected_body_sha256: Some(sha256.clone()),
        };
        let facts = TagInstalledFacts {
            before: observation.clone(),
            after: observation.clone(),
            metadata: TagMetadataSnapshot {
                format: TagWriteFormat::Mp3,
                title: None,
                artists: Vec::new(),
                album: None,
                lyrics: None,
            },
            format: TagWriteFormat::Mp3,
            old_body_sha256: sha256.clone(),
            new_body_sha256: sha256.clone(),
            audio_sha256: sha256,
        };
        close(&mut file).unwrap();
        drop(pins);
        (
            temp,
            selection,
            TagOriginalRecoveryIntent { observation, facts },
        )
    }
    #[test]
    fn actual_original_control_failure_retains_same_descriptor_until_checked_cleanup() {
        let (_temp, selection, intent) = original_fixture();
        let mut calibration = OriginalCalibration::default();
        let checks = std::sync::atomic::AtomicUsize::new(0);
        let error = calibration.prove(
            &selection,
            &intent,
            &ScanContext::new(Duration::from_secs(30)),
            &|| {
                if checks.fetch_add(1, std::sync::atomic::Ordering::AcqRel) == 1 {
                    anyhow::bail!("actual owner fence changed before control close");
                }
                Ok(())
            },
        );
        assert!(error.is_err());
        let raw = calibration.control.as_ref().unwrap().as_raw_handle();
        assert_ne!(
            unsafe {
                SetHandleInformation(
                    raw,
                    HANDLE_FLAG_PROTECT_FROM_CLOSE,
                    HANDLE_FLAG_PROTECT_FROM_CLOSE,
                )
            },
            0
        );
        assert!(calibration.close().is_err());
        assert!(calibration.has_live_handles());
        assert_eq!(calibration.control.as_ref().unwrap().as_raw_handle(), raw);
        assert_ne!(
            unsafe { SetHandleInformation(raw, HANDLE_FLAG_PROTECT_FROM_CLOSE, 0) },
            0
        );
        calibration.close().unwrap();
        assert!(!calibration.has_live_handles());
    }
    #[test]
    fn actual_read_data_stability_lease_after_exclusive_close_denies_write_delete_and_rename() {
        let (_temp, selection, intent) = original_fixture();
        let mut calibration = OriginalCalibration::default();
        let (pin, sha256) = calibration
            .prove(
                &selection,
                &intent,
                &ScanContext::new(Duration::from_secs(30)),
                &|| Ok(()),
            )
            .unwrap();
        assert!(!calibration.has_live_handles());
        assert_eq!(sha256, intent.facts.new_body_sha256);
        assert_eq!(observe_file(&pin).unwrap(), intent.observation);
        assert!(
            OpenOptions::new()
                .write(true)
                .open(&selection.path)
                .is_err()
        );
        use std::os::windows::fs::OpenOptionsExt;
        assert!(
            OpenOptions::new()
                .access_mode(DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&selection.path)
                .is_err()
        );
        assert!(
            std::fs::rename(
                &selection.path,
                selection.path.with_extension("foreign-name")
            )
            .is_err()
        );
        assert_eq!(observe_file(&pin).unwrap(), intent.observation);
        let mut pin = Some(pin);
        close(&mut pin).unwrap();
        assert!(pin.is_none());
        assert_eq!(
            format!(
                "{:x}",
                Sha256::digest(std::fs::read(&selection.path).unwrap())
            ),
            sha256
        );
    }
    #[test]
    fn actual_protect_from_close_failure_retains_same_descriptor_until_native_retry() {
        let (temp, pins, mut file) = fixture();
        let identity = actual_identity(file.as_ref().unwrap()).unwrap();
        let raw = file.as_ref().unwrap().as_raw_handle();
        assert_ne!(
            unsafe {
                SetHandleInformation(
                    raw,
                    HANDLE_FLAG_PROTECT_FROM_CLOSE,
                    HANDLE_FLAG_PROTECT_FROM_CLOSE,
                )
            },
            0
        );
        assert!(close(&mut file).is_err());
        assert_eq!(file.as_ref().unwrap().as_raw_handle(), raw);
        assert_eq!(actual_identity(file.as_ref().unwrap()).unwrap(), identity);
        assert!(File::open(temp.path().join("owned-native-test.part")).is_err());
        assert_ne!(
            unsafe { SetHandleInformation(raw, HANDLE_FLAG_PROTECT_FROM_CLOSE, 0) },
            0
        );
        close(&mut file).unwrap();
        assert!(file.is_none());
        assert_eq!(
            std::fs::read(temp.path().join("owned-native-test.part")).unwrap(),
            b"native-owned-body"
        );
        drop(pins);
    }
    #[test]
    fn actual_readonly_disposition_error_leaves_owned_file_unchanged_then_retires() {
        let (_temp, pins, mut file) = fixture();
        let parent = pins.last().unwrap();
        let expected = actual_identity(file.as_ref().unwrap()).unwrap();
        let mut info = FILE_BASIC_INFO {
            FileAttributes: FILE_ATTRIBUTE_READONLY,
            ..Default::default()
        };
        assert_ne!(
            unsafe {
                SetFileInformationByHandle(
                    file.as_ref().unwrap().as_raw_handle(),
                    FileBasicInfo,
                    &info as *const _ as *const _,
                    std::mem::size_of_val(&info) as u32,
                )
            },
            0
        );
        assert!(disposition(file.as_ref().unwrap()).is_err());
        assert_eq!(links(file.as_ref().unwrap()).unwrap(), 1);
        assert_eq!(actual_identity(file.as_ref().unwrap()).unwrap(), expected);
        info.FileAttributes = FILE_ATTRIBUTE_NORMAL;
        assert_ne!(
            unsafe {
                SetFileInformationByHandle(
                    file.as_ref().unwrap().as_raw_handle(),
                    FileBasicInfo,
                    &info as *const _ as *const _,
                    std::mem::size_of_val(&info) as u32,
                )
            },
            0
        );
        let mut calibration = Calibration::default();
        calibration.prepare(parent).unwrap();
        calibration.present(parent, &expected).unwrap();
        disposition(file.as_ref().unwrap()).unwrap();
        zero_links(file.as_ref().unwrap(), &expected).unwrap();
        calibration.status(parent, &expected, true).unwrap();
        close(&mut file).unwrap();
        calibration.status(parent, &expected, false).unwrap();
        assert!(!calibration.has_live_handles());
    }
    #[test]
    fn actual_extra_reader_keeps_exact_fileid_pending_after_owner_close() {
        let (_temp, pins, mut file) = fixture();
        let parent = pins.last().unwrap();
        let expected = actual_identity(file.as_ref().unwrap()).unwrap();
        let mut calibration = Calibration::default();
        calibration.prepare(parent).unwrap();
        calibration.present(parent, &expected).unwrap();
        let mut reader = Some(lookup(parent, &expected, 16).unwrap());
        disposition(file.as_ref().unwrap()).unwrap();
        zero_links(file.as_ref().unwrap(), &expected).unwrap();
        close(&mut file).unwrap();
        assert!(calibration.status(parent, &expected, false).is_err());
        calibration.status(parent, &expected, true).unwrap();
        zero_links(reader.as_ref().unwrap(), &expected).unwrap();
        close(&mut reader).unwrap();
        calibration.status(parent, &expected, false).unwrap();
        assert!(!calibration.has_live_handles());
    }
}
