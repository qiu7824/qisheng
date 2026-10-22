//! Bounded same-user deep-link transport. Dispatch retains the ordinary actor
//! command, source consent and URL consent rules.

use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

pub const MAX_RELAY_LINK_BYTES: usize = 32_768;
pub const MAX_RELAY_IDENTIFIER_BYTES: usize = 128;
const REQUEST_HEADER: usize = 12;
const ACK_HEADER: usize = 10;
const MAX_FRAME_BYTES: usize = REQUEST_HEADER + MAX_RELAY_IDENTIFIER_BYTES + MAX_RELAY_LINK_BYTES;
const MAX_RELAY_WORKERS: usize = 4;
pub const MAX_RELAY_CONNECTIONS: usize = MAX_RELAY_WORKERS - 1;
const MAX_DEADLINE: Duration = Duration::from_secs(10);
static WORKERS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelayErrorKind {
    Invalid,
    Busy,
    Cancelled,
    Timeout,
    Unauthorized,
    AlreadyOwned,
    Io,
    WorkerPanicked,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelayError {
    pub kind: RelayErrorKind,
    pub os_code: Option<u32>,
    pub delivery_unconfirmed: bool,
}
impl RelayError {
    fn new(kind: RelayErrorKind) -> Self {
        Self {
            kind,
            os_code: None,
            delivery_unconfirmed: false,
        }
    }
    #[cfg(windows)]
    fn os(kind: RelayErrorKind) -> Self {
        Self {
            kind,
            os_code: Some(unsafe { windows_sys::Win32::Foundation::GetLastError() }),
            delivery_unconfirmed: false,
        }
    }
}
impl fmt::Display for RelayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Deep-link relay {:?}", self.kind)?;
        if let Some(code) = self.os_code {
            write!(formatter, " (native code {code})")?;
        }
        if self.delivery_unconfirmed {
            formatter.write_str("; query the same request identifier before retrying")?;
        }
        Ok(())
    }
}
impl std::error::Error for RelayError {}
type RelayResult<T> = Result<T, RelayError>;

#[derive(Clone)]
pub struct RelayRequest {
    request_id: String,
    link: String,
}
impl fmt::Debug for RelayRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RelayRequest")
            .field("request_id", &self.request_id)
            .finish_non_exhaustive()
    }
}
fn identifier(value: &str) -> RelayResult<()> {
    if value.is_empty()
        || value.len() > MAX_RELAY_IDENTIFIER_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(RelayError::new(RelayErrorKind::Invalid));
    }
    Ok(())
}
impl RelayRequest {
    pub fn new(request_id: String, link: String) -> RelayResult<Self> {
        identifier(&request_id)?;
        if link.len() > MAX_RELAY_LINK_BYTES || crate::open_api::parse_deeplink(&link).is_err() {
            return Err(RelayError::new(RelayErrorKind::Invalid));
        }
        Ok(Self { request_id, link })
    }
    pub fn request_id(&self) -> &str {
        &self.request_id
    }
    pub fn link(&self) -> &str {
        &self.link
    }
    fn encode(&self) -> Vec<u8> {
        let mut bytes =
            Vec::with_capacity(REQUEST_HEADER + self.request_id.len() + self.link.len());
        bytes.extend_from_slice(b"LXDR\x01\x00");
        bytes.extend_from_slice(&(self.request_id.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&(self.link.len() as u32).to_le_bytes());
        bytes.extend_from_slice(self.request_id.as_bytes());
        bytes.extend_from_slice(self.link.as_bytes());
        bytes
    }
    fn header_lengths(bytes: &[u8]) -> RelayResult<(usize, usize)> {
        if bytes.len() != REQUEST_HEADER || &bytes[..6] != b"LXDR\x01\x00" {
            return Err(RelayError::new(RelayErrorKind::Invalid));
        }
        let id = u16::from_le_bytes([bytes[6], bytes[7]]) as usize;
        let link = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
        if id == 0 || id > MAX_RELAY_IDENTIFIER_BYTES || link == 0 || link > MAX_RELAY_LINK_BYTES {
            return Err(RelayError::new(RelayErrorKind::Invalid));
        }
        Ok((id, link))
    }
    fn decode(bytes: &[u8]) -> RelayResult<Self> {
        let header = bytes
            .get(..REQUEST_HEADER)
            .ok_or_else(|| RelayError::new(RelayErrorKind::Invalid))?;
        let (id, link) = Self::header_lengths(header)?;
        if bytes.len() != REQUEST_HEADER + id + link {
            return Err(RelayError::new(RelayErrorKind::Invalid));
        }
        let id = std::str::from_utf8(&bytes[REQUEST_HEADER..REQUEST_HEADER + id])
            .map_err(|_| RelayError::new(RelayErrorKind::Invalid))?;
        let link = std::str::from_utf8(&bytes[REQUEST_HEADER + id.len()..])
            .map_err(|_| RelayError::new(RelayErrorKind::Invalid))?;
        Self::new(id.into(), link.into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayDisposition {
    Applied,
    Committed,
    Accepted,
    Completed,
    Rejected,
    Busy,
    Cancelled,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayAcknowledgement {
    pub request_id: String,
    pub operation_id: Option<String>,
    pub disposition: RelayDisposition,
}
impl RelayAcknowledgement {
    fn validate(&self) -> RelayResult<()> {
        identifier(&self.request_id)?;
        if let Some(operation) = &self.operation_id {
            identifier(operation)?;
        }
        if matches!(
            self.disposition,
            RelayDisposition::Applied
                | RelayDisposition::Committed
                | RelayDisposition::Accepted
                | RelayDisposition::Completed
        ) && self.operation_id.is_none()
        {
            return Err(RelayError::new(RelayErrorKind::Invalid));
        }
        Ok(())
    }
    fn into_dispatch(self, fence: RelayFence) -> RelayDispatch {
        let (sender, receiver) = mpsc::sync_channel(1);
        let _ = sender.send(Ok(self));
        RelayDispatch::from_receiver(receiver, fence)
    }
    fn encode(&self) -> RelayResult<Vec<u8>> {
        self.validate()?;
        let status = match self.disposition {
            RelayDisposition::Applied => 0,
            RelayDisposition::Committed => 1,
            RelayDisposition::Accepted => 2,
            RelayDisposition::Completed => 6,
            RelayDisposition::Rejected => 3,
            RelayDisposition::Busy => 4,
            RelayDisposition::Cancelled => 5,
        };
        let operation = self.operation_id.as_deref().unwrap_or_default();
        let mut bytes = Vec::with_capacity(ACK_HEADER + self.request_id.len() + operation.len());
        bytes.extend_from_slice(b"LXDA\x01");
        bytes.push(status);
        bytes.extend_from_slice(&(self.request_id.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&(operation.len() as u16).to_le_bytes());
        bytes.extend_from_slice(self.request_id.as_bytes());
        bytes.extend_from_slice(operation.as_bytes());
        Ok(bytes)
    }
    fn header_lengths(bytes: &[u8]) -> RelayResult<(usize, usize)> {
        if bytes.len() != ACK_HEADER || &bytes[..5] != b"LXDA\x01" || bytes[5] > 6 {
            return Err(RelayError::new(RelayErrorKind::Invalid));
        }
        let id = u16::from_le_bytes([bytes[6], bytes[7]]) as usize;
        let operation = u16::from_le_bytes([bytes[8], bytes[9]]) as usize;
        if id == 0
            || id > MAX_RELAY_IDENTIFIER_BYTES
            || (operation == 0 && matches!(bytes[5], 0 | 1 | 2 | 6))
            || operation > MAX_RELAY_IDENTIFIER_BYTES
        {
            return Err(RelayError::new(RelayErrorKind::Invalid));
        }
        Ok((id, operation))
    }
    fn decode(bytes: &[u8]) -> RelayResult<Self> {
        let header = bytes
            .get(..ACK_HEADER)
            .ok_or_else(|| RelayError::new(RelayErrorKind::Invalid))?;
        let (id, operation) = Self::header_lengths(header)?;
        if bytes.len() != ACK_HEADER + id + operation {
            return Err(RelayError::new(RelayErrorKind::Invalid));
        }
        let text = |range| {
            std::str::from_utf8(&bytes[range])
                .map(str::to_owned)
                .map_err(|_| RelayError::new(RelayErrorKind::Invalid))
        };
        let disposition = match bytes[5] {
            0 => RelayDisposition::Applied,
            1 => RelayDisposition::Committed,
            2 => RelayDisposition::Accepted,
            3 => RelayDisposition::Rejected,
            4 => RelayDisposition::Busy,
            5 => RelayDisposition::Cancelled,
            6 => RelayDisposition::Completed,
            _ => return Err(RelayError::new(RelayErrorKind::Invalid)),
        };
        let result = Self {
            request_id: text(ACK_HEADER..ACK_HEADER + id)?,
            operation_id: if operation == 0 {
                None
            } else {
                Some(text(ACK_HEADER + id..bytes.len())?)
            },
            disposition,
        };
        result.validate()?;
        Ok(result)
    }
}

#[derive(Clone)]
pub struct RelayFence {
    closing: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
}
impl RelayFence {
    pub fn check(&self) -> RelayResult<()> {
        if self.closing.load(Ordering::Acquire) || self.cancelled.load(Ordering::Acquire) {
            return Err(RelayError::new(RelayErrorKind::Cancelled));
        }
        if Instant::now() >= self.deadline {
            return Err(RelayError::new(RelayErrorKind::Timeout));
        }
        Ok(())
    }
}
/// The actor bridge must recheck this owned fence immediately before ordinary
/// DispatchDeepLink effects. An acknowledgement represents its actual receipt.
pub trait RelayBackend: Send + Sync + 'static {
    fn dispatch_deeplink(
        &self,
        request: RelayRequest,
        fence: RelayFence,
    ) -> RelayResult<RelayDispatch>;
}
/// The normal actor owns the producer. Closing this observation cancels a
/// queued command; native connection handles remain with their IO worker.
pub struct RelayDispatch {
    receiver: mpsc::Receiver<RelayResult<RelayAcknowledgement>>,
    fence: RelayFence,
}
impl RelayDispatch {
    pub fn from_receiver(
        receiver: mpsc::Receiver<RelayResult<RelayAcknowledgement>>,
        fence: RelayFence,
    ) -> Self {
        Self { receiver, fence }
    }
}
impl Drop for RelayDispatch {
    fn drop(&mut self) {
        self.fence.cancelled.store(true, Ordering::Release);
    }
}
struct WorkerPermit;
impl WorkerPermit {
    fn acquire() -> RelayResult<Self> {
        WORKERS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_RELAY_WORKERS).then_some(count + 1)
            })
            .map(|_| Self)
            .map_err(|_| RelayError::new(RelayErrorKind::Busy))
    }
}
impl Drop for WorkerPermit {
    fn drop(&mut self) {
        WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(windows)]
mod native {
    use super::*;
    use crate::{
        repository::{ProfileGuard, ProfileLockIdentity},
        system_control::ManagedWorker,
    };
    use std::{ptr, sync::OnceLock, thread};
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_IO_INCOMPLETE,
            ERROR_IO_PENDING, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GetLastError, HANDLE,
            INVALID_HANDLE_VALUE, LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION_1,
            },
            GetTokenInformation, IsValidSid, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
            TokenUser,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
            PIPE_ACCESS_DUPLEX, ReadFile, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
            WriteFile,
        },
        System::{
            IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
            Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe,
                GetNamedPipeClientProcessId, GetNamedPipeServerProcessId, PIPE_READMODE_BYTE,
                PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT, PeekNamedPipe,
                WaitNamedPipeW,
            },
            Threading::{
                CreateEventW, GetCurrentProcess, OpenProcess, OpenProcessToken,
                PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
            },
        },
    };
    const GENERIC_READ_WRITE: u32 = 0x8000_0000 | 0x4000_0000;

    struct Handle(HANDLE);
    // Kernel pipe/event/token/process handles have no thread affinity. Only
    // their one owning worker closes them, after pending IO has completed.
    unsafe impl Send for Handle {}
    impl Handle {
        fn checked(value: HANDLE) -> RelayResult<Self> {
            if value.is_null() || value == INVALID_HANDLE_VALUE {
                Err(RelayError::os(RelayErrorKind::Io))
            } else {
                Ok(Self(value))
            }
        }
        fn close(&mut self) -> RelayResult<()> {
            if self.0.is_null() {
                return Ok(());
            }
            if unsafe { CloseHandle(self.0) } == 0 {
                return Err(RelayError::os(RelayErrorKind::Io));
            }
            self.0 = ptr::null_mut();
            Ok(())
        }
    }
    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }
    fn close_on_worker(handle: &mut Handle, closing: &AtomicBool) {
        while handle.close().is_err() {
            closing.store(true, Ordering::Release);
            thread::sleep(Duration::from_millis(20));
        }
    }
    struct IoEvent<'a> {
        handle: Handle,
        closing: &'a AtomicBool,
    }
    impl Drop for IoEvent<'_> {
        fn drop(&mut self) {
            close_on_worker(&mut self.handle, self.closing);
        }
    }
    struct LocalAllocation(*mut core::ffi::c_void);
    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }
    fn process_sid(process: HANDLE) -> RelayResult<String> {
        let mut token = ptr::null_mut();
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
            return Err(RelayError::os(RelayErrorKind::Unauthorized));
        }
        let token = Handle::checked(token)?;
        let mut length = 0;
        unsafe {
            GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut length);
        }
        if length < std::mem::size_of::<TOKEN_USER>() as u32 || length > 64 * 1024 {
            return Err(RelayError::new(RelayErrorKind::Unauthorized));
        }
        let mut words = vec![0usize; (length as usize).div_ceil(std::mem::size_of::<usize>())];
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                words.as_mut_ptr().cast(),
                length,
                &mut length,
            )
        } == 0
        {
            return Err(RelayError::os(RelayErrorKind::Unauthorized));
        }
        let user = unsafe { &*words.as_ptr().cast::<TOKEN_USER>() };
        if unsafe { IsValidSid(user.User.Sid) } == 0 {
            return Err(RelayError::new(RelayErrorKind::Unauthorized));
        }
        let mut output = ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut output) } == 0 {
            return Err(RelayError::os(RelayErrorKind::Unauthorized));
        }
        let _allocation = LocalAllocation(output.cast());
        let mut end = 0;
        while end < 184 && unsafe { *output.add(end) } != 0 {
            end += 1;
        }
        if end == 184 {
            return Err(RelayError::new(RelayErrorKind::Unauthorized));
        }
        String::from_utf16(unsafe { std::slice::from_raw_parts(output, end) })
            .map_err(|_| RelayError::new(RelayErrorKind::Unauthorized))
    }
    fn current_sid() -> RelayResult<String> {
        process_sid(unsafe { GetCurrentProcess() })
    }
    fn peer(pipe: &Handle, server: bool, wanted: &str) -> RelayResult<()> {
        let mut pid = 0;
        let ok = unsafe {
            if server {
                GetNamedPipeClientProcessId(pipe.0, &mut pid)
            } else {
                GetNamedPipeServerProcessId(pipe.0, &mut pid)
            }
        };
        if ok == 0 || pid == 0 {
            return Err(RelayError::os(RelayErrorKind::Unauthorized));
        }
        let process =
            Handle::checked(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) })?;
        if process_sid(process.0)? != wanted {
            return Err(RelayError::new(RelayErrorKind::Unauthorized));
        }
        Ok(())
    }
    #[derive(Clone)]
    pub struct RelayEndpoint {
        name: String,
        sid: String,
        active_writer: bool,
    }
    impl fmt::Debug for RelayEndpoint {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("RelayEndpoint(verified profile and user)")
        }
    }
    impl RelayEndpoint {
        pub(crate) fn from_active_guard(guard: &ProfileGuard) -> RelayResult<Self> {
            guard
                .verify()
                .map_err(|_| RelayError::new(RelayErrorKind::Unauthorized))?;
            let mut endpoint = Self::from_existing_guard(guard)?;
            endpoint.active_writer = true;
            Ok(endpoint)
        }
        pub(crate) fn from_existing_guard(guard: &ProfileGuard) -> RelayResult<Self> {
            use sha2::{Digest, Sha256};
            // This getter must verify the held descriptor and continuous leaf
            // proof. It never hashes a caller-provided pathname as identity.
            let identity: ProfileLockIdentity = guard
                .verified_lock_identity()
                .map_err(|_| RelayError::new(RelayErrorKind::Unauthorized))?;
            let sid = current_sid()?;
            let scope = serde_json::to_vec(&("lx-native-relay-v1", identity, &sid))
                .map_err(|_| RelayError::new(RelayErrorKind::Invalid))?;
            Ok(Self {
                name: format!(r"\\.\pipe\LXNative.v1.{:x}", Sha256::digest(scope)),
                sid,
                active_writer: false,
            })
        }
        #[cfg(test)]
        fn isolated(name: &str) -> RelayResult<Self> {
            identifier(name)?;
            if !name
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-')
            {
                return Err(RelayError::new(RelayErrorKind::Invalid));
            }
            Ok(Self {
                name: format!(r"\\.\pipe\LXNative.IsolatedTest.{name}"),
                sid: current_sid()?,
                active_writer: true,
            })
        }
    }
    fn create_pipe(endpoint: &RelayEndpoint, first: bool) -> RelayResult<Handle> {
        let sddl = wide(&format!("D:P(A;;GA;;;{})", endpoint.sid));
        let mut descriptor = ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(RelayError::os(RelayErrorKind::Unauthorized));
        }
        let _descriptor = LocalAllocation(descriptor);
        let security = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let name = wide(&endpoint.name);
        let pipe = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX
                    | FILE_FLAG_OVERLAPPED
                    | if first {
                        FILE_FLAG_FIRST_PIPE_INSTANCE
                    } else {
                        0
                    },
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                MAX_RELAY_WORKERS as u32,
                512,
                MAX_FRAME_BYTES as u32,
                20,
                &security,
            )
        };
        if pipe == INVALID_HANDLE_VALUE {
            let code = unsafe { GetLastError() };
            return Err(RelayError {
                kind: if first && code == ERROR_ACCESS_DENIED {
                    RelayErrorKind::AlreadyOwned
                } else {
                    RelayErrorKind::Io
                },
                os_code: Some(code),
                delivery_unconfirmed: false,
            });
        }
        Handle::checked(pipe)
    }

    enum IoKind {
        Connect,
        Read(usize),
        Write(Vec<u8>),
    }
    // This function never returns while the kernel still references its buffer,
    // OVERLAPPED or event. Cancellation waits belong to the retained worker.
    fn transfer(
        pipe: &Handle,
        kind: IoKind,
        closing: &AtomicBool,
        deadline: Option<Instant>,
    ) -> RelayResult<Vec<u8>> {
        if closing.load(Ordering::Acquire) {
            return Err(RelayError::new(RelayErrorKind::Cancelled));
        }
        let event = IoEvent {
            handle: Handle::checked(unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) })?,
            closing,
        };
        let mut overlapped = Box::new(OVERLAPPED::default());
        overlapped.hEvent = event.handle.0;
        let connect = matches!(&kind, IoKind::Connect);
        let writing = matches!(&kind, IoKind::Write(_));
        let mut bytes = match kind {
            IoKind::Connect => Vec::new(),
            IoKind::Read(length) => {
                if length > MAX_FRAME_BYTES {
                    return Err(RelayError::new(RelayErrorKind::Invalid));
                }
                vec![0; length]
            }
            IoKind::Write(bytes) => bytes,
        };
        let mut count = 0;
        let submitted = unsafe {
            if connect {
                ConnectNamedPipe(pipe.0, &mut *overlapped)
            } else if writing {
                WriteFile(
                    pipe.0,
                    bytes.as_ptr(),
                    bytes.len() as u32,
                    &mut count,
                    &mut *overlapped,
                )
            } else {
                ReadFile(
                    pipe.0,
                    bytes.as_mut_ptr(),
                    bytes.len() as u32,
                    &mut count,
                    &mut *overlapped,
                )
            }
        };
        let mut pending = submitted == 0;
        if pending {
            let code = unsafe { GetLastError() };
            if connect && code == ERROR_PIPE_CONNECTED {
                pending = false;
            } else if code != ERROR_IO_PENDING {
                return Err(RelayError {
                    kind: RelayErrorKind::Io,
                    os_code: Some(code),
                    delivery_unconfirmed: false,
                });
            }
        }
        let mut cancellation = None;
        if pending {
            loop {
                if unsafe { GetOverlappedResult(pipe.0, &*overlapped, &mut count, 0) } != 0 {
                    break;
                }
                let code = unsafe { GetLastError() };
                if code != ERROR_IO_INCOMPLETE {
                    return Err(cancellation.unwrap_or(RelayError {
                        kind: RelayErrorKind::Io,
                        os_code: Some(code),
                        delivery_unconfirmed: false,
                    }));
                }
                if cancellation.is_none() {
                    if closing.load(Ordering::Acquire) {
                        cancellation = Some(RelayError::new(RelayErrorKind::Cancelled));
                    } else if deadline.is_some_and(|at| Instant::now() >= at) {
                        cancellation = Some(RelayError::new(RelayErrorKind::Timeout));
                    }
                    if cancellation.is_some() {
                        unsafe {
                            CancelIoEx(pipe.0, &*overlapped);
                        }
                    }
                }
                let waited = unsafe { WaitForSingleObject(event.handle.0, 20) };
                if waited != WAIT_OBJECT_0 && waited != WAIT_TIMEOUT {
                    unsafe {
                        CancelIoEx(pipe.0, &*overlapped);
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            }
        }
        if let Some(error) = cancellation {
            return Err(error);
        }
        if closing.load(Ordering::Acquire) {
            return Err(RelayError::new(RelayErrorKind::Cancelled));
        }
        if deadline.is_some_and(|at| Instant::now() >= at) {
            return Err(RelayError::new(RelayErrorKind::Timeout));
        }
        if !connect && count == 0 {
            return Err(RelayError::new(RelayErrorKind::Io));
        }
        if writing && count as usize != bytes.len() {
            return Err(RelayError::new(RelayErrorKind::Io));
        }
        bytes.truncate(count as usize);
        Ok(bytes)
    }
    fn read_exact(
        pipe: &Handle,
        length: usize,
        closing: &AtomicBool,
        deadline: Instant,
    ) -> RelayResult<Vec<u8>> {
        if length > MAX_FRAME_BYTES {
            return Err(RelayError::new(RelayErrorKind::Invalid));
        }
        let mut bytes = Vec::with_capacity(length);
        while bytes.len() < length {
            bytes.extend(transfer(
                pipe,
                IoKind::Read(length - bytes.len()),
                closing,
                Some(deadline),
            )?);
        }
        Ok(bytes)
    }
    fn server_worker(
        mut pipe: Handle,
        endpoint: RelayEndpoint,
        closing: Arc<AtomicBool>,
        backend: Arc<dyn RelayBackend>,
        _permit: WorkerPermit,
    ) {
        while !closing.load(Ordering::Acquire) {
            if transfer(&pipe, IoKind::Connect, &closing, None).is_err() {
                break;
            }
            let deadline = Instant::now() + MAX_DEADLINE;
            let outcome = (|| -> RelayResult<()> {
                peer(&pipe, true, &endpoint.sid)?;
                let mut bytes = read_exact(&pipe, REQUEST_HEADER, &closing, deadline)?;
                let (id, link) = RelayRequest::header_lengths(&bytes)?;
                bytes.extend(read_exact(&pipe, id + link, &closing, deadline)?);
                let request = RelayRequest::decode(&bytes)?;
                let fence = RelayFence {
                    closing: closing.clone(),
                    cancelled: Arc::new(AtomicBool::new(false)),
                    deadline,
                };
                fence.check()?;
                let expected = request.request_id().to_owned();
                let dispatch = match backend.dispatch_deeplink(request, fence.clone()) {
                    Ok(dispatch) => dispatch,
                    Err(error) => RelayAcknowledgement {
                        request_id: expected.clone(),
                        operation_id: None,
                        disposition: match error.kind {
                            RelayErrorKind::Busy => RelayDisposition::Busy,
                            RelayErrorKind::Cancelled | RelayErrorKind::Timeout => {
                                RelayDisposition::Cancelled
                            }
                            _ => RelayDisposition::Rejected,
                        },
                    }
                    .into_dispatch(fence.clone()),
                };
                let ack = loop {
                    fence.check()?;
                    if unsafe {
                        PeekNamedPipe(
                            pipe.0,
                            ptr::null_mut(),
                            0,
                            ptr::null_mut(),
                            ptr::null_mut(),
                            ptr::null_mut(),
                        )
                    } == 0
                    {
                        return Err(RelayError::os(RelayErrorKind::Cancelled));
                    }
                    match dispatch.receiver.recv_timeout(Duration::from_millis(20)) {
                        Ok(Ok(ack)) => break ack,
                        Ok(Err(error)) => {
                            break RelayAcknowledgement {
                                request_id: expected.clone(),
                                operation_id: None,
                                disposition: match error.kind {
                                    RelayErrorKind::Busy => RelayDisposition::Busy,
                                    RelayErrorKind::Cancelled | RelayErrorKind::Timeout => {
                                        RelayDisposition::Cancelled
                                    }
                                    _ => RelayDisposition::Rejected,
                                },
                            };
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => (),
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            return Err(RelayError::new(RelayErrorKind::Io));
                        }
                    }
                };
                if ack.request_id != expected {
                    return Err(RelayError::new(RelayErrorKind::Invalid));
                }
                fence.check()?;
                transfer(
                    &pipe,
                    IoKind::Write(ack.encode()?),
                    &closing,
                    Some(deadline),
                )?;
                // Disconnect discards unread outbound pipe bytes. A bounded
                // client read confirmation replaces an unbounded FlushFileBuffers.
                if read_exact(&pipe, 1, &closing, deadline)? != [0xac] {
                    return Err(RelayError::new(RelayErrorKind::Invalid));
                }
                Ok(())
            })();
            unsafe {
                DisconnectNamedPipe(pipe.0);
            }
            if outcome.is_err() && closing.load(Ordering::Acquire) {
                break;
            }
        }
        // Every transfer has actually completed (or cancelled and completed)
        // before this worker releases the physical pipe handle.
        close_on_worker(&mut pipe, &closing);
        drop(pipe);
    }

    struct PendingStart {
        pipe: Handle,
        endpoint: RelayEndpoint,
        closing: Arc<AtomicBool>,
        backend: Arc<dyn RelayBackend>,
        permit: WorkerPermit,
    }
    type StartSlot = Arc<Mutex<Option<PendingStart>>>;
    struct StagingState {
        starts: Vec<PendingStart>,
        reservations: usize,
        accepting: bool,
    }
    struct StagingRetirement {
        state: Arc<Mutex<StagingState>>,
        worker: Option<ManagedWorker>,
        error: Option<RelayError>,
    }
    static STAGING_RETIREMENT: OnceLock<Mutex<Option<StagingRetirement>>> = OnceLock::new();
    fn staging_slot() -> &'static Mutex<Option<StagingRetirement>> {
        STAGING_RETIREMENT.get_or_init(|| Mutex::new(None))
    }
    fn join_staging_idle(slot: &mut Option<StagingRetirement>) -> RelayResult<bool> {
        let Some(retirement) = slot.as_mut() else {
            return Ok(true);
        };
        if let Some(error) = retirement.error {
            return Err(error);
        }
        if retirement
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            return Ok(false);
        }
        if let Some(mut worker) = retirement.worker.take()
            && worker.join_finished().is_err()
        {
            let error = RelayError::new(RelayErrorKind::WorkerPanicked);
            retirement.error = Some(error);
            return Err(error);
        }
        let state = retirement.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.reservations != 0 || !state.starts.is_empty() {
            return Err(RelayError::new(RelayErrorKind::Io));
        }
        drop(state);
        *slot = None;
        Ok(true)
    }
    struct StagingReservation {
        state: Arc<Mutex<StagingState>>,
        active: bool,
    }
    impl StagingReservation {
        fn reserve() -> RelayResult<Self> {
            let mut slot = staging_slot().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(retirement) = slot.as_ref() {
                let mut state = retirement.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.accepting {
                    if state.reservations >= MAX_RELAY_WORKERS {
                        return Err(RelayError::new(RelayErrorKind::Busy));
                    }
                    state.reservations += 1;
                    return Ok(Self {
                        state: retirement.state.clone(),
                        active: true,
                    });
                }
            }
            if !join_staging_idle(&mut slot)? {
                return Err(RelayError::new(RelayErrorKind::Busy));
            }
            let permit = WorkerPermit::acquire()?;
            let state = Arc::new(Mutex::new(StagingState {
                starts: Vec::with_capacity(MAX_RELAY_WORKERS),
                reservations: 1,
                accepting: true,
            }));
            let retained = state.clone();
            let worker = ManagedWorker::spawn("lx-relay-staging-retirement", move || {
                let _permit = permit;
                loop {
                    let mut state = retained.lock().unwrap_or_else(|e| e.into_inner());
                    let mut index = 0;
                    while index < state.starts.len() {
                        if state.starts[index].pipe.close().is_ok() {
                            state.starts.swap_remove(index);
                        } else {
                            index += 1;
                        }
                    }
                    if state.reservations == 0 && state.starts.is_empty() {
                        state.accepting = false;
                        break;
                    }
                    drop(state);
                    thread::sleep(Duration::from_millis(20));
                }
            })
            .map_err(|_| RelayError::new(RelayErrorKind::Io))?;
            *slot = Some(StagingRetirement {
                state: state.clone(),
                worker: Some(worker),
                error: None,
            });
            Ok(Self {
                state,
                active: true,
            })
        }
        fn retain(&self, start: PendingStart) {
            // Every retained start still owns a charged permit; the bank cannot
            // outgrow the same four-role bound that admitted its actual pipe.
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .starts
                .push(start);
        }
        fn release(&mut self) {
            if self.active {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                state.reservations -= 1;
                self.active = false;
            }
        }
        fn poll_actual_idle(&self) -> RelayResult<bool> {
            let mut slot = staging_slot().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(retirement) = slot.as_ref() {
                if !Arc::ptr_eq(&retirement.state, &self.state) {
                    return Ok(true);
                }
                let state = retirement.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.reservations > 0 {
                    return Ok(true);
                }
            }
            join_staging_idle(&mut slot)
        }
    }
    impl Drop for StagingReservation {
        fn drop(&mut self) {
            self.release();
            let _ = self.poll_actual_idle();
        }
    }
    pub struct RelayServer {
        endpoint: RelayEndpoint,
        closing: Arc<AtomicBool>,
        workers: Vec<ManagedWorker>,
        staged: Option<StartSlot>,
        retirement: Option<StagingReservation>,
        join_error: Option<RelayError>,
    }
    pub struct RelayServerFailure {
        pub error: RelayError,
        pub owner: Option<Box<RelayServer>>,
    }
    impl fmt::Debug for RelayServerFailure {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("RelayServerFailure")
                .field("error", &self.error)
                .finish_non_exhaustive()
        }
    }
    impl RelayServer {
        fn start_endpoint(
            endpoint: RelayEndpoint,
            backend: Arc<dyn RelayBackend>,
            connections: usize,
        ) -> Result<Self, RelayServerFailure> {
            let mut owner = Self {
                endpoint,
                closing: Arc::new(AtomicBool::new(false)),
                workers: Vec::with_capacity(MAX_RELAY_WORKERS),
                staged: None,
                retirement: None,
                join_error: None,
            };
            let result = (|| -> RelayResult<()> {
                if !owner.endpoint.active_writer {
                    return Err(RelayError::new(RelayErrorKind::Unauthorized));
                }
                if !(1..=MAX_RELAY_CONNECTIONS).contains(&connections) {
                    return Err(RelayError::new(RelayErrorKind::Invalid));
                }
                owner.retirement = Some(StagingReservation::reserve()?);
                for index in 0..connections {
                    let permit = WorkerPermit::acquire()?;
                    let pipe = create_pipe(&owner.endpoint, index == 0)?;
                    let slot = Arc::new(Mutex::new(Some(PendingStart {
                        pipe,
                        endpoint: owner.endpoint.clone(),
                        closing: owner.closing.clone(),
                        backend: backend.clone(),
                        permit,
                    })));
                    owner.staged = Some(slot.clone());
                    let worker = ManagedWorker::spawn("lx-deeplink-relay", move || {
                        let start = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
                        if let Some(start) = start {
                            server_worker(
                                start.pipe,
                                start.endpoint,
                                start.closing,
                                start.backend,
                                start.permit,
                            );
                        }
                    })
                    .map_err(|_| RelayError::new(RelayErrorKind::Io))?;
                    owner.staged = None;
                    owner.workers.push(worker);
                }
                Ok(())
            })();
            match result {
                Ok(()) => Ok(owner),
                Err(error) => {
                    owner.begin_shutdown();
                    Err(RelayServerFailure {
                        error,
                        owner: Some(Box::new(owner)),
                    })
                }
            }
        }
        pub fn endpoint(&self) -> &RelayEndpoint {
            &self.endpoint
        }
        pub(crate) fn start_verified_endpoint(
            endpoint: RelayEndpoint,
            backend: Arc<dyn RelayBackend>,
        ) -> Result<Self, RelayServerFailure> {
            Self::start_endpoint(endpoint, backend, 1)
        }
        pub fn accepts_fence(&self, fence: &RelayFence) -> bool {
            Arc::ptr_eq(&self.closing, &fence.closing) && !self.closing.load(Ordering::Acquire)
        }
        pub fn begin_shutdown(&self) {
            self.closing.store(true, Ordering::Release);
        }
        pub fn shutdown(&mut self, timeout: Duration) -> RelayResult<bool> {
            self.begin_shutdown();
            if let Some(error) = self.join_error {
                return Err(error);
            }
            if let Some(slot) = &self.staged {
                let mut staged = slot.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(start) = staged.as_mut() {
                    start.pipe.close()?;
                }
                staged.take();
            }
            self.staged = None;
            let deadline = Instant::now() + timeout.min(MAX_DEADLINE);
            loop {
                let mut index = 0;
                while index < self.workers.len() {
                    if self.workers[index].is_finished() {
                        let mut worker = self.workers.swap_remove(index);
                        if worker.join_finished().is_err() {
                            self.join_error = Some(RelayError::new(RelayErrorKind::WorkerPanicked));
                        }
                    } else {
                        index += 1;
                    }
                }
                if let Some(error) = self.join_error {
                    return Err(error);
                }
                if self.workers.is_empty() {
                    if let Some(retirement) = self.retirement.as_mut() {
                        retirement.release();
                        if !retirement.poll_actual_idle()? {
                            if Instant::now() >= deadline {
                                return Ok(false);
                            }
                            thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                    }
                    self.retirement = None;
                    return Ok(true);
                }
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                thread::sleep(Duration::from_millis(2));
            }
        }
    }
    impl Drop for RelayServer {
        fn drop(&mut self) {
            self.begin_shutdown();
            if let Some(slot) = self.staged.take()
                && let Some(start) = slot.lock().unwrap_or_else(|e| e.into_inner()).take()
            {
                // The permit remains in this bounded bank with the actual pipe.
                // No failed closure spawn may dispose of a prepared native owner.
                // Successful preparation reserved this staging lease and worker
                // before creating the pipe; this path cannot need a new spawn.
                self.retirement
                    .as_ref()
                    .expect("Reserved actual relay retirement owner")
                    .retain(start);
            }
        }
    }

    pub struct RelayClient {
        closing: Arc<AtomicBool>,
        worker: Option<ManagedWorker>,
        result: Arc<Mutex<Option<RelayResult<RelayAcknowledgement>>>>,
        join_error: Option<RelayError>,
    }
    pub struct RelayClientFailure {
        pub error: RelayError,
        pub owner: Option<Box<RelayClient>>,
    }
    impl fmt::Debug for RelayClientFailure {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("RelayClientFailure")
                .field("error", &self.error)
                .field("retained", &self.owner.is_some())
                .finish()
        }
    }
    impl fmt::Display for RelayClientFailure {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.error.fmt(f)
        }
    }
    impl std::error::Error for RelayClientFailure {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.error)
        }
    }
    fn client_worker(
        endpoint: RelayEndpoint,
        request: RelayRequest,
        closing: Arc<AtomicBool>,
        deadline: Instant,
    ) -> RelayResult<RelayAcknowledgement> {
        if current_sid()? != endpoint.sid {
            return Err(RelayError::new(RelayErrorKind::Unauthorized));
        }
        let name = wide(&endpoint.name);
        let mut pipe = loop {
            if closing.load(Ordering::Acquire) {
                return Err(RelayError::new(RelayErrorKind::Cancelled));
            }
            if Instant::now() >= deadline {
                return Err(RelayError::new(RelayErrorKind::Timeout));
            }
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ_WRITE,
                    0,
                    ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                break Handle::checked(handle)?;
            }
            let code = unsafe { GetLastError() };
            if code != ERROR_PIPE_BUSY && code != ERROR_FILE_NOT_FOUND {
                return Err(RelayError {
                    kind: RelayErrorKind::Io,
                    os_code: Some(code),
                    delivery_unconfirmed: false,
                });
            }
            unsafe {
                WaitNamedPipeW(name.as_ptr(), 20);
            }
            thread::sleep(Duration::from_millis(2));
        };
        if let Err(error) = peer(&pipe, false, &endpoint.sid) {
            close_on_worker(&mut pipe, &closing);
            return Err(error);
        }
        let mut submitted = false;
        let result = (|| -> RelayResult<_> {
            // Even a failed write can have delivered part of a frame. No later
            // failure is represented as proof that an ordinary command did not run.
            submitted = true;
            transfer(
                &pipe,
                IoKind::Write(request.encode()),
                &closing,
                Some(deadline),
            )?;
            let mut bytes = read_exact(&pipe, ACK_HEADER, &closing, deadline)?;
            let (id, operation) = RelayAcknowledgement::header_lengths(&bytes)?;
            bytes.extend(read_exact(&pipe, id + operation, &closing, deadline)?);
            let acknowledgement = RelayAcknowledgement::decode(&bytes)?;
            if acknowledgement.request_id != request.request_id {
                return Err(RelayError::new(RelayErrorKind::Invalid));
            }
            transfer(&pipe, IoKind::Write(vec![0xac]), &closing, Some(deadline))?;
            Ok(acknowledgement)
        })();
        close_on_worker(&mut pipe, &closing);
        drop(pipe);
        result.map_err(|mut error| {
            error.delivery_unconfirmed = submitted;
            error
        })
    }
    impl RelayClient {
        pub fn start(
            endpoint: RelayEndpoint,
            request: RelayRequest,
            timeout: Duration,
        ) -> RelayResult<Self> {
            if timeout.is_zero() || timeout > MAX_DEADLINE {
                return Err(RelayError::new(RelayErrorKind::Invalid));
            }
            let permit = WorkerPermit::acquire()?;
            let closing = Arc::new(AtomicBool::new(false));
            let cancellation = closing.clone();
            let result = Arc::new(Mutex::new(None));
            let output = result.clone();
            let deadline = Instant::now() + timeout;
            let worker = ManagedWorker::spawn("lx-deeplink-relay-client", move || {
                let _permit = permit;
                let settled = client_worker(endpoint, request, cancellation, deadline);
                *output.lock().unwrap_or_else(|e| e.into_inner()) = Some(settled);
            })
            .map_err(|_| RelayError::new(RelayErrorKind::Io))?;
            Ok(Self {
                closing,
                worker: Some(worker),
                result,
                join_error: None,
            })
        }
        pub fn begin_shutdown(&self) {
            self.closing.store(true, Ordering::Release);
        }
        fn poll_join(&mut self) -> RelayResult<bool> {
            if let Some(error) = self.join_error {
                return Err(error);
            }
            if self
                .worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
            {
                return Ok(false);
            }
            if let Some(mut worker) = self.worker.take()
                && worker.join_finished().is_err()
            {
                let error = RelayError::new(RelayErrorKind::WorkerPanicked);
                self.join_error = Some(error);
                return Err(error);
            }
            Ok(true)
        }
        pub fn shutdown(&mut self, timeout: Duration) -> RelayResult<bool> {
            self.begin_shutdown();
            let deadline = Instant::now() + timeout.min(MAX_DEADLINE);
            loop {
                if self.poll_join()? {
                    return Ok(true);
                }
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                thread::sleep(Duration::from_millis(2));
            }
        }
        pub fn finish(
            mut self,
            observation: Duration,
        ) -> Result<RelayAcknowledgement, RelayClientFailure> {
            let deadline = Instant::now() + observation.min(MAX_DEADLINE);
            loop {
                match self.poll_join() {
                    Ok(true) => {
                        let result = self.result.lock().unwrap_or_else(|e| e.into_inner()).take();
                        return result
                            .unwrap_or_else(|| Err(RelayError::new(RelayErrorKind::Io)))
                            .map_err(|error| RelayClientFailure { error, owner: None });
                    }
                    Ok(false) => (),
                    Err(error) => {
                        return Err(RelayClientFailure {
                            error,
                            owner: Some(Box::new(self)),
                        });
                    }
                }
                if Instant::now() >= deadline {
                    self.begin_shutdown();
                    let mut error = RelayError::new(RelayErrorKind::Timeout);
                    error.delivery_unconfirmed = true;
                    return Err(RelayClientFailure {
                        error,
                        owner: Some(Box::new(self)),
                    });
                }
                thread::sleep(Duration::from_millis(2));
            }
        }
    }
    impl Drop for RelayClient {
        fn drop(&mut self) {
            self.begin_shutdown();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        struct Backend;
        impl RelayBackend for Backend {
            fn dispatch_deeplink(
                &self,
                request: RelayRequest,
                fence: RelayFence,
            ) -> RelayResult<RelayDispatch> {
                fence.check()?;
                Ok(RelayAcknowledgement {
                    request_id: request.request_id.clone(),
                    operation_id: Some(request.request_id),
                    disposition: RelayDisposition::Applied,
                }
                .into_dispatch(fence))
            }
        }
        #[test]
        #[ignore = "Requires explicit opt-in for one isolated same-user native named-pipe fixture; never registers an LX protocol handler."]
        fn isolated_actual_pipe_transfers_one_valid_deeplink_then_releases_real_workers() {
            let _case = super::super::tests::CASES
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let name = format!(
                "{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let endpoint = RelayEndpoint::isolated(&name).unwrap();
            let mut server =
                RelayServer::start_endpoint(endpoint.clone(), Arc::new(Backend), 1).unwrap();
            assert!(RelayServer::start_endpoint(endpoint.clone(), Arc::new(Backend), 1).is_err());
            let request =
                RelayRequest::new("isolated-request".into(), "lxmusic://player/pause".into())
                    .unwrap();
            let result = RelayClient::start(endpoint, request, Duration::from_secs(3))
                .unwrap()
                .finish(Duration::from_secs(4))
                .unwrap();
            assert_eq!(result.request_id, "isolated-request");
            assert_eq!(result.disposition, RelayDisposition::Applied);
            assert!(server.shutdown(Duration::from_secs(3)).unwrap());
        }
    }
}
#[cfg(windows)]
pub use native::{RelayClient, RelayClientFailure, RelayEndpoint, RelayServer, RelayServerFailure};

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) static CASES: Mutex<()> = Mutex::new(());
    #[test]
    fn strict_frames_preserve_utf8_and_reject_truncation_trailing_bytes_and_grant_commands() {
        let request =
            RelayRequest::new("正式深链-1".into(), "lxmusic://player/pause".into()).unwrap();
        let frame = request.encode();
        let decoded = RelayRequest::decode(&frame).unwrap();
        assert_eq!(decoded.request_id, request.request_id);
        assert_eq!(decoded.link, request.link);
        for length in 0..frame.len() {
            assert!(RelayRequest::decode(&frame[..length]).is_err());
        }
        let mut extra = frame.clone();
        extra.push(0);
        assert!(RelayRequest::decode(&extra).is_err());
        let mut large = frame;
        large[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(RelayRequest::decode(&large).is_err());
        for link in [
            "https://unapproved.invalid/audio.mp3",
            "lxmusic://player/approveSource",
            "lxmusic://player/approvePlaybackUrl",
            "{\"command\":\"approve_playback_url\"}",
        ] {
            assert!(RelayRequest::new("request".into(), link.into()).is_err());
        }
        assert!(RelayRequest::new("x".repeat(129), "lxmusic://player/pause".into()).is_err());
    }
    #[test]
    fn acknowledgements_are_bounded_typed_and_bound_to_the_request_identifier() {
        let ack = RelayAcknowledgement {
            request_id: "request".into(),
            operation_id: Some("actual-operation".into()),
            disposition: RelayDisposition::Accepted,
        };
        let bytes = ack.encode().unwrap();
        assert_eq!(RelayAcknowledgement::decode(&bytes).unwrap(), ack);
        let mut bad = bytes.clone();
        bad[5] = 9;
        assert!(RelayAcknowledgement::decode(&bad).is_err());
        let mut trailing = bytes;
        trailing.push(0);
        assert!(RelayAcknowledgement::decode(&trailing).is_err());
        let completed = RelayAcknowledgement {
            disposition: RelayDisposition::Completed,
            ..ack
        };
        assert_eq!(
            RelayAcknowledgement::decode(&completed.encode().unwrap()).unwrap(),
            completed
        );
        assert_eq!(
            serde_json::from_value::<RelayAcknowledgement>(
                serde_json::to_value(&completed).unwrap()
            )
            .unwrap(),
            completed
        );
    }
    #[test]
    fn cancelled_or_expired_owned_fences_reject_dispatch_without_a_delivery_claim() {
        let closing = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::new(AtomicBool::new(false));
        let fence = RelayFence {
            closing: closing.clone(),
            cancelled: cancelled.clone(),
            deadline: Instant::now() + Duration::from_secs(1),
        };
        fence.check().unwrap();
        cancelled.store(true, Ordering::Release);
        assert_eq!(fence.check().unwrap_err().kind, RelayErrorKind::Cancelled);
        cancelled.store(false, Ordering::Release);
        closing.store(true, Ordering::Release);
        assert_eq!(fence.check().unwrap_err().kind, RelayErrorKind::Cancelled);
        let expired = RelayFence {
            closing: Arc::new(AtomicBool::new(false)),
            cancelled,
            deadline: Instant::now(),
        };
        assert_eq!(expired.check().unwrap_err().kind, RelayErrorKind::Timeout);
        assert!(!expired.check().unwrap_err().delivery_unconfirmed);
        assert!(
            !format!(
                "{:?}",
                RelayRequest::new("safe".into(), "lxmusic://player/pause".into()).unwrap()
            )
            .contains("lxmusic")
        );
    }
    #[test]
    fn a_closed_receipt_observer_cancels_the_same_queued_actor_fence() {
        let fence = RelayFence {
            closing: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline: Instant::now() + Duration::from_secs(1),
        };
        let (_producer, receiver) = mpsc::sync_channel(1);
        let dispatch = RelayDispatch::from_receiver(receiver, fence.clone());
        fence.check().unwrap();
        drop(dispatch);
        assert_eq!(fence.check().unwrap_err().kind, RelayErrorKind::Cancelled);
    }
    #[test]
    fn four_owned_worker_permits_remain_charged_until_their_actual_owner_retires() {
        let _case = CASES.lock().unwrap_or_else(|e| e.into_inner());
        let mut permits = Vec::new();
        let baseline = WORKERS.load(Ordering::Acquire);
        for _ in baseline..MAX_RELAY_WORKERS {
            permits.push(WorkerPermit::acquire().unwrap());
        }
        assert_eq!(
            WorkerPermit::acquire().err().unwrap().kind,
            RelayErrorKind::Busy
        );
        drop(permits.pop());
        let replacement = WorkerPermit::acquire().unwrap();
        assert!(WorkerPermit::acquire().is_err());
        drop(replacement);
        drop(permits);
        let free = WorkerPermit::acquire().unwrap();
        drop(free);
    }
}
