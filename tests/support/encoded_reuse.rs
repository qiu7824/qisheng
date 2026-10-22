use lx_gpui_player::{
    audio::{
        cache::CacheOptions,
        transport::{
            ApprovedAudioRequest, AudioTransport, CacheIdentity, StreamLease, TransportOptions,
        },
    },
    domain::{
        media::ResolvedMediaKey,
        music::{LxMusicDtoVersion, TrackRecord},
    },
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub const BLOCK_SIZE: usize = 16 * 1024;
pub const HEADER_BYTES: u64 = 120;

pub fn directory() -> tempfile::TempDir {
    let base = std::env::temp_dir().join("lx-native-encoded-reuse-tests");
    fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("case-")
        .tempdir_in(base)
        .unwrap()
}

pub fn options(root: &Path) -> TransportOptions {
    TransportOptions {
        cache: CacheOptions {
            root: root.to_owned(),
            block_size: BLOCK_SIZE,
            ram_bytes: 64 * 1024,
            disk_bytes: 512 * 1024,
            max_objects: 16,
            max_blocks: 24,
        },
        connect_timeout: Duration::from_secs(2),
        header_timeout: Duration::from_secs(8),
        idle_timeout: Duration::from_secs(3),
        max_retries: 0,
        sequential_lead_blocks: 2,
        range_lookahead_blocks: 0,
    }
}

pub fn wave(seconds: f64, frequency: f64) -> Vec<u8> {
    let rate = 8_000_u32;
    let frames = (seconds * f64::from(rate)).round() as u32;
    let size = frames * 2;
    let mut bytes = Vec::new();
    bytes.extend(b"RIFF");
    bytes.extend((size + 36).to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(16_u32.to_le_bytes());
    bytes.extend(1_u16.to_le_bytes());
    bytes.extend(1_u16.to_le_bytes());
    bytes.extend(rate.to_le_bytes());
    bytes.extend((rate * 2).to_le_bytes());
    bytes.extend(2_u16.to_le_bytes());
    bytes.extend(16_u16.to_le_bytes());
    bytes.extend(b"data");
    bytes.extend(size.to_le_bytes());
    for frame in 0..frames {
        let sample = (std::f64::consts::TAU * frequency * f64::from(frame) / f64::from(rate)).sin();
        bytes.extend(((sample * 6_000.0) as i16).to_le_bytes());
    }
    bytes
}

pub fn record() -> TrackRecord {
    TrackRecord::from_lx_dto(
        serde_json::json!({
            "source":"kw", "songmid":"original-encoded", "name":"Original reusable tone",
            "singer":"Fixture composer", "albumName":"Original recordings", "interval":"00:02",
            "types":[{"type":"128k"},{"type":"320k"}], "_types":{"128k":{},"320k":{}},
        }),
        LxMusicDtoVersion::Legacy,
    )
    .unwrap()
}

#[derive(Clone)]
pub struct Context {
    pub record: TrackRecord,
    pub source_sha256: String,
    pub policy: u64,
    pub quality: String,
}
impl Default for Context {
    fn default() -> Self {
        Self {
            record: record(),
            source_sha256: digest(b"Original approved encoded source"),
            policy: 1,
            quality: "128k".into(),
        }
    }
}
impl Context {
    pub fn identity(&self, approval: u64) -> CacheIdentity {
        let key = ResolvedMediaKey::new(
            &self.record,
            &self.quality,
            &self.source_sha256,
            self.policy,
            &self.record.to_source_music_info(),
        )
        .unwrap();
        CacheIdentity {
            track_id: self.record.id.clone(),
            platform: self.record.platform.source.clone(),
            actual_quality: self.quality.clone(),
            source_sha256: self.source_sha256.clone(),
            source_policy_version: self.policy,
            media_context_sha256: key.fingerprint().unwrap(),
            source_epoch: 100 + approval,
            approval_id: format!("fresh-exact-url-approval-{approval}"),
            approval_epoch: approval,
            ..Default::default()
        }
    }
    pub fn request(&self, url: &str, approval: u64) -> ApprovedAudioRequest {
        ApprovedAudioRequest::new(url, self.identity(approval)).unwrap()
    }
}
pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Clone, Debug)]
pub struct HttpRecord {
    pub target: String,
    pub range: Option<(u64, Option<u64>)>,
    pub if_range: Option<String>,
    pub status: u16,
    pub body_bytes: usize,
}
#[derive(Clone)]
pub struct Representation {
    pub body: Arc<Vec<u8>>,
    pub etag: Option<String>,
    pub status: u16,
    pub ignore_range: bool,
}
impl Default for Representation {
    fn default() -> Self {
        Self {
            body: Arc::new(wave(2.0, 440.0)),
            etag: Some("\"original-v1\"".into()),
            status: 200,
            ignore_range: false,
        }
    }
}
pub struct Server {
    pub url: String,
    pub representation: Arc<Mutex<Representation>>,
    log: Arc<Mutex<Vec<HttpRecord>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Server {
    pub fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/original.wav", listener.local_addr().unwrap());
        let representation = Arc::new(Mutex::new(Representation::default()));
        let log = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_view = representation.clone();
        let worker_log = log.clone();
        let stopping = stop.clone();
        let thread = thread::spawn(move || {
            let mut clients = Vec::new();
            while !stopping.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        let view = worker_view.clone();
                        let log = worker_log.clone();
                        let stop = stopping.clone();
                        clients.push(thread::spawn(move || serve(socket, view, log, stop)));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("original HTTP accept: {error}"),
                }
            }
            for client in clients {
                client.join().unwrap();
            }
        });
        Self {
            url,
            representation,
            log,
            stop,
            thread: Some(thread),
        }
    }
    pub fn records(&self) -> Vec<HttpRecord> {
        self.log.lock().unwrap().clone()
    }
    pub fn count(&self) -> usize {
        self.log.lock().unwrap().len()
    }
    pub fn bytes_since(&self, cursor: usize) -> usize {
        self.log.lock().unwrap()[cursor..]
            .iter()
            .map(|item| item.body_bytes)
            .sum()
    }
    pub fn wait_records(&self, minimum: usize) {
        let started = Instant::now();
        while self.count() < minimum {
            assert!(started.elapsed() < Duration::from_secs(8));
            thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let result = thread.join();
            if !thread::panicking() {
                assert!(result.is_ok(), "original HTTP fixture failed");
            }
        }
    }
}

fn serve(
    mut socket: TcpStream,
    view: Arc<Mutex<Representation>>,
    log: Arc<Mutex<Vec<HttpRecord>>>,
    stop: Arc<AtomicBool>,
) {
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let started = Instant::now();
    let mut request = Vec::new();
    let mut scratch = [0; 1024];
    while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
        if stop.load(Ordering::Acquire) {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(12),
            "real HTTP header deadline after {} bytes",
            request.len()
        );
        match socket.read(&mut scratch) {
            Ok(0) => return,
            Ok(count) => request.extend_from_slice(&scratch[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(_) => return,
        }
        assert!(request.len() <= 64 * 1024);
    }
    let request = String::from_utf8(request).unwrap();
    let target = request
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned();
    let value = |name: &str| {
        request
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim().to_owned())
    };
    let range = value("range").map(|text| {
        let (start, end) = text
            .strip_prefix("bytes=")
            .unwrap()
            .split_once('-')
            .unwrap();
        (start.parse::<u64>().unwrap(), end.parse::<u64>().ok())
    });
    let if_range = value("if-range");
    let current = view.lock().unwrap().clone();
    let accept_range = !current.ignore_range
        && range.is_some()
        && if_range
            .as_ref()
            .is_none_or(|tag| current.etag.as_ref() == Some(tag) && !tag.starts_with("W/"));
    let (status, start, end) = if current.status >= 400 {
        (current.status, 0, 0)
    } else if accept_range && range.unwrap().0 >= current.body.len() as u64 {
        (416, 0, 0)
    } else if accept_range {
        let (start, end) = range.unwrap();
        (
            206,
            start as usize,
            end.unwrap_or(current.body.len() as u64 - 1)
                .min(current.body.len() as u64 - 1) as usize,
        )
    } else {
        (200, 0, current.body.len() - 1)
    };
    let length = if status >= 400 { 0 } else { end - start + 1 };
    let mut header = format!(
        "HTTP/1.1 {status} Original\r\nContent-Type: audio/wav\r\nContent-Length: {length}\r\nConnection: close\r\n"
    );
    if let Some(tag) = &current.etag {
        header.push_str(&format!("ETag: {tag}\r\n"));
    }
    if status == 206 {
        header.push_str(&format!(
            "Content-Range: bytes {start}-{end}/{}\r\n",
            current.body.len()
        ));
    }
    if status == 416 {
        header.push_str(&format!(
            "Content-Range: bytes */{}\r\n",
            current.body.len()
        ));
    }
    header.push_str("\r\n");
    let mut sent = 0;
    if socket.write_all(header.as_bytes()).is_ok() {
        while sent < length {
            match socket.write(&current.body[start + sent..=end]) {
                Ok(0) | Err(_) => break,
                Ok(count) => sent += count,
            }
        }
    }
    log.lock().unwrap().push(HttpRecord {
        target,
        range,
        if_range,
        status,
        body_bytes: sent,
    });
}

pub fn open(
    transport: &Arc<AudioTransport>,
    context: &Context,
    url: &str,
    approval: u64,
) -> StreamLease {
    transport
        .open(
            context.request(url, approval),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap()
}
pub fn read_all(
    transport: &Arc<AudioTransport>,
    context: &Context,
    url: &str,
    approval: u64,
) -> Vec<u8> {
    let mut lease = open(transport, context, url, approval);
    let mut actual = Vec::new();
    lease.reader.read_to_end(&mut actual).unwrap();
    lease.control.cancel();
    assert!(lease.control.join(Duration::from_secs(5)));
    drop(lease);
    actual
}

pub fn files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut result = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let metadata = fs::symlink_metadata(entry.path()).unwrap();
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            assert_eq!(metadata.file_attributes() & 0x400, 0);
        }
        assert!(!metadata.file_type().is_symlink());
        if metadata.is_dir() {
            result.extend(files(&entry.path()));
        } else {
            assert!(metadata.is_file());
            result.push(entry.path());
        }
    }
    result.sort();
    result
}
pub fn blocks(root: &Path) -> Vec<std::path::PathBuf> {
    files(root)
        .into_iter()
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("block-")
        })
        .collect()
}
pub fn physical_bytes(root: &Path) -> u64 {
    files(root)
        .iter()
        .map(|path| fs::metadata(path).unwrap().len())
        .sum()
}
