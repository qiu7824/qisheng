//! Per-profile launch forwarding; the UI receives only explicitly opened paths.
use anyhow::{Context, Result, ensure};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use zsui::UiInvalidationHandle;

pub const RELAUNCH_COMMAND: &str = "instance.relaunch";
#[derive(Clone, Default)]
pub struct Inbox(Arc<Pending>);
#[derive(Default)]
struct Pending {
    paths: Mutex<VecDeque<Vec<PathBuf>>>,
    closing: AtomicBool,
    handled: AtomicUsize,
    #[cfg(feature = "smoke")]
    background_closes: AtomicUsize,
}
impl Inbox {
    pub fn take(&self) -> Option<Vec<PathBuf>> {
        self.0
            .paths
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }
    pub fn close(&self) {
        self.0.closing.store(true, Ordering::Release);
    }
    pub fn handled(&self) {
        self.0.handled.fetch_add(1, Ordering::AcqRel);
    }
    #[cfg(feature = "smoke")]
    pub fn handled_count(&self) -> usize {
        self.0.handled.load(Ordering::Acquire)
    }
    #[cfg(feature = "smoke")]
    pub fn background_closed(&self) {
        self.0.background_closes.fetch_add(1, Ordering::AcqRel);
    }
    #[cfg(feature = "smoke")]
    pub fn background_close_count(&self) -> usize {
        self.0.background_closes.load(Ordering::Acquire)
    }
    #[cfg(unix)]
    fn enqueue(&self, paths: Vec<PathBuf>, wake: &UiInvalidationHandle) -> Result<()> {
        ensure!(
            !self.0.closing.load(Ordering::Acquire),
            "The application is closing"
        );
        let mut queue = self.0.paths.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(queue.len() < 8, "The launch queue is full");
        queue.push_back(paths);
        if let Err(error) = wake.request_app_command(zsui::Command::custom(RELAUNCH_COMMAND)) {
            queue.pop_back();
            return Err(error.into());
        }
        Ok(())
    }
}

pub fn id_for_directory(directory: &Path) -> Result<String> {
    std::fs::create_dir_all(directory)?;
    let directory = std::fs::canonicalize(directory)?;
    #[cfg(windows)]
    {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::hash::DefaultHasher::new();
        directory.to_string_lossy().to_lowercase().hash(&mut hasher);
        Ok(format!("lx-native-{:016x}", hasher.finish()))
    }
    #[cfg(not(windows))]
    {
        use sha2::{Digest, Sha256};
        let grant = lx_gpui_player::download::DestinationGrant::open_selected(&directory, 1)?;
        let key = format!("{}:{}", grant.identity().device, grant.identity().object);
        Ok(format!(
            "qs-{}",
            &format!("{:x}", Sha256::digest(key.as_bytes()))[..24]
        ))
    }
}

pub enum Launch {
    Forwarded,
    Primary(Guard),
}
pub struct Guard {
    #[cfg(unix)]
    _server: unix::Server,
}
pub fn start(
    id: &str,
    paths: &[PathBuf],
    inbox: Inbox,
    wake: UiInvalidationHandle,
) -> Result<Launch> {
    #[cfg(windows)]
    {
        let _ = (paths, inbox, wake);
        if zsui::zs_forward_to_running_instance(id) {
            return Ok(Launch::Forwarded);
        }
        Ok(Launch::Primary(Guard {}))
    }
    #[cfg(unix)]
    {
        unix::start(id, paths, inbox, wake)
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::{
        fs::{self, File, OpenOptions},
        io::{Read, Write},
        os::unix::{
            fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt},
            io::AsRawFd,
            net::{UnixListener, UnixStream},
        },
        thread,
        time::{Duration, Instant},
    };
    const MAX_FRAME: usize = 64 * 1024;
    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Request {
        version: u8,
        instance: String,
        paths: Vec<String>,
    }
    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Reply {
        version: u8,
        instance: String,
        queued: bool,
        error: Option<String>,
    }

    fn private_directory() -> Result<PathBuf> {
        let uid = unsafe { libc::geteuid() };
        // Finder, desktop launchers and shells may provide different TMPDIRs.
        // A per-user endpoint must stay stable across those launch environments.
        let directory = PathBuf::from("/tmp").join(format!("qisheng-{uid}"));
        match fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let metadata = fs::symlink_metadata(&directory)?;
        ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == uid
                && metadata.mode() & 0o077 == 0,
            "Launch IPC directory must be private and owned by the current user"
        );
        Ok(fs::canonicalize(directory)?)
    }
    fn peer_is_current_user(stream: &UnixStream) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            let mut peer = std::mem::MaybeUninit::<libc::ucred>::uninit();
            let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            ensure!(
                unsafe {
                    libc::getsockopt(
                        stream.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_PEERCRED,
                        peer.as_mut_ptr().cast(),
                        &mut length,
                    )
                } == 0,
                "Cannot inspect launch IPC peer"
            );
            ensure!(
                length as usize == std::mem::size_of::<libc::ucred>()
                    && unsafe { peer.assume_init().uid } == unsafe { libc::geteuid() },
                "Launch IPC peer belongs to another user"
            );
        }
        #[cfg(target_os = "macos")]
        {
            let mut uid = 0;
            let mut gid = 0;
            ensure!(
                unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0
                    && uid == unsafe { libc::geteuid() },
                "Launch IPC peer belongs to another user"
            );
        }
        Ok(())
    }
    fn read_exact_until(
        stream: &mut UnixStream,
        mut bytes: &mut [u8],
        deadline: Instant,
    ) -> Result<()> {
        while !bytes.is_empty() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .context("Launch IPC deadline expired")?;
            stream.set_read_timeout(Some(remaining.max(Duration::from_millis(1))))?;
            let count = stream.read(bytes)?;
            ensure!(count > 0, "Launch IPC frame is incomplete");
            bytes = &mut bytes[count..];
        }
        Ok(())
    }
    fn read_frame<T: serde::de::DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut header = [0u8; 4];
        read_exact_until(stream, &mut header, deadline)?;
        let length = u32::from_be_bytes(header) as usize;
        ensure!(
            length > 0 && length <= MAX_FRAME,
            "Launch IPC frame exceeds its bound"
        );
        let mut bytes = vec![0; length];
        read_exact_until(stream, &mut bytes, deadline)?;
        Ok(serde_json::from_slice(&bytes)?)
    }
    fn write_frame<T: serde::Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
        let bytes = serde_json::to_vec(value)?;
        ensure!(
            bytes.len() <= MAX_FRAME,
            "Launch IPC frame exceeds its bound"
        );
        stream.set_write_timeout(Some(Duration::from_secs(1)))?;
        stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
        stream.write_all(&bytes)?;
        Ok(())
    }
    fn forward(path: &Path, request: &Request) -> Result<bool> {
        let mut stream = UnixStream::connect(path)?;
        peer_is_current_user(&stream)?;
        write_frame(&mut stream, request)?;
        let reply: Reply = read_frame(&mut stream)?;
        ensure!(
            reply.version == 1 && reply.instance == request.instance,
            "Launch IPC endpoint has another identity"
        );
        ensure!(
            reply.queued,
            "Launch IPC rejected the request: {}",
            reply.error.as_deref().unwrap_or("No reason supplied")
        );
        Ok(reply.queued)
    }
    pub(super) fn start(
        id: &str,
        paths: &[PathBuf],
        inbox: Inbox,
        wake: UiInvalidationHandle,
    ) -> Result<Launch> {
        ensure!(
            id.len() <= 48 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "Invalid launch instance identifier"
        );
        ensure!(paths.len() <= 256, "Too many paths in one launch");
        let paths = paths
            .iter()
            .map(|p| -> Result<String> {
                let path = fs::canonicalize(p)?;
                let text = path.to_str().context("Launch filename is not UTF-8")?;
                ensure!(text.len() <= 16 * 1024, "Launch filename is too long");
                Ok(text.to_owned())
            })
            .collect::<Result<Vec<_>>>()?;
        let request = Request {
            version: 1,
            instance: id.into(),
            paths,
        };
        ensure!(
            serde_json::to_vec(&request)?.len() <= MAX_FRAME,
            "Launch request exceeds its bound"
        );
        let directory = private_directory()?;
        let endpoint = directory.join(format!("{id}.sock"));
        let lock_path = directory.join(format!("{id}.lock"));
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&lock_path)?;
        let metadata = lock.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.nlink() == 1
                && metadata.mode() & 0o077 == 0,
            "Launch lock is not a private, unique regular file"
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    let reason = match forward(&endpoint, &request) {
                        Ok(true) => return Ok(Launch::Forwarded),
                        Ok(false) => "The launch queue is closing".into(),
                        Err(error) => format!("{error:#}"),
                    };
                    ensure!(
                        Instant::now() < deadline,
                        "The existing client is still starting or stopping: {reason}"
                    );
                    thread::sleep(Duration::from_millis(50));
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        let selected_lock = fs::symlink_metadata(&lock_path)?;
        ensure!(
            selected_lock.dev() == metadata.dev() && selected_lock.ino() == metadata.ino(),
            "Launch lock namespace changed"
        );
        if let Ok(metadata) = fs::symlink_metadata(&endpoint) {
            ensure!(
                metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() },
                "Launch endpoint is not an owned socket"
            );
            match forward(&endpoint, &request) {
                Ok(true) => return Ok(Launch::Forwarded),
                Ok(false) => anyhow::bail!("Existing launch endpoint is closing"),
                Err(error)
                    if error.downcast_ref::<std::io::Error>().is_some_and(|e| {
                        matches!(
                            e.kind(),
                            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                        )
                    }) =>
                {
                    let current = fs::symlink_metadata(&endpoint)?;
                    ensure!(
                        current.ino() == metadata.ino() && current.dev() == metadata.dev(),
                        "Launch socket changed before stale cleanup"
                    );
                    fs::remove_file(&endpoint)?;
                }
                Err(error) => return Err(error),
            }
        }
        let listener = UnixListener::bind(&endpoint)?;
        let socket = fs::symlink_metadata(&endpoint)?;
        listener.set_nonblocking(true)?;
        let (signal, stopped) = UnixStream::pair()?;
        let closing = Arc::new(AtomicBool::new(false));
        let worker_closing = closing.clone();
        let mut guard = Server {
            endpoint,
            socket: (socket.dev(), socket.ino()),
            _lock: lock,
            closing,
            signal,
            thread: None,
        };
        let instance = id.to_string();
        guard.thread = Some(
            thread::Builder::new()
                .name("qisheng-launches".into())
                .spawn(move || {
                    while !worker_closing.load(Ordering::Acquire) {
                        let mut ready = [
                            libc::pollfd {
                                fd: listener.as_raw_fd(),
                                events: libc::POLLIN,
                                revents: 0,
                            },
                            libc::pollfd {
                                fd: stopped.as_raw_fd(),
                                events: libc::POLLIN,
                                revents: 0,
                            },
                        ];
                        let polled = unsafe { libc::poll(ready.as_mut_ptr(), 2, -1) };
                        if polled < 0 {
                            if std::io::Error::last_os_error().kind()
                                == std::io::ErrorKind::Interrupted
                            {
                                continue;
                            }
                            break;
                        }
                        if worker_closing.load(Ordering::Acquire) || ready[1].revents != 0 {
                            break;
                        }
                        let Ok((mut stream, _)) = listener.accept() else {
                            continue;
                        };
                        let accepted = (|| -> Result<()> {
                            stream.set_nonblocking(false)?;
                            peer_is_current_user(&stream)?;
                            let request: Request = read_frame(&mut stream)?;
                            ensure!(
                                request.version == 1
                                    && request.instance == instance
                                    && request.paths.len() <= 256,
                                "Invalid launch request"
                            );
                            ensure!(
                                request.paths.iter().all(|p| p.len() <= 16 * 1024
                                    && !p.contains('\0')
                                    && Path::new(p).is_absolute()),
                                "Launch requests require bounded absolute filenames"
                            );
                            inbox.enqueue(
                                request.paths.into_iter().map(PathBuf::from).collect(),
                                &wake,
                            )
                        })();
                        let _ = write_frame(
                            &mut stream,
                            &Reply {
                                version: 1,
                                instance: instance.clone(),
                                queued: accepted.is_ok(),
                                error: accepted
                                    .err()
                                    .map(|error| format!("{error:#}").chars().take(512).collect()),
                            },
                        );
                    }
                })?,
        );
        Ok(Launch::Primary(Guard { _server: guard }))
    }
    pub(super) struct Server {
        endpoint: PathBuf,
        socket: (u64, u64),
        _lock: File,
        closing: Arc<AtomicBool>,
        signal: UnixStream,
        thread: Option<thread::JoinHandle<()>>,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.closing.store(true, Ordering::Release);
            let _ = self.signal.write_all(&[1]);
            if let Some(worker) = self.thread.take() {
                let _ = worker.join();
            }
            if fs::symlink_metadata(&self.endpoint)
                .is_ok_and(|m| m.file_type().is_socket() && (m.dev(), m.ino()) == self.socket)
            {
                let _ = fs::remove_file(&self.endpoint);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn same_profile_forwards_bounded_unicode_paths_and_releases_its_listener() {
            let folder = tempfile::tempdir().unwrap();
            let id = id_for_directory(folder.path()).unwrap();
            let inbox = Inbox::default();
            let Launch::Primary(guard) =
                start(&id, &[], inbox.clone(), UiInvalidationHandle::new()).unwrap()
            else {
                panic!("Primary")
            };
            let path = folder.path().join("含 空格.wav");
            fs::write(&path, b"fixture").unwrap();
            assert!(matches!(
                start(
                    &id,
                    &[path.clone()],
                    Inbox::default(),
                    UiInvalidationHandle::new()
                )
                .unwrap(),
                Launch::Forwarded
            ));
            assert_eq!(inbox.take().unwrap(), vec![fs::canonicalize(path).unwrap()]);
            drop(guard);
            let Launch::Primary(next) =
                start(&id, &[], Inbox::default(), UiInvalidationHandle::new()).unwrap()
            else {
                panic!("Reacquire")
            };
            drop(next);
        }
        #[test]
        fn different_profiles_have_different_endpoints() {
            let first = tempfile::tempdir().unwrap();
            let second = tempfile::tempdir().unwrap();
            assert_ne!(
                id_for_directory(first.path()).unwrap(),
                id_for_directory(second.path()).unwrap()
            );
            assert_eq!(
                id_for_directory(first.path()).unwrap(),
                id_for_directory(first.path()).unwrap()
            );
        }
        #[test]
        fn oversized_wire_frame_is_rejected_before_allocating_a_payload() {
            let (mut sender, mut receiver) = UnixStream::pair().unwrap();
            sender
                .write_all(&((MAX_FRAME + 1) as u32).to_be_bytes())
                .unwrap();
            assert!(read_frame::<Request>(&mut receiver).is_err());
        }
    }
}
