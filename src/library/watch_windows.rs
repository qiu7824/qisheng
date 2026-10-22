//! Immutable-volume notifications are filtered before allocating root hints.
use super::*;
use crate::library::authority::{RootAuthority, RootProof};
use anyhow::Context as _;
use std::os::windows::{
    ffi::OsStrExt,
    io::{AsRawHandle, FromRawHandle},
};
use std::{collections::BTreeSet, fs::File, io, ptr, time::Instant};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_IO_PENDING, ERROR_NOTIFY_ENUM_DIR, GetLastError, HANDLE,
        INVALID_HANDLE_VALUE,
    },
    Storage::FileSystem::*,
    System::IO::{CancelIoEx, CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED},
};
const BUFFER_BYTES: usize = 64 * 1024;
struct Port(HANDLE);
impl Drop for Port {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct CompletionKey {
    slot: usize,
    generation: u64,
}
struct Sensor {
    volume: Vec<u16>,
    file: File,
    buffer: Box<[u32; BUFFER_BYTES / 4]>,
    overlapped: Box<OVERLAPPED>,
    key: Box<CompletionKey>,
    pending: bool,
    cancelling: bool,
    subscribers: BTreeSet<usize>,
    waiters: BTreeSet<usize>,
    _reservation: Credit,
}
impl Sensor {
    fn new(
        volume: Vec<u16>,
        serial: &str,
        port: HANDLE,
        slot: usize,
        generation: u64,
        reservation: Credit,
    ) -> Result<Box<Self>> {
        // This GUID comes from a selected root FH and names an existing volume.
        let name: Vec<u16> = volume.iter().copied().chain(Some(0)).collect();
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED | FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        ensure!(
            handle != INVALID_HANDLE_VALUE,
            "Cannot open native volume sensor: {}",
            io::Error::last_os_error()
        );
        let file = unsafe { File::from_raw_handle(handle) };
        validate_directory(&file)?;
        ensure!(
            identity(&file)?.volume == serial,
            "Native sensor differs from its selected volume"
        );
        let key = Box::new(CompletionKey { slot, generation });
        ensure!(
            unsafe {
                CreateIoCompletionPort(
                    handle,
                    port,
                    key.as_ref() as *const CompletionKey as usize,
                    0,
                )
            } == port,
            "Cannot associate native IOCP: {}",
            io::Error::last_os_error()
        );
        let mut sensor = Box::new(Self {
            volume,
            file,
            buffer: Box::new([0; BUFFER_BYTES / 4]),
            overlapped: Box::default(),
            key,
            pending: false,
            cancelling: false,
            subscribers: BTreeSet::new(),
            waiters: BTreeSet::new(),
            _reservation: reservation,
        });
        sensor.issue()?;
        Ok(sensor)
    }
    fn key_value(&self) -> usize {
        self.key.as_ref() as *const CompletionKey as usize
    }
    fn issue(&mut self) -> Result<()> {
        ensure!(!self.pending, "Native sensor already has pending I/O");
        *self.overlapped = OVERLAPPED::default();
        self.buffer.fill(0);
        let success = unsafe {
            ReadDirectoryChangesW(
                self.file.as_raw_handle(),
                self.buffer.as_mut_ptr().cast(),
                BUFFER_BYTES as u32,
                1,
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_DIR_NAME
                    | FILE_NOTIFY_CHANGE_SIZE
                    | FILE_NOTIFY_CHANGE_LAST_WRITE
                    | FILE_NOTIFY_CHANGE_ATTRIBUTES
                    | FILE_NOTIFY_CHANGE_CREATION,
                ptr::null_mut(),
                self.overlapped.as_mut(),
                None,
            )
        };
        if success == 0 {
            let error = unsafe { GetLastError() };
            ensure!(
                error == ERROR_IO_PENDING,
                "Cannot issue native volume notifications: {}",
                io::Error::from_raw_os_error(error as i32)
            );
        }
        self.pending = true;
        Ok(())
    }
    fn cancel(&mut self) -> Result<()> {
        if self.cancelling {
            return Ok(());
        }
        self.cancelling = true;
        if self.pending {
            let success =
                unsafe { CancelIoEx(self.file.as_raw_handle(), self.overlapped.as_ref()) };
            let error = unsafe { GetLastError() };
            ensure!(
                success != 0 || error == windows_sys::Win32::Foundation::ERROR_NOT_FOUND,
                "Native cancellation failed: {}",
                io::Error::from_raw_os_error(error as i32)
            );
        }
        Ok(())
    }
}
struct Root {
    record: LibraryRoot,
    token: LibraryWatchToken,
    authority: RootAuthority,
    _proof: RootProof,
    file: File,
    dirty: Arc<DirtyState>,
    _slot: Credit,
    sensor: usize,
    active: bool,
    retiring: bool,
    prepare_credit: Option<Credit>,
    stops: Vec<Credit>,
    last_path: PathBuf,
    prefix: Vec<u16>,
}
impl Root {
    fn prepare(
        record: LibraryRoot,
        token: LibraryWatchToken,
        authority: &AuthorityRegistry,
        dirty: Arc<DirtyState>,
        slot: Credit,
    ) -> Result<(Self, Vec<u16>)> {
        let verified = authority.watch_authority(&record)?;
        let proof = verified.proof()?;
        let name: Vec<u16> = record
            .path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        ensure!(
            handle != INVALID_HANDLE_VALUE,
            "Cannot open selected native root: {}",
            io::Error::last_os_error()
        );
        let named = unsafe { File::from_raw_handle(handle) };
        validate_directory(&named)?;
        ensure!(
            identity(&named)? == record.anchor && current_path(&named)? == record.path,
            "NeedsReapproval: selected root changed"
        );
        let file = reopen_by_id(&named)?;
        drop(named);
        verified.check()?;
        let (volume, prefix) = volume_path(&file)?;
        Ok((
            Self {
                last_path: record.path.clone(),
                record,
                token,
                authority: verified,
                _proof: proof,
                file,
                dirty,
                _slot: slot,
                sensor: usize::MAX,
                active: false,
                retiring: false,
                prepare_credit: None,
                stops: Vec::new(),
                prefix,
            },
            volume,
        ))
    }
    fn refresh(&mut self, volume: &[u16], output: &mut Output) -> Result<bool> {
        self.authority.check()?;
        validate_directory(&self.file)?;
        ensure!(
            identity(&self.file)? == self.record.anchor,
            "NeedsReapproval: root anchor changed"
        );
        let path = current_path(&self.file)?;
        let (actual_volume, prefix) = volume_path(&self.file)?;
        ensure!(
            actual_volume == volume,
            "NeedsReapproval: root changed volumes"
        );
        self.prefix = prefix;
        if path != self.last_path {
            self.last_path = path.clone();
            output.dirty_cause(
                &self.dirty,
                Vec::new(),
                true,
                self.active,
                NativeDirtyReason::RootMoved,
            );
            output.control(
                NativeWatchEvent::RootMoved {
                    token: self.token.clone(),
                    current_path: path,
                    anchor: self.record.anchor.clone(),
                },
                None,
            );
            if self.token.options.root_move_policy
                == crate::domain::library_watch::RootMovePolicy::Pause
            {
                self.dirty.fail(&LibraryError::new(
                    LibraryErrorKind::Unavailable,
                    "Native watch root moved",
                ));
                return Ok(false);
            }
        }
        Ok(true)
    }
}
struct Owner {
    // Native buffers/FH are destroyed before selected-root reservation credits.
    sensors: Vec<Option<Box<Sensor>>>,
    roots: Vec<Option<Root>>,
    port: Port,
    shared: Arc<Shared>,
    generation: u64,
}
struct Preparation {
    record: LibraryRoot,
    token: LibraryWatchToken,
    dirty: Arc<DirtyState>,
    slot: Credit,
    credit: Credit,
}
impl Owner {
    fn new(shared: Arc<Shared>) -> Result<Self> {
        let port =
            Port(unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, ptr::null_mut(), 0, 1) });
        ensure!(
            !port.0.is_null(),
            "Cannot create native IOCP: {}",
            io::Error::last_os_error()
        );
        Ok(Self {
            sensors: (0..MAX_CONTEXTS).map(|_| None).collect(),
            roots: (0..MAX_CONTEXTS).map(|_| None).collect(),
            port,
            shared,
            generation: 0,
        })
    }
    fn prepare(
        &mut self,
        preparation: Preparation,
        authority: &AuthorityRegistry,
        output: &mut Output,
    ) {
        let Preparation {
            record,
            token,
            dirty,
            slot,
            credit,
        } = preparation;
        let failure_token = token.clone();
        let (mut root, volume) = match Root::prepare(record, token, authority, dirty, slot) {
            Ok(value) => value,
            Err(error) => {
                output.forget(&failure_token.root_id, failure_token.watch_generation);
                output.control(
                    NativeWatchEvent::Prepared {
                        token: failure_token,
                        result: Err(error),
                    },
                    Some(credit),
                );
                return;
            }
        };
        root.prepare_credit = Some(credit);
        let Some(index) = self.roots.iter().position(Option::is_none) else {
            let credit = root.prepare_credit.take();
            output.forget(&root.token.root_id, root.token.watch_generation);
            output.control(
                NativeWatchEvent::Prepared {
                    token: root.token,
                    result: Err(LibraryError::budget("Native root capacity exhausted").into()),
                },
                credit,
            );
            return;
        };
        let existing = self.sensors.iter().position(|sensor| {
            sensor
                .as_ref()
                .is_some_and(|sensor| sensor.volume == volume)
        });
        let preparation = (|| -> Result<usize> {
            if let Some(index) = existing {
                return Ok(index);
            }
            let index = self
                .sensors
                .iter()
                .position(Option::is_none)
                .context("Native sensor capacity exhausted")?;
            let reservation = Credit::reserve(self.shared.kernel_count.clone(), MAX_CONTEXTS)?;
            self.generation = self
                .generation
                .checked_add(1)
                .context("Native sensor generation exhausted")?;
            self.sensors[index] = Some(Sensor::new(
                volume,
                &root.record.anchor.volume,
                self.port.0,
                index,
                self.generation,
                reservation,
            )?);
            Ok(index)
        })();
        match preparation {
            Ok(sensor_index) => {
                root.sensor = sensor_index;
                let sensor = self.sensors[sensor_index].as_mut().unwrap();
                sensor.subscribers.insert(index);
                if sensor.pending && !sensor.cancelling {
                    let credit = root.prepare_credit.take();
                    output.control(
                        NativeWatchEvent::Prepared {
                            token: root.token.clone(),
                            result: Ok(()),
                        },
                        credit,
                    );
                }
                self.roots[index] = Some(root);
            }
            Err(error) => {
                let credit = root.prepare_credit.take();
                output.forget(&root.token.root_id, root.token.watch_generation);
                output.control(
                    NativeWatchEvent::Prepared {
                        token: root.token,
                        result: Err(error),
                    },
                    credit,
                );
            }
        }
    }
    fn cancel_root(&mut self, index: usize, output: &mut Output) {
        let Some(root) = self.roots[index].as_mut() else {
            return;
        };
        if root.retiring {
            return;
        }
        root.retiring = true;
        root.active = false;
        let Some(sensor) = self.sensors[root.sensor].as_mut() else {
            return;
        };
        sensor.subscribers.remove(&index);
        sensor.waiters.insert(index);
        if let Err(error) = sensor.cancel() {
            output.control(
                NativeWatchEvent::Error {
                    token: root.token.clone(),
                    error: watch_error(&error),
                },
                None,
            );
        }
    }
    fn fail_root(&mut self, index: usize, error: LibraryError, output: &mut Output) {
        if let Some(root) = self.roots[index].as_ref() {
            output.dirty_cause(
                &root.dirty,
                Vec::new(),
                true,
                root.active,
                NativeDirtyReason::SensorError,
            );
            output.control(
                NativeWatchEvent::Error {
                    token: root.token.clone(),
                    error,
                },
                None,
            );
        }
        self.cancel_root(index, output);
    }
    fn finish_barrier(&mut self, index: usize, output: &mut Output) {
        let Some(sensor) = self.sensors[index].as_mut() else {
            return;
        };
        if sensor.pending {
            return;
        }
        let waiters = std::mem::take(&mut sensor.waiters);
        sensor.cancelling = false;
        let subscribers: Vec<_> = sensor.subscribers.iter().copied().collect();
        let mut rearm_error = None;
        if subscribers.is_empty() {
            drop(self.sensors[index].take());
        } else {
            if let Err(error) = sensor.issue() {
                rearm_error = Some(watch_error(&error));
            }
            for subscriber in &subscribers {
                if let Some(root) = self.roots[*subscriber].as_mut()
                    && rearm_error.is_none()
                {
                    if let Some(credit) = root.prepare_credit.take() {
                        output.control(
                            NativeWatchEvent::Prepared {
                                token: root.token.clone(),
                                result: Ok(()),
                            },
                            Some(credit),
                        );
                    }
                    if !waiters.is_empty() {
                        output.dirty_cause(
                            &root.dirty,
                            Vec::new(),
                            true,
                            root.active,
                            NativeDirtyReason::SharedCancelGap,
                        );
                    }
                }
            }
        }
        for root_index in waiters {
            if let Some(mut root) = self.roots[root_index].take() {
                let token = root.token.clone();
                let stops = std::mem::take(&mut root.stops);
                if let Some(credit) = root.prepare_credit.take() {
                    output.control(
                        NativeWatchEvent::Prepared {
                            token: token.clone(),
                            result: Err(LibraryError::new(
                                LibraryErrorKind::Cancelled,
                                "Native subscription stopped during preparation",
                            )
                            .into()),
                        },
                        Some(credit),
                    );
                }
                drop(root);
                if !stops.is_empty() || self.shared.closing.load(Ordering::Acquire) {
                    output.forget(&token.root_id, token.watch_generation);
                }
                for credit in stops {
                    output.control(
                        NativeWatchEvent::Stopped {
                            root_id: token.root_id.clone(),
                            watch_generation: token.watch_generation,
                        },
                        Some(credit),
                    );
                }
            }
        }
        if let Some(error) = rearm_error {
            for root in subscribers {
                self.fail_root(root, error.clone(), output);
            }
        }
    }
    fn refresh(&mut self, index: usize, output: &mut Output) {
        let Some(root) = self.roots[index].as_ref().filter(|root| !root.retiring) else {
            return;
        };
        let volume = self.sensors[root.sensor].as_ref().unwrap().volume.clone();
        match self.roots[index].as_mut().unwrap().refresh(&volume, output) {
            Ok(true) => {}
            Ok(false) => self.cancel_root(index, output),
            Err(error) => self.fail_root(index, watch_error(&error), output),
        }
    }
    fn packet(&mut self, index: usize, bytes: usize, output: &mut Output) {
        let subscribers: Vec<_> = self.sensors[index]
            .as_ref()
            .unwrap()
            .subscribers
            .iter()
            .copied()
            .collect();
        for root in &subscribers {
            self.refresh(*root, output);
        }
        let sensor = self.sensors[index].as_ref().unwrap();
        let mut batches: Vec<_> = subscribers
            .iter()
            .filter_map(|index| {
                self.roots[*index]
                    .as_ref()
                    .filter(|root| !root.retiring)
                    .map(|_| (*index, Vec::new(), false, false, 0usize))
            })
            .collect();
        // Outside names remain only borrowed UTF-16 spans in this 64 KiB packet.
        let result = walk_records(sensor.buffer.as_ref(), bytes, |name, kind| {
            for (index, hints, full, dropped, path_bytes) in &mut batches {
                let root = self.roots[*index].as_ref().unwrap();
                match selected_name(name, &root.prefix, root.record.recursive) {
                    Selection::Outside => {}
                    Selection::Root => *full = true,
                    Selection::Invalid => {
                        *full = true;
                        *dropped = true;
                    }
                    Selection::Relative(relative) => {
                        let path = match String::from_utf16(relative) {
                            Ok(name) => PathBuf::from(name),
                            Err(_) => {
                                *full = true;
                                *dropped = true;
                                continue;
                            }
                        };
                        if !safe_relative(&path)
                            || hints.len() >= MAX_HINTS
                            || *path_bytes + path.as_os_str().len() * 4 > MAX_HINT_BYTES
                        {
                            *full = true;
                            *dropped = true;
                            continue;
                        }
                        *path_bytes += path.as_os_str().len() * 4;
                        *full |= matches!(
                            kind,
                            NativeWatchKind::RenamedOld | NativeWatchKind::RenamedNew
                        );
                        hints.push(NativeWatchHint {
                            path,
                            kind,
                            is_directory: None,
                            rename_cookie: None,
                        });
                    }
                }
            }
        });
        if result.is_err() {
            #[cfg(test)]
            self.shared.native_gaps.fetch_add(1, Ordering::AcqRel);
        }
        for (index, hints, full, dropped, _) in batches {
            let root = self.roots[index].as_ref().unwrap();
            if result.is_err() || dropped {
                root.dirty.dropped.fetch_add(1, Ordering::AcqRel);
            }
            if result.is_err() || full || !hints.is_empty() {
                output.dirty(&root.dirty, hints, full || result.is_err(), root.active);
            }
        }
    }
    fn complete(
        &mut self,
        key: usize,
        overlapped: *mut OVERLAPPED,
        bytes: u32,
        error: u32,
        output: &mut Output,
    ) {
        let Some(index) = self.sensors.iter().position(|sensor| {
            sensor
                .as_ref()
                .is_some_and(|sensor| sensor.key_value() == key)
        }) else {
            return;
        };
        let sensor = self.sensors[index].as_mut().unwrap();
        if sensor.key.slot != index
            || sensor.key.generation == 0
            || !sensor.pending
            || !ptr::eq(overlapped, sensor.overlapped.as_ref())
        {
            return;
        }
        sensor.pending = false;
        #[cfg(test)]
        if sensor.cancelling {
            self.shared
                .cancelled_completions
                .fetch_add(1, Ordering::AcqRel);
        }
        #[cfg(test)]
        wait_completion_gate(&self.shared, sensor.cancelling);
        if !sensor.cancelling {
            if error == 0 {
                self.packet(index, bytes as usize, output);
            } else if error == ERROR_NOTIFY_ENUM_DIR {
                #[cfg(test)]
                self.shared.native_gaps.fetch_add(1, Ordering::AcqRel);
                let roots: Vec<_> = self.sensors[index]
                    .as_ref()
                    .unwrap()
                    .subscribers
                    .iter()
                    .copied()
                    .collect();
                for root in roots {
                    let root = self.roots[root].as_ref().unwrap();
                    root.dirty.dropped.fetch_add(1, Ordering::AcqRel);
                    output.dirty(&root.dirty, Vec::new(), true, root.active);
                }
            } else {
                let roots: Vec<_> = self.sensors[index]
                    .as_ref()
                    .unwrap()
                    .subscribers
                    .iter()
                    .copied()
                    .collect();
                for root in roots {
                    self.fail_root(
                        root,
                        watch_error(&anyhow::anyhow!(
                            "Native notifications failed: {}",
                            io::Error::from_raw_os_error(error as i32)
                        )),
                        output,
                    );
                }
            }
        }
        self.finish_barrier(index, output);
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        // This destructor runs only on the registered native thread, including unwind.
        for sensor in self
            .sensors
            .iter_mut()
            .flatten()
            .filter(|sensor| sensor.pending)
        {
            unsafe {
                CancelIoEx(sensor.file.as_raw_handle(), sensor.overlapped.as_ref());
            }
        }
        while self.sensors.iter().flatten().any(|sensor| sensor.pending) {
            let mut bytes = 0;
            let mut key = 0;
            let mut overlapped = ptr::null_mut();
            unsafe {
                GetQueuedCompletionStatus(self.port.0, &mut bytes, &mut key, &mut overlapped, 10);
            }
            if !overlapped.is_null()
                && let Some(sensor) = self
                    .sensors
                    .iter_mut()
                    .flatten()
                    .find(|sensor| sensor.key_value() == key)
                && ptr::eq(overlapped, sensor.overlapped.as_ref())
            {
                sensor.pending = false;
            }
        }
    }
}
pub(super) fn run(
    authority: AuthorityRegistry,
    commands: Receiver<Command>,
    shared: Arc<Shared>,
    output: &mut Output,
) -> Result<()> {
    let mut owner = Owner::new(shared.clone())?;
    let mut probe = Instant::now();
    loop {
        output.flush();
        for _ in 0..CONTROL_CAPACITY {
            let Ok(command) = commands.try_recv() else {
                break;
            };
            if shared.closing.load(Ordering::Acquire) {
                if let Command::Prepare { token, .. } = command {
                    output.forget(&token.root_id, token.watch_generation);
                }
                continue;
            }
            match command {
                Command::Prepare {
                    root,
                    token,
                    dirty,
                    slot,
                    credit,
                } => owner.prepare(
                    Preparation {
                        record: root,
                        token,
                        dirty,
                        slot,
                        credit,
                    },
                    &authority,
                    output,
                ),
                Command::Activate { token, credit } => {
                    if let Some(root) = owner.roots.iter_mut().flatten().find(|root| {
                        root.token == token && !root.retiring && root.prepare_credit.is_none()
                    }) {
                        root.active = true;
                        output.control(NativeWatchEvent::Watching { token }, Some(credit));
                        output.baseline(&root.dirty);
                    } else {
                        output.control(
                            NativeWatchEvent::Error {
                                token,
                                error: LibraryError::conflict(
                                    "Prepared native root is no longer subscribed",
                                ),
                            },
                            Some(credit),
                        );
                    }
                }
                Command::Stop {
                    root_id,
                    generation,
                    credit,
                } => {
                    if let Some(index) = owner.roots.iter().position(|root| {
                        root.as_ref().is_some_and(|root| {
                            root.token.root_id == root_id
                                && root.token.watch_generation == generation
                        })
                    }) {
                        owner.roots[index].as_mut().unwrap().stops.push(credit);
                        owner.cancel_root(index, output);
                    } else {
                        output.forget(&root_id, generation);
                        output.control(
                            NativeWatchEvent::Stopped {
                                root_id,
                                watch_generation: generation,
                            },
                            Some(credit),
                        );
                    }
                }
            }
        }
        if shared.closing.load(Ordering::Acquire) {
            for index in 0..MAX_CONTEXTS {
                owner.cancel_root(index, output);
            }
        }
        for index in 0..MAX_CONTEXTS {
            if owner.sensors[index]
                .as_ref()
                .is_some_and(|sensor| !sensor.pending && sensor.cancelling)
            {
                owner.finish_barrier(index, output);
            }
        }
        if shared.closing.load(Ordering::Acquire)
            && owner.roots.iter().all(Option::is_none)
            && owner.sensors.iter().all(Option::is_none)
        {
            break;
        }
        if probe.elapsed() >= Duration::from_secs(1) && !shared.closing.load(Ordering::Acquire) {
            probe = Instant::now();
            for index in 0..MAX_CONTEXTS {
                owner.refresh(index, output);
            }
        }
        let mut bytes = 0;
        let mut key = 0;
        let mut overlapped = ptr::null_mut();
        let success = unsafe {
            GetQueuedCompletionStatus(owner.port.0, &mut bytes, &mut key, &mut overlapped, 10)
        };
        let error = if success == 0 {
            unsafe { GetLastError() }
        } else {
            0
        };
        if !overlapped.is_null() {
            owner.complete(key, overlapped, bytes, error, output);
        }
    }
    while let Ok(command) = commands.try_recv() {
        if let Command::Prepare { token, .. } = command {
            output.forget(&token.root_id, token.watch_generation);
        }
    }
    Ok(())
}
fn validate_directory(file: &File) -> Result<()> {
    let mut attributes = FILE_ATTRIBUTE_TAG_INFO::default();
    ensure!(
        unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileAttributeTagInfo,
                &mut attributes as *mut _ as *mut _,
                std::mem::size_of_val(&attributes) as u32,
            )
        } != 0,
        "Cannot inspect native directory: {}",
        io::Error::last_os_error()
    );
    ensure!(
        attributes.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0
            && attributes.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "Native directory is not a regular directory"
    );
    let mut standard = FILE_STANDARD_INFO::default();
    ensure!(
        unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileStandardInfo,
                &mut standard as *mut _ as *mut _,
                std::mem::size_of_val(&standard) as u32,
            )
        } != 0
            && !standard.DeletePending,
        "Native directory is deleted or unavailable"
    );
    Ok(())
}
fn reopen_by_id(original: &File) -> Result<File> {
    let mut info = FILE_ID_INFO::default();
    ensure!(
        unsafe {
            GetFileInformationByHandleEx(
                original.as_raw_handle(),
                FileIdInfo,
                &mut info as *mut _ as *mut _,
                std::mem::size_of_val(&info) as u32,
            )
        } != 0,
        "Cannot identify selected root"
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
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )
    };
    ensure!(
        handle != INVALID_HANDLE_VALUE,
        "UnsupportedCapability: path-independent root proofs are unavailable: {}",
        io::Error::last_os_error()
    );
    let file = unsafe { File::from_raw_handle(handle) };
    validate_directory(&file)?;
    ensure!(
        identity(&file)? == identity(original)? && current_path(&file)? == current_path(original)?,
        "Selected root differs from its verified object"
    );
    Ok(file)
}
fn identity(file: &File) -> Result<FileIdentity> {
    let mut info = FILE_ID_INFO::default();
    ensure!(
        unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                &mut info as *mut _ as *mut _,
                std::mem::size_of_val(&info) as u32,
            )
        } != 0,
        "Cannot inspect native identity: {}",
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
fn final_name(file: &File, flags: u32) -> Result<Vec<u16>> {
    let mut buffer = vec![0u16; 16 * 1024 + 1];
    let length = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            FILE_NAME_NORMALIZED | flags,
        )
    } as usize;
    ensure!(
        length > 0 && length < buffer.len(),
        "Cannot obtain bounded native location: {}",
        io::Error::last_os_error()
    );
    buffer.truncate(length);
    Ok(buffer)
}
fn current_path(file: &File) -> Result<PathBuf> {
    Ok(crate::domain::music::normalize_local_path(&PathBuf::from(
        String::from_utf16(&final_name(file, VOLUME_NAME_DOS)?)?,
    )))
}
fn volume_path(file: &File) -> Result<(Vec<u16>, Vec<u16>)> {
    let path = String::from_utf16(&final_name(file, VOLUME_NAME_GUID)?)?;
    ensure!(
        path.starts_with("\\\\?\\Volume{"),
        "UnsupportedCapability: immutable volume paths are unavailable"
    );
    let end = path
        .find("}\\")
        .context("Native volume GUID terminator is missing")?
        + 2;
    Ok((
        path[..end].encode_utf16().collect(),
        path[end..].encode_utf16().collect(),
    ))
}
enum Selection<'a> {
    Outside,
    Root,
    Invalid,
    Relative(&'a [u16]),
}
fn selected_name<'a>(name: &'a [u16], prefix: &[u16], recursive: bool) -> Selection<'a> {
    let fold = |unit: u16| {
        if (b'a' as u16..=b'z' as u16).contains(&unit) {
            unit - 32
        } else {
            unit
        }
    };
    if name.len() < prefix.len() || !name.iter().zip(prefix).all(|(a, b)| fold(*a) == fold(*b)) {
        return Selection::Outside;
    }
    let relative = if prefix.is_empty() {
        name
    } else {
        if name.len() == prefix.len() {
            return Selection::Root;
        }
        if name[prefix.len()] != b'\\' as u16 {
            return Selection::Outside;
        }
        &name[prefix.len() + 1..]
    };
    if relative.is_empty() {
        return Selection::Root;
    }
    if !recursive
        && relative
            .iter()
            .any(|unit| *unit == b'\\' as u16 || *unit == b'/' as u16)
    {
        return Selection::Outside;
    }
    if relative.contains(&0) {
        return Selection::Invalid;
    }
    Selection::Relative(relative)
}
fn walk_records(
    buffer: &[u32],
    length: usize,
    mut accept: impl FnMut(&[u16], NativeWatchKind),
) -> Result<()> {
    ensure!(
        length > 0 && length <= std::mem::size_of_val(buffer),
        "Native notification gap"
    );
    let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), length) };
    let mut position = 0usize;
    loop {
        ensure!(position + 12 <= bytes.len(), "Invalid notification header");
        let word = |at| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let next = word(position) as usize;
        let action = word(position + 4);
        let names = word(position + 8) as usize;
        ensure!(
            names != 0
                && names.is_multiple_of(2)
                && names <= 32 * 1024
                && position + 12 + names <= bytes.len()
                && (next == 0
                    || (next.is_multiple_of(4)
                        && next >= 12 + names
                        && position + next <= bytes.len())),
            "Invalid notification offset"
        );
        let kind = match action {
            FILE_ACTION_ADDED => NativeWatchKind::Created,
            FILE_ACTION_REMOVED => NativeWatchKind::Removed,
            FILE_ACTION_MODIFIED => NativeWatchKind::Modified,
            FILE_ACTION_RENAMED_OLD_NAME => NativeWatchKind::RenamedOld,
            FILE_ACTION_RENAMED_NEW_NAME => NativeWatchKind::RenamedNew,
            _ => anyhow::bail!("Invalid notification action"),
        };
        let name = unsafe {
            std::slice::from_raw_parts(bytes.as_ptr().add(position + 12).cast::<u16>(), names / 2)
        };
        accept(name, kind);
        if next == 0 {
            return Ok(());
        }
        position += next;
    }
}
#[cfg(test)]
fn wait_completion_gate(shared: &Shared, cancelled: bool) {
    let gate = {
        shared
            .gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    };
    if let Some(gate) = gate {
        if gate.cancel_only.load(Ordering::Acquire) && !cancelled {
            return;
        }
        gate.entered.store(true, Ordering::Release);
        while !gate.release.load(Ordering::Acquire) {
            std::thread::park_timeout(Duration::from_millis(1));
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn record(name: &[u16], next: u32) -> (Vec<u32>, usize) {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&next.to_le_bytes());
        bytes.extend_from_slice(&FILE_ACTION_ADDED.to_le_bytes());
        bytes.extend_from_slice(&(name.len() as u32 * 2).to_le_bytes());
        for unit in name {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let actual = bytes.len();
        bytes.resize(actual.div_ceil(4) * 4, 0);
        (
            bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|p| u32::from_le_bytes(*p))
                .collect(),
            actual,
        )
    }
    #[test]
    fn raw_packets_allocate_only_selected_relative_names_and_reject_broken_offsets() {
        let prefix: Vec<_> = "selected\\root".encode_utf16().collect();
        for text in [
            "private\\other.wav",
            "selected\\root-sibling\\other.wav",
            "selected\\root\\child\\other.wav",
        ] {
            let name: Vec<_> = text.encode_utf16().collect();
            assert!(matches!(
                selected_name(&name, &prefix, false),
                Selection::Outside
            ));
        }
        assert!(walk_records(&[0; 3], 0, |_, _| panic!("Invalid packet forwarded")).is_err());
        let (bytes, length) = record(
            &"selected\\root\\valid.wav"
                .encode_utf16()
                .collect::<Vec<_>>(),
            4,
        );
        assert!(walk_records(&bytes, length, |_, _| panic!("Invalid offset forwarded")).is_err());
        let (bytes, length) = record(
            &"selected\\root\\中文.wav"
                .encode_utf16()
                .collect::<Vec<_>>(),
            0,
        );
        let mut own = None;
        walk_records(&bytes, length, |name, _| {
            if let Selection::Relative(relative) = selected_name(name, &prefix, true) {
                own = Some(String::from_utf16(relative).unwrap());
            }
        })
        .unwrap();
        assert_eq!(own.as_deref(), Some("中文.wav"));
    }
}
